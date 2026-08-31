//! The `jqf serve` route: a resident daemon — one compiled program, NDJSON in / NDJSON out per connection, a socket
//! instead of a pipe.
//!
//! # The session protocol
//!
//! A connection is ONE record stream, exactly like `--follow`'s pipe but with a per-connection session: the client
//! pushes NDJSON frames, the daemon publishes NDJSON out (one record per line, the codec's own terminator). Each
//! completed byte range is driven by the SAME `execute_record_request` the whole-input record route and the follow
//! cycles use — same framer, same payload ladder, same planner — so the per-record laws carry over unchanged:
//!
//! - **poison isolation per message** — a per-value error is reported on the
//!   daemon's stderr and the session continues;
//! - **the exact cut** — a record is complete only after its physical
//!   terminator; the tail is held until more bytes arrive;
//! - **cycle compaction** — the completed range drains after every drive, so
//!   a long session's memory stays bounded by the current partial record plus one cycle, never by the stream;
//! - **recovering finalization** — a truncated tail at connection EOF is
//!   finalized through `ndjson.recovering@1`'s own law (an ordered error issue, never a crash).
//!
//! The dialect is recovering BY CONSTRUCTION (like `--follow`'s live tail): a half-written message over a socket is the
//! normal state of the world, not a fault. A strict-profile knob is a next-pass item, decided when the feed freezes its
//! profile selection.
//!
//! # The diagnostic side channel (decided at design time, 2026-08-06)
//!
//! Session diagnostics ride the daemon's STDERR, never the connection's NDJSON out. Interleaving would corrupt the
//! client's framing — the record contract is byte-clean NDJSON, one record per line — and the feed model publishes
//! diagnostics on a stream SEPARATE from the output buffer; the socket's spelling of that separation is the daemon's
//! stderr (the conventional server log). Per-value errors and record issues render in the CLI's own shapes (`jqf: error
//! (at conn#N:line)` / `jqf: record …`), re-based to the connection's absolute line/offset, and the retained `jqf:
//! diag` NDJSON records print under `--diagnostics`. A socket client and an FFI host therefore see ONE contract: output
//! records and diagnostics records on two channels, never mixed — the surface the record contract freezes.
//!
//! # Client-visible error frames (, decided 2026-08-21)
//!
//! The stderr side channel alone left a request-reply client unable to distinguish "error" from "no answer": a poison
//! record produced NO reply line. Every ERROR-severity event therefore ALSO publishes one error frame on the connection
//! itself, in ordinal order relative to the output records, as a single NDJSON line:
//!
//! - a record issue: `{"jqf:error":{"kind":"record-issue","code":"<code>",
//!   "message":"<detail>","record":N,"offset":O}}` (`N`/`O` re-based to the connection's absolute record/byte position;
//!   `code`/`message` from the framing codec that raised it, the same text the stderr line renders);
//! - a per-value runtime error: `{"jqf:error":{"kind":"value-error",
//!   "message":"<text>","line":L}}` (`L` the connection's absolute input line).
//!
//! ADVISORY issues publish no frame — they are informational (a complete final record without its terminator), and a
//! client that asked for records must not receive chatter for them; the stderr side channel carries them. The stderr
//! renderings are UNCHANGED: the frame is a second rendering of the same event, not a replacement.
//!
//! Recorded narrowing: a program that itself emits an object whose only key is `jqf:error` with this exact inner shape
//! is indistinguishable from an error frame. A second channel (a side socket) and a line-prefix sentinel were rejected
//! — the first doubles the protocol's transport surface, the second breaks byte-clean NDJSON framing for every existing
//! client.
//!
//! # The concurrency model
//!
//! Thread-per-connection : every accepted connection is served on its own thread, so one idle client cannot queue the
//! others behind it. The daemon's WARM state (the compiled program) is shared read-only — the parallel morsel machinery
//! already runs one program graph across worker threads, so the graph is thread-safe to read — but the
//! `ResourceContext` is request-local (NOT `Send`, the worker-grants law), so each session builds its OWN account and
//! context inside its thread, exactly as a fresh run would: same limits shape, same governor, same environment
//! snapshot. The per-session cost of that is the ledger allocation, not a recompile.
//!
//! Session threads reserve the SAME stack the request thread reserves (`JQF_REQUEST_STACK_BYTES`'s default 256 MiB)
//! because a session drive can reach the same deep recursions a run can — but a stack RESERVATION is virtual address
//! space; resident pages are only what the drive touches, so N connections do NOT cost N resident megabytes.
//!
//! Concurrency is capped at `MAX_CONCURRENT_SESSIONS` live sessions by a counting semaphore around the accept loop: at
//! the cap the daemon stops accepting (the listen backlog holds further clients) until a session ends. Each session's
//! end still releases its freed pages back toward the OS ceiling (`release_freed_pages`), now on the session's own
//! thread — the no-compounding law of the serial daemon, kept.
//!
//! Both halves of a connection carry the SAME idle window (`--read-timeout` governs writes too): `SessionReader` polls
//! POLLIN before each read, and its twin `SessionWriter` polls POLLOUT before each write. A client that stops reading
//! therefore cannot wedge one session writer past the deadline (SIGPIPE is ignored by design, so an ungated write would
//! block forever): on expiry THAT session ends cleanly — the drive unwinds without further writes, the diagnostic on
//! the daemon's stderr names the stall and the elapsed time, and the concurrency slot releases. Neither the accept loop
//! nor any other session is touched; zero disables both windows.
//!
//! # The exit law
//!
//! The daemon runs until a signal: SIGINT/SIGTERM keep their default dispositions, so the process dies with the
//! signal's own status (no handler is installed). SIGPIPE — which `main` restored to `SIG_DFL` so `jqf … | head` dies
//! as quietly as adopted — is re-ignored HERE: a daemon must not die when a client disconnects mid-write, so a write to
//! a closed connection becomes EPIPE and ends the session, not the daemon. Per-connection errors never kill the daemon;
//! they are diagnostics on its stderr. The per- connection "exit class" is therefore carried as diagnostics in v1 (a
//! socket session has no process exit; the 044 feed's per-poll outcome record is the freeze point).

#[cfg(unix)]
use std::fs::{File, OpenOptions};
use std::io::{self, Read as IoRead, Write as IoWrite};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::fd::AsRawFd as _;

use jqf_codec_core::{DiagnosticPolicy, RecordIssueSeverity, ValidationMode};
use jqf_codec_json::JsonEncodeOptions;
use jqf_codec_json::ndjson::{NdjsonProfile, NdjsonTerminator};
use jqf_data::{ObjectBuilder, ObjectKey, Value};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, CompiledProgram, try_compile_program};
use jqf_resource::{RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_runtime::records::{
    OutputTarget, RecordDriveError, RecordDriveSpec, RecordInputKind, RecordOutputSpec, RecordRunModel, WorkerRequest,
    execute_record_request,
};
use jqf_sdk::{EncodedItemReport, ItemSink, RecordIssueReport, SequenceValueError};

use crate::args::{CliParallelSelection, ServeArguments};
use crate::errors::{CliFailure, ExitClass, compile_failure, resource_note};
use crate::loader::{CliModuleLoader, host_environment};
use crate::output::CliStderr;
use crate::plan::COOPERATIVE_CREDITS;
use crate::routes::record::{count_newlines, plan_record_request, render_record_drive_failure};
use crate::{eprint_line_buffered, eprint_value_error_at, flush_stderr};

/// The per-connection read chunk, and the compaction law's practical bound: a session holds at most one chunk plus the
/// held partial tail, never the whole stream (the follow memory law, carried over).
const READ_CHUNK: usize = 64 * 1024;

/// The cap on concurrently LIVE sessions : at the cap the accept loop blocks and the listen backlog holds further
/// clients until a session ends. Each live session reserves the request stack VIRTUALLY; resident memory stays bounded
/// by what the drives actually touch.
const MAX_CONCURRENT_SESSIONS: usize = 64;

/// A counting semaphore bounding live sessions: `acquire` blocks while all slots are held; `release` wakes one waiter.
/// The accept loop holds one slot for the whole lifetime of each spawned session thread.
struct SessionSlots {
    state: Mutex<usize>,
    released: Condvar,
}

impl SessionSlots {
    fn new(max: usize) -> Self {
        debug_assert!(max > 0);
        Self {
            state: Mutex::new(max),
            released: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut free = self.state.lock().expect("session slots lock");
        while *free == 0 {
            free = self.released.wait(free).expect("session slots wait");
        }
        *free -= 1;
    }

    fn release(&self) {
        let mut free = self.state.lock().expect("session slots lock");
        *free += 1;
        drop(free);
        self.released.notify_one();
    }
}

/// Restores one concurrency slot when the session thread's closure ENDS — by completing or by unwinding. A trailing
/// `release` statement is skipped when a panic unwinds past it, and each skipped release permanently consumes a slot; a
/// Drop guard is unwind-safe.
struct SessionSlotGuard {
    slots: Arc<SessionSlots>,
}

impl Drop for SessionSlotGuard {
    fn drop(&mut self) {
        self.slots.release();
    }
}

/// One `--listen` target, as bound.
enum Listener {
    #[cfg(unix)]
    Unix {
        listener: UnixListener,
        /// Exclusive lock on `{socket}.lock`, held for the listener lifetime so a second serve cannot unlink a live
        /// daemon when connect fails (full backlog, ECONNREFUSED). Dropping the guard removes the zero-byte lock file
        /// WHILE the flock is still held, so the file survives only a signal death (which runs no destructors).
        _lock: UnixLockGuard,
    },
    Tcp(TcpListener),
}

/// Resolves one `--listen` spelling: a target whose part after the LAST colon parses as a port number is TCP
/// (`host:port`, an empty host binding the loopback default); everything else is a unix-socket path. The rule is the
/// one spelled in the help; a unix path containing a colon is deliberately not a TCP spelling unless its tail is a
/// number.
fn bind_listener(spec: &str) -> Result<Listener, CliFailure> {
    if let Some((host, port)) = spec.rsplit_once(':') {
        match port.parse::<u16>() {
            // An empty host binds the loopback default; any other host is bound verbatim (`0.0.0.0`, `:`, a name, …).
            Ok(port) if port != 0 => {
                let address = if host.is_empty() {
                    format!("127.0.0.1:{port}")
                } else if host.contains(':') && !host.starts_with('[') {
                    // Bare IPv6 (`:1:8080`): wrap the host so the socket address parser sees one address, not an extra
                    // colon.
                    format!("[{host}]:{port}")
                } else {
                    format!("{host}:{port}")
                };
                let listener = TcpListener::bind(&address)
                    .map_err(|error| CliFailure::from(format!("cannot bind {spec}: {error}")))?;
                return Ok(Listener::Tcp(listener));
            }
            Ok(_) => return Err("--listen port must be between 1 and 65535".into()),
            Err(_) => {}
        }
    }
    #[cfg(unix)]
    {
        let (listener, lock) = bind_unix(std::path::Path::new(spec))
            .map_err(|error| CliFailure::from(format!("cannot bind unix socket {spec}: {error}")))?;
        Ok(Listener::Unix { listener, _lock: lock })
    }
    #[cfg(not(unix))]
    {
        Err("a unix-socket --listen target requires a Unix host".into())
    }
}

/// The `{socket}.lock` file's lifetime guard: holds the exclusive flock and unlinks the file on drop, BEFORE the lock's
/// own descriptor closes (field drops run after `Drop:drop`), so a normal daemon exit leaves no zero-byte residue.
/// Best-effort: an unlink failure leaves the file for the next serve to reuse.
#[cfg(unix)]
struct UnixLockGuard {
    _lock: File,
    lock_path: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for UnixLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.lock_path);
    }
}

/// Binds a unix listener, recovering a STALE socket from a dead daemon.
///
/// An exclusive flock on `{path}.lock` is held for the listener lifetime. A live daemon holds that lock, so a second
/// serve refuses even when connect would return ECONNREFUSED (full backlog). A crash leftover releases the lock, so the
/// next serve can unlink the socket and bind.
#[cfg(unix)]
fn bind_unix(path: &std::path::Path) -> io::Result<(UnixListener, UnixLockGuard)> {
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = std::path::PathBuf::from(lock_path);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    lock.try_lock()
        .map_err(|_| io::Error::new(io::ErrorKind::AddrInUse, "address already in use"))?;
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        // The lock is this process's but the socket could not be bound: release the flock and remove the file so the
        // next serve starts clean.
        Err(error) => {
            drop(UnixLockGuard { _lock: lock, lock_path });
            return Err(error);
        }
    };
    Ok((listener, UnixLockGuard { _lock: lock, lock_path }))
}

/// One connection's transport: a unix stream or a TCP stream. Both are full duplex; the session holds one clone per
/// half, each behind its own idle-window reader or writer.
enum SessionStream {
    #[cfg(unix)]
    Unix(UnixStream),
    Tcp(std::net::TcpStream),
}

impl SessionStream {
    /// Clones the handle so the session can hold one for reading and one for writing (both transports are full duplex,
    /// so the halves are independent).
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => Ok(Self::Unix(stream.try_clone()?)),
            Self::Tcp(stream) => Ok(Self::Tcp(stream.try_clone()?)),
        }
    }

    /// Applies the per-connection read/idle timeout. The timeout is enforced by `SessionReader`'s poll-before-read
    /// loop, not by a socket option: macOS rejects `SO_RCVTIMEO` on `AF_UNIX` sockets with `EINVAL`, which used to
    /// surface as `cannot arm the read timeout` on every connection, and a poll-based window works identically on both
    /// transports. `None` (or zero) disables it; a timed-out read surfaces as an `io:Error` in the session drive, which
    /// reports the session failure and lets the daemon accept the next connection — a dribbling client must not hold
    /// the accept loop.
    fn read_half(&self, timeout: Option<Duration>) -> io::Result<SessionReader> {
        Ok(SessionReader {
            stream: self.try_clone()?,
            deadline_reset: timeout.filter(|timeout| !timeout.is_zero()),
        })
    }

    /// The write half's twin constructor: the same window arms the poll-before-write loop (`--read-timeout` governs
    /// writes too). `None` (or zero) writes blocking.
    fn write_half(&self, timeout: Option<Duration>) -> io::Result<SessionWriter> {
        Ok(SessionWriter {
            stream: self.try_clone()?,
            deadline: timeout.filter(|timeout| !timeout.is_zero()),
            expired_after: None,
        })
    }
}

/// The reading half of one connection, with the per-read idle window.
///
/// Each `read` first polls the descriptor for readability over the remaining window and only then issues the blocking
/// read (which therefore cannot block past data arriving); an expired window returns `WouldBlock`, the same shape the
/// socket-option timeout produced, so the session drive's existing idle handling is unchanged.
struct SessionReader {
    stream: SessionStream,
    /// The per-read idle window; `None` reads blocking.
    deadline_reset: Option<Duration>,
}

impl IoRead for SessionReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let Some(window) = self.deadline_reset else {
            return self.stream.read(buffer);
        };
        // The poll-before-read loop is the unix idle window (libc:poll; macOS rejects SO_RCVTIMEO on AF_UNIX,). Windows
        // has no libc in this tree and its only transport is TCP: sessions there read blocking without idle enforcement
        // — a recorded unix-only narrowing, the Tier-2 shape (works where Rust works).
        #[cfg(unix)]
        {
            let fd = self.stream.as_raw_fd();
            let deadline = Instant::now() + window;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let milliseconds = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
                let ready = unsafe { libc::poll(std::ptr::from_mut(&mut poll_fd).cast(), 1, milliseconds) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if ready == 0 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                return self.stream.read(buffer);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = window;
            self.stream.read(buffer)
        }
    }
}

/// The writing half of one connection — `SessionReader`'s twin.
///
/// Each `write` first polls the descriptor for writability over the remaining window and only then issues a
/// NON-BLOCKING write attempt; a spurious readiness or a zero-space attempt retries within the same window. The attempt
/// must not use the descriptor's blocking mode: poll's writability promises room for SOME bytes, and a blocking stream
/// write commits to the caller's WHOLE slice — a 64 KiB flush against a nearly-full buffer would sleep inside the
/// syscall until the peer reads, past any deadline (the recorded stall limitation, observed live: exactly one
/// send-buffer's worth lands, then silence). Non-blocking mode makes partial writes ordinary, so every step of progress
/// re-enters the gate.
///
/// An expired window returns `WouldBlock` carrying the stall wording (so the wording survives every rendering path,
/// including the record drive's sink-error arm) and records the elapsed time for the session's dedicated diagnostic. A
/// client that stops reading therefore ends ITS OWN session within one window instead of wedging a writer until it
/// resumes.
struct SessionWriter {
    stream: SessionStream,
    /// The per-write idle window; `None` writes blocking.
    deadline: Option<Duration>,
    /// Elapsed time at the most recent deadline expiry; reset at every write attempt, so on any observed error it
    /// describes THAT error.
    expired_after: Option<Duration>,
}

/// One gated write attempt's outcome: progress (possibly partial), no room left in the window's budget, or a real
/// failure.
#[cfg(unix)]
enum WriteAttempt {
    Progressed(usize),
    Blocked,
    Failed(io::Error),
}

impl SessionWriter {
    /// Issues one non-blocking write attempt over the session's own descriptor. `O_NONBLOCK` is an
    /// open-file-description flag shared with this connection's other halves, but a session drives its halves
    /// sequentially on one thread, so the flag is private for the duration of the attempt and restored before every
    /// return.
    #[cfg(unix)]
    fn attempt_write(&mut self, bytes: &[u8]) -> WriteAttempt {
        let fd = self.stream.as_raw_fd();
        // Safety: `fcntl` on the session's own descriptor, and the previous
        // flag word is restored on every path below.
        let previous = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if previous < 0 {
            return WriteAttempt::Failed(io::Error::last_os_error());
        }
        unsafe {
            if libc::fcntl(fd, libc::F_SETFL, previous | libc::O_NONBLOCK) < 0 {
                return WriteAttempt::Failed(io::Error::last_os_error());
            }
            let outcome = self.stream.write(bytes);
            // Restore BEFORE interpreting the outcome: the flag must not leak into the reader half however the attempt
            // ended.
            let restore_failed = libc::fcntl(fd, libc::F_SETFL, previous) < 0;
            if restore_failed {
                return WriteAttempt::Failed(io::Error::last_os_error());
            }
            match outcome {
                // No progress: the poll loop decides whether the budget allows another attempt.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => WriteAttempt::Blocked,
                other => match other {
                    Ok(written) => WriteAttempt::Progressed(written),
                    Err(error) => WriteAttempt::Failed(error),
                },
            }
        }
    }
}

impl IoWrite for SessionWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.expired_after = None;
        let Some(window) = self.deadline else {
            return self.stream.write(bytes);
        };
        // The poll-before-write loop is the unix idle window's write half. Windows has no libc in this tree and its
        // only transport is TCP: sessions there write blocking without idle enforcement — the same recorded unix-only
        // narrowing as the read half, the Tier-2 shape (works where Rust works).
        #[cfg(unix)]
        {
            let started = Instant::now();
            let fd = self.stream.as_raw_fd();
            loop {
                let remaining = (started + window).saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    let elapsed = started.elapsed();
                    self.expired_after = Some(elapsed);
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        format!(
                            "the peer stopped accepting bytes; the write deadline \
                             ({:.3}s) expired after {:.3}s",
                            window.as_secs_f64(),
                            elapsed.as_secs_f64()
                        ),
                    ));
                }
                let mut poll_fd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let milliseconds = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
                let ready = unsafe { libc::poll(std::ptr::from_mut(&mut poll_fd).cast(), 1, milliseconds) };
                if ready < 0 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if ready == 0 {
                    // The poll budget is spent; the loop head reports the expiry.
                    continue;
                }
                match self.attempt_write(bytes) {
                    WriteAttempt::Progressed(written) => return Ok(written),
                    WriteAttempt::Failed(error) => return Err(error),
                    // No room this attempt: back around for the expiry check.
                    WriteAttempt::Blocked => {}
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = window;
            self.stream.write(bytes)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for SessionStream {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.as_raw_fd(),
            Self::Tcp(stream) => stream.as_raw_fd(),
        }
    }
}

impl IoRead for SessionStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl IoWrite for SessionStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(bytes),
            Self::Tcp(stream) => stream.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

/// The per-connection sink: publishes item bytes to the connection and renders diagnostics on the daemon's stderr in
/// the CLI's own shapes, re-based to the connection's absolute line/offset and named `conn#N`.
///
/// A socket session has no `-e` exit-status fact and no process exit; the item reports are observed only for the sink
/// contract, never for a verdict.
struct SessionSink {
    /// The buffered connection writer (`SessionWriter`'s idle window gates every socket write); flushed at every cycle
    /// boundary so a client sees each batch as it lands.
    out: io::BufWriter<SessionWriter>,
    /// The connection's diagnostic label (`conn#N`).
    label: String,
    /// Newlines before the current cycle's first byte: its absolute line base within the connection.
    base_line: u64,
    /// Byte offset before the current cycle's first byte.
    base_offset: u64,
    /// Physical records before the current cycle's first byte.
    base_record: u64,
}

impl ItemSink for SessionSink {
    type Error = io::Error;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.out.write(bytes)
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }

    fn report_record_issue(&mut self, issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        // The shared rendering law, re-based to the connection's absolute offset and spelled by NDJSON (the session's
        // only framing codec); the session never re-renders the issue shape.
        crate::output::render_record_issue(
            &issue,
            self.base_offset,
            self.base_record,
            jqf_codec_json::ndjson::issue_text,
        );
        //: an ERROR-severity issue also publishes the documented
        // client-visible error frame, in ordinal order on the connection itself — a request-reply client must be able
        // to distinguish "error" from "no answer". Advisories stay stderr-only.
        if issue.severity() == RecordIssueSeverity::Error {
            let (code, message) = jqf_codec_json::ndjson::issue_text(issue.code());
            let detail = issue
                .cause()
                .and_then(crate::errors::render_codec_diagnostic)
                .unwrap_or_else(|| message.to_owned());
            let mut frame = Vec::with_capacity(128);
            frame.extend_from_slice(b"{\"jqf:error\":{\"kind\":\"record-issue\",\"code\":");
            write_json_string(&mut frame, code);
            frame.extend_from_slice(b",\"message\":");
            write_json_string(&mut frame, &detail);
            frame.extend_from_slice(b",\"record\":");
            frame.extend_from_slice(issue.ordinal().saturating_add(self.base_record).to_string().as_bytes());
            frame.extend_from_slice(b",\"offset\":");
            frame.extend_from_slice(issue.offset().saturating_add(self.base_offset).to_string().as_bytes());
            frame.extend_from_slice(b"}}\n");
            self.out.write_all(&frame)?;
        }
        Ok(())
    }

    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        // Same law as the follow route: the cycle's line count starts at one, so the facade adds the newlines the
        // connection had already consumed before the cycle began, and names the connection, not `<stdin>`.
        let absolute_line = error.input_line().saturating_add(self.base_line);
        eprint_value_error_at(&self.label, absolute_line, error.frame_note(), error.message());
        //: the client-visible twin of the stderr line.
        let mut frame = Vec::with_capacity(96);
        frame.extend_from_slice(b"{\"jqf:error\":{\"kind\":\"value-error\",\"message\":");
        write_json_string(&mut frame, error.message());
        frame.extend_from_slice(b",\"line\":");
        frame.extend_from_slice(absolute_line.to_string().as_bytes());
        frame.extend_from_slice(b"}}\n");
        self.out.write_all(&frame)?;
        Ok(())
    }
}

/// Writes `text` as one JSON string literal (quotes included) into `out`: quotes, backslashes and C0 controls escaped,
/// everything else copied verbatim. The error-frame texts are diagnostic renderings, so this only needs to be correct,
/// never fast.
fn write_json_string(out: &mut Vec<u8>, text: &str) {
    out.push(b'"');
    for character in text.chars() {
        match character {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            control if (control as u32) < 0x20 => {
                out.extend_from_slice(format!("\\u{:04x}", control as u32).as_bytes());
            }
            other => out.extend_from_slice(other.encode_utf8(&mut [0u8; 4]).as_bytes()),
        }
    }
    out.push(b'"');
}

/// Drives one session cycle's completed record range through the record route.
///
/// The whole-input record route over the cycle's bytes, exactly as the follow route drives its cycles: the recovering
/// profile, the per-record model, NDJSON out, the same planner. A per-value error is reported to the daemon's stderr
/// and the session goes on (poison isolation); a terminal failure propagates and ends the session, never the daemon.
fn drive_cycle(
    compiled: &CompiledProgram,
    resources: &mut ResourceContext<'_>,
    sink: &mut SessionSink,
    batch: &[u8],
    diagnostics: bool,
) -> Result<(), CliFailure> {
    let source_label = sink.label.clone();
    let spec = RecordDriveSpec {
        input: batch,
        source_name: &source_label,
        files: None,
        kind: RecordInputKind::Ndjson,
        profile: NdjsonProfile::Recovering,
        json_seq_profile: jqf_codec_json::seq::JsonSeqProfile::Strict,
        csv_delimiter: None,
        csv_textdata: false,
        // No per-record ceiling: the session's own retained buffer physically bounds a record, exactly as the
        // whole-input route's unlimited input ceiling does.
        max_record_bytes: u64::MAX,
        catalog: crate::routes::record_catalog(),
        output: RecordOutputSpec {
            target: OutputTarget::Ndjson,
            terminator: NdjsonTerminator::Lf,
            json: JsonEncodeOptions {
                indent: crate::args::DEFAULT_INDENT,
                raw_strings: false,
                sort_keys: false,
                ascii_output: false,
                raw_output_nul: false,
            },
            no_newline: false,
        },
        model: RecordRunModel::PerRecord,
        edit: false,
        cooperative_credits: COOPERATIVE_CREDITS,
        // The serve subcommand's argument surface carries no iteration dial (`--max-iterations` is a usage error after
        // `serve`), so the cycle's ceiling is the uncapped default.
        max_iterations: None,
    };
    let plan = plan_record_request(
        CliParallelSelection {
            enabled: true,
            workers: WorkerRequest::Auto,
        },
        // A socket session has no exit code to report; the last-value law is diagnostics on the side channel, never a
        // process exit. It has no colour either: a socket is never a terminal, and the daemon's protocol must stay
        // byte-clean.
        false,
        false,
        // The daemon serves no `--edit` session.
        false,
        // A socket session has no `--unbuffered` flag; the sink flushes per morsel by construction.
        false,
        batch.len() as u64,
        spec,
        compiled,
        resources.mismatch_policy() != jqf_resource::policy::MismatchPolicy::Lenient,
        //: the serve daemon's record drive declines the non-default
        // strictness dial exactly as the CLI route does — a worker's warnings and promotion have no session to surface
        // into.
        resources.strictness() != jqf_resource::policy::StrictnessPolicy::Error,
        // The daemon has no `--split-exp` surface (a socket session cannot own per-item files); the split destination
        // is CLI-only.
        false,
        // A serve session carries no `--arg`/`-L` bindings.
        false,
    );
    if diagnostics {
        eprint_line_buffered(&format!("jqf: {plan}"));
        flush_stderr();
    }
    match execute_record_request(spec, plan, compiled, resources, sink, None) {
        Ok(report) => {
            if diagnostics {
                eprint_line_buffered(&format!("jqf: {report}"));
            }
            Ok(())
        }
        Err(RecordDriveError::Pipeline(jqf_sdk::Failure::Pipeline(ref pipeline)))
            if jqf_runtime::feed::is_per_value_failure(pipeline.failure()) =>
        {
            // The failure was already reported to the daemon's stderr as the sequence continued past it; the session
            // goes on.
            Ok(())
        }
        Err(error) => Err(render_record_drive_failure(error)),
    }?;
    // The diagnostics channel is flushed at every cycle boundary, exactly as the follow route flushes: a daemon dies by
    // signal, so `main`'s exit-time flush never runs, and a session's issue must be visible on the side channel before
    // the next cycle — the whole point of a server log.
    flush_stderr();
    Ok(())
}

/// The session-end diagnostic for an expired write deadline: names the stall and the elapsed time. The session ends
/// cleanly — no further writes, no retry loop — and neither the accept loop nor any other session is touched.
fn write_stall_failure(label: &str, elapsed: Duration) -> CliFailure {
    CliFailure::from(format!(
        "{label}: write stalled: no socket progress for {:.3}s; session ended",
        elapsed.as_secs_f64()
    ))
}

/// Maps a direct connection-write failure to its session diagnostic: an expired write deadline gets the dedicated stall
/// wording (the read side's idle expiry has no twin message because a quiet read IS the idle law); every other error
/// keeps the plain shape.
fn write_failure(label: &str, writer: &SessionWriter, error: &io::Error) -> CliFailure {
    match writer.expired_after {
        Some(elapsed) => write_stall_failure(label, elapsed),
        None => CliFailure::from(format!("cannot write {label}: {error}")),
    }
}

/// Serves one connection to completion: incremental reads, per-cycle record drives, the held tail finalized at EOF.
fn serve_session(
    stream: &SessionStream,
    label: &str,
    compiled: &CompiledProgram,
    resources: &mut ResourceContext<'_>,
    diagnostics: bool,
    idle_timeout: Duration,
) -> Result<(), CliFailure> {
    resources.reset_run_diagnostics();
    // The connection is full duplex: one clone per half, each carrying its own copy of the idle window
    // (`--read-timeout` governs both).
    let mut reader = stream
        .read_half(Some(idle_timeout))
        .map_err(|error| CliFailure::from(format!("cannot split {label}: {error}")))?;
    let writer = stream
        .write_half(Some(idle_timeout))
        .map_err(|error| CliFailure::from(format!("cannot split {label}: {error}")))?;
    let mut sink = SessionSink {
        out: io::BufWriter::with_capacity(READ_CHUNK, writer),
        label: label.to_owned(),
        base_line: 0,
        base_offset: 0,
        base_record: 0,
    };
    let mut retained: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; READ_CHUNK];
    loop {
        let read = match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // Idle window expired (SessionReader's poll returns WouldBlock): finalize the held tail (a complete
                // unterminated record is a recovering accept), then end the session. Returning here would drop that
                // record.
                break;
            }
            Err(error) => {
                return Err(CliFailure::from(format!("cannot read {label}: {error}")));
            }
        };
        retained.extend_from_slice(&scratch[..read]);
        // The one cut the NDJSON framer makes exact (shared with the follow and stream routes): a record is complete
        // only after its physical terminator; everything after the last line feed is held.
        let complete_end =
            crate::routes::stream::complete_span(jqf_runtime::records::RecordInputKind::Ndjson, &retained);
        crate::routes::stream::govern_held_partial(&retained, complete_end)?;
        if complete_end > 0 {
            if let Err(error) = drive_cycle(compiled, resources, &mut sink, &retained[..complete_end], diagnostics) {
                // A write that expired mid-drive surfaces as the drive's sink error; the dedicated stall wording
                // replaces the generic rendering when the writer says the deadline is what fired.
                return Err(match sink.out.get_ref().expired_after {
                    Some(elapsed) => write_stall_failure(label, elapsed),
                    None => error,
                });
            }
            let newlines = count_newlines(&retained[..complete_end]);
            sink.base_line = sink.base_line.saturating_add(newlines);
            sink.base_record = sink.base_record.saturating_add(newlines);
            sink.base_offset = sink.base_offset.saturating_add(complete_end as u64);
            retained.drain(..complete_end);
            // Records stream OUT as they arrive; the connection is flushed at every cycle boundary so the client sees
            // each batch as it lands.
            if let Err(error) = sink.out.flush() {
                return Err(write_failure(label, sink.out.get_ref(), &error));
            }
        }
    }
    // The client closed: finalize the held tail through the recovering dialect's own law — a complete final value
    // without a terminator is a record with an advisory; a truncated one is an ordered error issue — exactly the
    // whole-input recovering behavior over the same bytes.
    if !retained.is_empty() {
        if let Err(error) = drive_cycle(compiled, resources, &mut sink, &retained, diagnostics) {
            return Err(match sink.out.get_ref().expired_after {
                Some(elapsed) => write_stall_failure(label, elapsed),
                None => error,
            });
        }
        if let Err(error) = sink.out.flush() {
            return Err(write_failure(label, sink.out.get_ref(), &error));
        }
    }
    Ok(())
}

/// Builds the empty `$ARGS` object (`{"positional": [], "named": {}}`) the CLI binds on EVERY request, so a serve
/// program reading `$ARGS` compiles exactly as a run's does. A serve request carries no user bindings, so the object is
/// always empty.
fn empty_args_value() -> Result<Value, CliFailure> {
    let positional = jqf_data::Array::try_from_vec(Vec::new()).map_err(|_| CliFailure::Message {
        class: ExitClass::Usage,
        message: "cannot allocate $ARGS.positional".to_owned(),
    })?;
    let named = ObjectBuilder::try_with_capacity(0)
        .map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS.named".to_owned(),
        })?
        .try_finish()
        .map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS.named".to_owned(),
        })?;
    let mut builder = ObjectBuilder::try_with_capacity(2).map_err(|_| CliFailure::Message {
        class: ExitClass::Usage,
        message: "cannot allocate $ARGS".to_owned(),
    })?;
    for (name, value) in [
        ("positional", Value::Array(positional)),
        ("named", Value::Object(named)),
    ] {
        let key = ObjectKey::try_from_str(name).map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS key".to_owned(),
        })?;
        builder.try_insert_last(key, value).map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS entry".to_owned(),
        })?;
    }
    builder
        .try_finish()
        .map(Value::Object)
        .map_err(|_| CliFailure::Message {
            class: ExitClass::Usage,
            message: "cannot allocate $ARGS".to_owned(),
        })
}

/// Builds ONE session's request-local resources inside its own thread: the `ResourceContext` is not `Send` (the
/// worker-grants law), so each session opens its own account under the daemon's limits shape, against the same shared
/// governor. The shape mirrors the daemon's setup exactly; only the ledger is per-session. The stderr sink and
/// diagnostics buffer are locals of the session thread, so the context never borrows caller state.
fn serve_one_session(
    stream: &SessionStream,
    label: &str,
    compiled: &CompiledProgram,
    diagnostics: bool,
    idle_timeout: Duration,
) -> Result<(), CliFailure> {
    let mut limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, 0, crate::MAX_NESTING_DEPTH);
    if std::env::var_os("JQF_PROBE_UNACCOUNTED").is_some() {
        limits = limits.with_probe_unaccounted();
    }
    let account = RequestAccount::try_new(limits)
        .map_err(|error| CliFailure::from(format!("cannot create the session account: {}", resource_note(error))))?;
    let cli_stderr = CliStderr;
    let mut resources = ResourceContext::new(
        account,
        &crate::rss::GOVERNOR,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS)
            .ok_or_else(|| CliFailure::from("invalid cooperative work quantum"))?,
    )
    .map_err(|error| CliFailure::from(format!("session cancelled before start: {error:?}")))?
    .with_environment(host_environment()?)
    .with_stderr(&cli_stderr)
    .with_host_extension(Box::new(jqf_engine::ModuleLoaderHandle::new(Box::new(
        CliModuleLoader::new(Vec::new()),
    ))));
    let diagnostics_buffer = jqf_sdk::Diagnostics::new(if diagnostics {
        DiagnosticPolicy::All
    } else {
        DiagnosticPolicy::Off
    });
    if let Some(session_diagnostics) = &diagnostics_buffer {
        resources.set_diagnostics(session_diagnostics);
    }
    serve_session(stream, label, compiled, &mut resources, diagnostics, idle_timeout)
}

/// Serves one accepted connection end-to-end on its own thread: builds the session's request-local resources, drives
/// the session, releases the freed pages, and reports a failure on the daemon's stderr — never the daemon's death.
fn run_session_thread(
    stream: &SessionStream,
    label: &str,
    compiled: &CompiledProgram,
    diagnostics: bool,
    idle_timeout: Duration,
) {
    // The session's own request-local resources are built inside `serve_one_session` — its ledger under the daemon's
    // limits shape, against the shared governor.
    let outcome = serve_one_session(stream, label, compiled, diagnostics, idle_timeout);
    if let Err(failure) = outcome {
        // A session failure is reported and the daemon continues: a client's malformed stream, a per-value error, a
        // session that crossed the memory ceiling, or a connection that closed mid-write must never kill the server.
        eprint_line_buffered(&format!("jqf: serve: {label}: {failure}"));
        flush_stderr();
    }
    // D3 : a session's reservations drop when its drive ends, and the release step returns the freed pages to the OS so
    // the daemon's RSS comes back toward baseline after a session that built a large value. Concurrent sessions make
    // this per-thread; the no-compounding law is unchanged.
    crate::rss::release_freed_pages();
}

/// Runs the `jqf serve` daemon: bind, compile once, accept forever.
///
/// Setup failures (the governor, the account, the compile, the bind) are usage-class errors raised BEFORE the loop; a
/// session failure inside the loop is reported on the daemon's stderr and the daemon continues. The loop itself ends
/// only on a signal.
#[expect(
    clippy::too_many_lines,
    reason = "one linear daemon setup: governor, account, compile, bind, then the accept loop — \
              splitting it would thread the same dozen locals through helpers, exactly as the \
              run-request body in main.rs stays one linear invocation"
)]
pub(crate) fn run_daemon(args: ServeArguments) -> Result<u8, CliFailure> {
    // A daemon must not die when a client disconnects mid-write. `main` restored SIGPIPE to SIG_DFL so `jqf … | head`
    // dies as quietly as the reference; serve re-ignores it (std's own startup default), so a write to a closed
    // connection surfaces as EPIPE and ends the session, not the process.
    // Safety: `signal(SIGPIPE, SIG_IGN)` is async-signal-safe and touches no
    // Rust state.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    // The RSS governor arms BEFORE anything binds: the daemon is the governor's hardest test, and its ceiling is
    // PER-DAEMON (the plan's statement — one account, one governor, watching the whole resident process).
    // Per-connection isolation is the drive's natural release behavior: each drive's reservations drop when it ends.
    crate::rss::configure(
        args.max_rss
            .unwrap_or(crate::rss::MaxRss::Percent(crate::rss::DEFAULT_CEILING_PERCENT)),
        args.max_rss.is_some(),
    )?;
    crate::rss::set_report_enabled(args.diagnostics);
    if args.diagnostics {
        eprint_line_buffered(&format!("jqf: {}", crate::provenance::BuildProvenance));
        flush_stderr();
    }
    let mut limits = ResourceLimits::new(
        u64::MAX,
        // No output ceiling, exactly as the run route has none (the CLI output-ceiling law lives on
        // `MAX_OUTPUT_BYTES`'s replacement in main.rs); a session's output streams to the connection unbounded.
        u64::MAX,
        u64::MAX,
        0,
        crate::MAX_NESTING_DEPTH,
    );
    if std::env::var_os("JQF_PROBE_UNACCOUNTED").is_some() {
        limits = limits.with_probe_unaccounted();
    }
    let account = RequestAccount::try_new(limits)
        .map_err(|error| format!("error: cannot create request account: {}", resource_note(error)))?;
    let cli_stderr = CliStderr;
    let diagnostics_buffer = jqf_sdk::Diagnostics::new(if args.diagnostics {
        DiagnosticPolicy::All
    } else {
        DiagnosticPolicy::Off
    });
    let mut resources = ResourceContext::new(
        account,
        &crate::rss::GOVERNOR,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).ok_or_else(|| "invalid cooperative work quantum".to_owned())?,
    )
    .map_err(|error| format!("request cancelled before start: {error:?}"))?
    .with_environment(host_environment()?)
    .with_stderr(&cli_stderr)
    .with_host_extension(Box::new(jqf_engine::ModuleLoaderHandle::new(Box::new(
        CliModuleLoader::new(Vec::new()),
    ))));
    if let Some(diagnostics) = &diagnostics_buffer {
        resources.set_diagnostics(diagnostics);
    }
    // The program: `-f` reads it from a file (the adopted message shape), else the positional, else the identity filter
    // — the CLI's own law, unchanged.
    let mut program = args.program;
    let source_program = match &args.program_file {
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|error| CliFailure::Message {
                class: ExitClass::Usage,
                message: format!(
                    "Could not open {}: {}",
                    path.display(),
                    crate::input::io_error_text(&error)
                ),
            })?;
            let text = String::from_utf8(bytes).map_err(|_| CliFailure::Message {
                class: ExitClass::Usage,
                message: "program is not valid UTF-8".to_owned(),
            })?;
            program = Some(text);
            program.as_deref().ok_or_else(|| CliFailure::Message {
                class: ExitClass::Usage,
                message: "-f read a program file but none was stored".to_owned(),
            })?
        }
        None => program.as_deref().unwrap_or("."),
    };
    let args_binding = empty_args_value()?;
    let source_label: String = args
        .program_file
        .as_ref()
        .map_or_else(|| String::from("<top-level>"), |path| path.display().to_string());
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    // COMPILED ONCE: the daemon's whole point is that the per-invocation fixed cost (compile, warm allocator, warm
    // planner) is paid a single time and every connection then answers against the same warm program.
    let compiled = try_compile_program(
        source_program,
        policy,
        CompileOptions {
            cli_vars: &[(String::from("$ARGS"), args_binding)],
            split_exp: false,
            source_label: &source_label,
        },
        &resources,
    )
    .map_err(|error| compile_failure(&error, source_program))?;
    let listener = bind_listener(&args.listen)?;
    eprint_line_buffered(&format!("jqf: serve: listening on {}", args.listen));
    flush_stderr();
    //: the session threads reserve the request stack (the same
    // knob `spawn_request` uses) — a VIRTUAL reservation per live session; resident pages are only what a drive
    // touches.
    let stack_bytes = jqf_sdk::request_stack_bytes()?;
    let compiled = Arc::new(compiled);
    let slots = Arc::new(SessionSlots::new(MAX_CONCURRENT_SESSIONS));
    let connections = AtomicU64::new(0);
    loop {
        let stream = match &listener {
            #[cfg(unix)]
            Listener::Unix { listener, .. } => {
                let (stream, _) = listener
                    .accept()
                    .map_err(|error| CliFailure::from(format!("accept failed: {error}")))?;
                SessionStream::Unix(stream)
            }
            Listener::Tcp(listener) => {
                let (stream, _) = listener
                    .accept()
                    .map_err(|error| CliFailure::from(format!("accept failed: {error}")))?;
                // A request-reply daemon pays Nagle × delayed-ACK on every small reply without this (~40 ms per
                // exchange); the session's writes are already whole frames, so disabling coalescing is pure latency
                // win. Best-effort: a refusal to set the option must not refuse the connection.
                let _ = stream.set_nodelay(true);
                SessionStream::Tcp(stream)
            }
        };
        let ordinal = connections.fetch_add(1, Ordering::Relaxed) + 1;
        let label = format!("conn#{ordinal}");
        slots.acquire();
        let spawned = std::thread::Builder::new()
            .name(format!("jqf-serve-{label}"))
            .stack_size(stack_bytes)
            .spawn({
                let compiled = Arc::clone(&compiled);
                let slots = Arc::clone(&slots);
                move || {
                    // The guard restores the slot even if the session thread panics — a trailing release statement
                    // would be skipped by the unwind, and each skipped release permanently shrank the daemon's
                    // concurrency.
                    let _slot = SessionSlotGuard { slots };
                    run_session_thread(&stream, &label, &compiled, args.diagnostics, args.read_timeout);
                }
            });
        if let Err(error) = spawned {
            // The slot was taken before the spawn; give it back and go on accepting — a thread-creation failure is one
            // connection's bad luck, not the daemon's death.
            slots.release();
            eprint_line_buffered(&format!("jqf: serve: cannot start a session thread: {error}"));
            flush_stderr();
        }
    }
}
