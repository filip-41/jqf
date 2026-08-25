#![cfg(unix)]
//! The W1 leak battery: a signal-killed spill request leaves nothing behind.
//!
//! Three deaths, each with the vacuity guard FIRST: the test only kills the child after proving a run was actually
//! written, so a pass cannot come from the spill path never engaging. The proof is the store directory itself — the
//! store creates it LAZILY, on the first run creation, so its existence in TMPDIR is exactly the fact that the spill
//! engaged (`jqf-runtime/src/ spill.rs`).
//!
//! The cleanup under test is the CLI's async-signal-safe handler: on SIGINT/SIGTERM/SIGPIPE it removes the
//! (create-then-unlinked, hence empty) store directory with one `rmdir` and re-raises under the default disposition, so
//! the process dies with the signal's own wait status and, for SIGPIPE, exactly as quietly as jq.

use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// One isolated TMPDIR for a leak test, removed on drop. The counter keeps the parallel test threads from colliding on
/// one clock-tick's worth of nanosecond names.
struct LeakDir(PathBuf);

impl LeakDir {
    fn new() -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "jqf-spill-leak-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).expect("leak dir");
        LeakDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for LeakDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes `count` descending numbers (one adjacent JSON text per line), so the sort both reorders them and spills when
/// the key budget is tiny.
fn write_input(path: &Path, count: usize) {
    let mut file = std::fs::File::create(path).expect("input file");
    for i in 0..count {
        writeln!(file, "{}", count - i).expect("input write");
    }
}

/// The spill store directory's entry in `base`, when one exists.
fn spill_dir_in(base: &Path) -> Option<PathBuf> {
    std::fs::read_dir(base)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("jqf-spill-"))
        })
}

/// Spawns a spill-budgeted `sort_by([.])` over `input` with `base` as TMPDIR. The general-drive key graph (a
/// construct-array, not a bare key) is what takes the spill path; a bare `sort` rides the static keyed lane, which does
/// not spill. stdout stays OPEN: the child cannot exit until its (unread, larger than the pipe buffer) output is
/// written, so the spill directory is guaranteed to persist from first flush until a signal.
fn spawn_spilling(input: &Path, base: &Path) -> Child {
    Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .arg("-s")
        .arg("--max-spill-bytes")
        .arg("131072")
        .arg("sort_by([.])")
        .arg(input)
        .env("TMPDIR", base)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns")
}

/// Polls `base` until the spill directory appears — the lazy-creation law, so this is a PROOF that a run was written —
/// or panics.
fn wait_for_spill(base: &Path, timeout: Duration) -> PathBuf {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(dir) = spill_dir_in(base) {
            return dir;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the spill directory never appeared: the spill path never engaged");
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child did not die within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn send_signal(pid: u32, signal: &str) {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("kill runs");
    assert!(status.success(), "kill -{signal} {pid}");
}

/// `jqf --max-spill-bytes N 'sort_by([.])' | head` must die of SIGPIPE at the quiet 141 with the spill directory
/// already removed.
#[test]
fn sigpipe_leaves_nothing_behind() {
    let leak = LeakDir::new();
    let input = leak.path().join("input");
    write_input(&input, 200_000);
    let mut child = spawn_spilling(&input, leak.path());
    // Vacuity guard: prove a run was actually written BEFORE cutting the pipe.
    wait_for_spill(leak.path(), Duration::from_secs(30));
    // Close our read end: the child's next write raises SIGPIPE, the handler removes the store directory and re-raises,
    // and the death is jq's quiet
    // 141. The child cannot have exited normally first — its output exceeds
    // the pipe buffer and we never read it, so it is blocked in a write.
    drop(child.stdout.take().expect("stdout piped"));
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    assert_eq!(status.signal(), Some(13), "must die of SIGPIPE, was {status:?}");
    assert_eq!(
        spill_dir_in(leak.path()),
        None,
        "the spill directory must not survive a SIGPIPE death"
    );
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr piped")
        .read_to_string(&mut stderr)
        .expect("stderr reads");
    assert!(
        stderr.is_empty(),
        "the SIGPIPE death must be as quiet as jq's, got: {stderr}"
    );
}

/// Ctrl-C mid-sort must leave nothing behind, and the process must die by SIGINT (the shell's 130), not linger on a
/// handled signal.
#[test]
fn sigint_mid_sort_leaves_nothing_behind() {
    let leak = LeakDir::new();
    let input = leak.path().join("input");
    write_input(&input, 300_000);
    let mut child = spawn_spilling(&input, leak.path());
    // Vacuity guard: prove a run was actually written before the kill.
    let dir = wait_for_spill(leak.path(), Duration::from_secs(30));
    // The store directory is 0700 in the running CLI, exactly as the runtime test pins it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).expect("spill dir stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the live spill directory must be 0700");
    }
    send_signal(child.id(), "INT");
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    assert_eq!(status.signal(), Some(2), "must die by SIGINT, was {status:?}");
    assert!(!dir.exists(), "the observed spill directory must be removed");
    assert_eq!(
        spill_dir_in(leak.path()),
        None,
        "no spill directory may remain after a SIGINT death"
    );
}

/// The other half of the handler's contract (systemd and process managers send TERM, not INT): same cleanup, death by
/// SIGTERM.
#[test]
fn sigterm_mid_sort_leaves_nothing_behind() {
    let leak = LeakDir::new();
    let input = leak.path().join("input");
    write_input(&input, 300_000);
    let mut child = spawn_spilling(&input, leak.path());
    // Vacuity guard: prove a run was actually written before the kill.
    let dir = wait_for_spill(leak.path(), Duration::from_secs(30));
    send_signal(child.id(), "TERM");
    let status = wait_with_timeout(&mut child, Duration::from_secs(30));
    assert_eq!(status.signal(), Some(15), "must die by SIGTERM, was {status:?}");
    assert!(!dir.exists(), "the observed spill directory must be removed");
    assert_eq!(
        spill_dir_in(leak.path()),
        None,
        "no spill directory may remain after a SIGTERM death"
    );
}
