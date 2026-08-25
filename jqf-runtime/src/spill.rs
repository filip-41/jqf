//! The host half of the spill contract: a temp-directory store of run files. The ONLY `std` in the spill path — the
//! engine and the resource crate are `no_std` by law, and `jqf-runtime` is the only crate that owns a thread and a
//! filesystem.
//!
//! Runs are files named `0.run`, `1.run`, … inside one temp directory whose name is `jqf-spill-` plus a hex suffix. The
//! prefix belongs to the directory, not the run file. The directory is created lazily at the first `create_run`. Writes
//! are full-file (the engine builds each run in a buffer); reads are sequential via a positional cursor per open run.
//!
//! # The temp-directory contract
//!
//! The store owns one private directory under the base (the system temp dir when no base is given), created with
//! mkdtemp discipline:
//!
//! - The name is `jqf-spill-` plus a 16-byte hex suffix read from `/dev/urandom` (time/pid fallback elsewhere), so the
//!   pid is not the only entropy and the path is not enumerable.
//! - Creation is EXCLUSIVE: `fs::create_dir` (never `create_dir_all`, which adopts a path that already exists,
//!   regardless of owner), at mode `0700` set at CREATION via `DirBuilderExt::mode` — never chmod'd afterward, which
//!   would leave a race window. A pre-existing path at the store's name is [`ResourceError::HostFailure`], never an
//!   adoption.
//! - The directory is created LAZILY, at the first run creation. `create_run` is the only entry into the spill path, so
//!   the directory's existence in the temp dir is exactly the fact that a run was written — which is what lets the
//!   signal leak test prove spill engaged before it asserts cleanup.
//!
//! Run files are CREATE-THEN-UNLINKED: each `create_run` opens its file with `OpenOptions::create_new(true)` at mode
//! `0600` — which refuses an existing path, including a symlink, so a run file can never be redirected — then
//! immediately unlinks it and keeps the `File` handle. POSIX keeps the inode alive until the last descriptor closes, so
//! a user's sort keys NEVER have a name on disk after the write, and the worst signal-death residue is an empty `0700`
//! directory. Reads therefore cannot reopen by name: each cursor is a CLONE of the held handle read POSITIONALLY
//! (`read_at` / `seek_read`), never a fresh `File::open` through a path.
//!
//! This is the CREATE-THEN-UNLINK choice, taken over a signal handler that unlinks named run files, for two reasons.
//! First, it removes the cleanup problem at the source: there is never anything to unlink, so the CLI's
//! SIGINT/SIGTERM/SIGPIPE handler only ever has to remove one empty directory — a single async-signal-safe `rmdir` on a
//! pre-rendered path. A handler that unlinked named runs would need a pre-rendered path PER RUN, a bounded list a large
//! spill overflows, leaving named data behind on exactly the death the handler exists for. Second, it retires the
//! latent truncate trap for free: `write_run` no longer re-opens with `File::create` — which truncates through whatever
//! path — because the write goes through the held handle with an explicit seek + `set_len`, so it can only ever affect
//! this run's own inode.
//!
//! The store is `Drop`-only on the normal path (`remove_dir_all` of a directory that holds no names); the CLI
//! additionally pre-renders the directory path into an async-signal-safe cleanup handler for the signal deaths `Drop`
//! cannot see.

use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use jqf_resource::{ResourceError, RunCursorId, RunId, SpillStore};

/// One run file on disk: an open, unlinked handle. The name existed for one syscall; the inode lives until this handle
/// (and its cursors' clones) drop.
struct RunFile {
    id: RunId,
    file: File,
}

/// The merge's per-cursor readahead: each cursor serves entries from a small buffer refilled with ONE positional read,
/// instead of the two-pread-per- entry pattern that let kernel `pread` time dominate the merge.
///
/// Memory law: the merge already holds O(runs) heads; the buffer adds O(runs × `SPILL_READAHEAD_BYTES`) — for the
/// lane's 55 runs, ~880 KiB, an order of magnitude below the O(n) positions/output the merge publishes.
const SPILL_READAHEAD_BYTES: usize = 16 * 1024;

/// Refusal ceiling for one entry's unread window. An entry is one encoded sort key plus its position, so a demand
/// beyond this bound is corruption (a hostile or damaged key length), not data: honoring it would grow the cursor
/// toward the whole run size before the honest truncation error. The bound sits far above any legitimate key and far
/// below any run a caller spills, so only corrupt input ever reaches it.
const MAX_ENTRY_WINDOW_BYTES: usize = 64 * 1024 * 1024;

/// A positional read cursor over one run file.
struct RunCursor {
    file: File,
    /// The file offset the next refill reads at.
    position: u64,
    /// The unread part of the current readahead window.
    buf: Vec<u8>,
    /// The index of the next unread byte in `buf`.
    buf_pos: usize,
}

impl RunCursor {
    /// Compacts the unread remainder to the front, then refills the window with one positional read of up to
    /// [`SPILL_READAHEAD_BYTES`]. Returns the bytes read (0 = the run's end).
    fn refill(&mut self) -> io::Result<usize> {
        let remaining = self.buf.len() - self.buf_pos;
        if remaining > 0 && self.buf_pos > 0 {
            self.buf.copy_within(self.buf_pos.., 0);
            self.buf.truncate(remaining);
        } else if remaining == 0 {
            self.buf.clear();
        }
        self.buf_pos = 0;
        let start = self.buf.len();
        self.buf
            .try_reserve(SPILL_READAHEAD_BYTES)
            .map_err(|error| io::Error::new(io::ErrorKind::OutOfMemory, error))?;
        let spare = self.buf.spare_capacity_mut();
        let want = spare.len().min(SPILL_READAHEAD_BYTES);
        // SAFETY: `read_at` writes initialized bytes into the spare prefix; `set_len` below publishes exactly that
        // prefix.
        let dst = unsafe { core::slice::from_raw_parts_mut(spare.as_mut_ptr().cast::<u8>(), want) };
        let n = read_at(&self.file, dst, self.position)?;
        debug_assert!(n <= want);
        // SAFETY: `n` bytes at `start` were initialized by `read_at`.
        unsafe {
            self.buf.set_len(start + n);
        }
        self.position += n as u64;
        Ok(n)
    }

    /// Ensures at least `need` unread bytes are buffered. Returns `false` when the run ends first.
    fn ensure(&mut self, need: usize) -> io::Result<bool> {
        while self.buf.len() - self.buf_pos < need {
            if self.refill()? == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The temp-directory spill store.
pub struct TempDirSpillStore {
    inner: RefCell<TempDirSpillStoreInner>,
}

struct TempDirSpillStoreInner {
    dir: PathBuf,
    dir_created: bool,
    runs: Vec<RunFile>,
    cursors: Vec<RunCursor>,
    next_run: u64,
    next_cursor: u64,
}

impl TempDirSpillStore {
    /// Creates the store in a fresh directory under `base` (the system temp dir when `base` is `None`). The name is
    /// unpredictable and the creation is exclusive at `0700`; see the module docs.
    ///
    /// # Errors
    ///
    /// Returns [`ResourceError::HostFailure`] when the store cannot be constructed — only if the base is unusable,
    /// since the directory itself is created lazily on the first [`SpillStore::create_run`].
    pub fn try_new(base: Option<&Path>) -> Result<Self, ResourceError> {
        let base = match base {
            Some(base) => base.to_owned(),
            None => std::env::temp_dir(),
        };
        let name = format!("jqf-spill-{}", random_suffix());
        Self::try_new_at(&base.join(name))
    }

    /// Creates the store AT `dir`, refusing a path that already exists. The directory itself is created lazily, on the
    /// first [`SpillStore::create_run`]; construction only records the path and validates that it is not already taken.
    /// `try_new` derives `dir` from a random suffix; this form is the test seam that pins the refusal and the mode.
    ///
    /// # Errors
    ///
    /// Returns [`ResourceError::HostFailure`] when `dir` already exists.
    pub fn try_new_at(dir: &Path) -> Result<Self, ResourceError> {
        // Advisory early check: the authoritative exclusive create still happens at the first create_run, but a caller
        // that constructed over a taken path hears about it here, immediately (and the refusal test pins this call).
        if fs::metadata(dir).is_ok() {
            return Err(ResourceError::HostFailure {
                detail: "spill temp dir path already exists",
            });
        }
        Ok(Self {
            inner: RefCell::new(TempDirSpillStoreInner {
                dir: dir.to_owned(),
                dir_created: false,
                runs: Vec::new(),
                cursors: Vec::new(),
                next_run: 0,
                next_cursor: 0,
            }),
        })
    }

    /// The store's directory path, whether or not the directory has been created yet. The CLI reads this once, at
    /// construction, to pre-render the path its async-signal-safe cleanup handler removes.
    #[must_use]
    pub fn temp_dir(&self) -> PathBuf {
        self.inner.borrow().dir.clone()
    }
}

impl Drop for TempDirSpillStore {
    fn drop(&mut self) {
        // Only remove a directory this store created. Construction records a path; a pre-existing path that appears
        // before the exclusive create is HostFailure, never an adoption — and never ours to delete.
        let inner = self.inner.borrow();
        if inner.dir_created {
            let _ = fs::remove_dir_all(&inner.dir);
        }
    }
}

impl SpillStore for TempDirSpillStore {
    fn create_run(&self) -> Result<RunId, ResourceError> {
        let mut inner = self.inner.borrow_mut();
        if !inner.dir_created {
            create_private_dir(&inner.dir).map_err(|_| ResourceError::HostFailure {
                detail: "spill temp dir creation failed",
            })?;
            inner.dir_created = true;
        }
        let id = RunId(inner.next_run);
        inner.next_run = inner.next_run.saturating_add(1);
        let path = inner.dir.join(format!("{}.run", id.0));
        // create_new refuses an existing path — including a symlink — so the run can never be redirected through one.
        // The failure surfaces here, at the earliest point, exactly as the old File::create touch did.
        let file = create_private_run_file(&path).map_err(|_| ResourceError::HostFailure {
            detail: "spill create failed",
        })?;
        // Create-then-unlink: from here on the run has no name. A failed unlink is a host failure — a named run is
        // exactly what this scheme forbids.
        fs::remove_file(&path).map_err(|_| ResourceError::HostFailure {
            detail: "spill create failed",
        })?;
        inner.runs.push(RunFile { id, file });
        Ok(id)
    }

    fn write_run(&self, id: RunId, bytes: &[u8]) -> Result<(), ResourceError> {
        let mut inner = self.inner.borrow_mut();
        let run = inner
            .runs
            .iter_mut()
            .find(|run| run.id == id)
            .ok_or(ResourceError::HostFailure {
                detail: "spill write to unknown run",
            })?;
        // The handle is the one created (and unlinked) by create_run. Seek to the front and truncate so a repeated
        // write REPLACES the run exactly as the old File::create re-open did — while being unable to touch anything but
        // this run's own inode (no path, no race).
        let result = (|| -> io::Result<()> {
            run.file.seek(SeekFrom::Start(0))?;
            run.file.set_len(0)?;
            run.file.write_all(bytes)
        })();
        result.map_err(|_| ResourceError::HostFailure {
            detail: "spill write failed",
        })
    }

    fn open_run(&self, id: RunId) -> Result<RunCursorId, ResourceError> {
        let mut inner = self.inner.borrow_mut();
        let run = inner
            .runs
            .iter()
            .find(|run| run.id == id)
            .ok_or(ResourceError::HostFailure {
                detail: "spill open of unknown run",
            })?;
        let file = run.file.try_clone().map_err(|_| ResourceError::HostFailure {
            detail: "spill open failed",
        })?;
        let cursor = RunCursorId(inner.next_cursor);
        inner.next_cursor = inner.next_cursor.saturating_add(1);
        inner.cursors.push(RunCursor {
            file,
            position: 0,
            buf: Vec::new(),
            buf_pos: 0,
        });
        Ok(cursor)
    }

    fn read_next(&self, cursor: RunCursorId, out: &mut Vec<u8>) -> Result<Option<u64>, ResourceError> {
        let mut inner = self.inner.borrow_mut();
        // The cursor index is the RunCursorId's low bits: the store hands out cursors in order and never removes one
        // before the merge ends.
        let index = usize::try_from(cursor.0).unwrap_or(usize::MAX);
        let entry = inner.cursors.get_mut(index).ok_or(ResourceError::HostFailure {
            detail: "spill read of unknown cursor",
        })?;
        // One entry: u32 key_len + key bytes + u64 position. The cursor serves entries from its readahead window (one
        // pread per refill), never the per-entry positional read pair the merge used to pay for every one of its 300k
        // steps. The file was unlinked at create, so there is no path to reopen, and on unix a cloned descriptor shares
        // the file offset, so reads are positional on the cursor's own offset.
        if !entry.ensure(4).map_err(|_| ResourceError::HostFailure {
            detail: "spill read failed",
        })? {
            // Zero unread bytes is the run's clean end; 1-3 bytes is a run truncated mid-header and must raise — a
            // corruption masquerading as the run's end would silently drop the remainder.
            if entry.buf.len() - entry.buf_pos == 0 {
                return Ok(None);
            }
            return Err(ResourceError::HostFailure {
                detail: "spill entry truncated",
            });
        }
        let key_len = u32::from_le_bytes(
            entry.buf[entry.buf_pos..entry.buf_pos + 4]
                .try_into()
                .expect("a four-byte window was ensured above"),
        ) as usize;
        entry.buf_pos += 4;
        // A corrupt or hostile key length must not steer this cursor into growing its window toward the whole run size
        // one readahead at a time. An entry is one encoded sort key plus its position, so a demand past
        // `MAX_ENTRY_WINDOW_BYTES` is corruption: refuse before any refill allocates.
        if key_len > MAX_ENTRY_WINDOW_BYTES - 8 {
            return Err(ResourceError::HostFailure {
                detail: "spill entry length exceeds bound",
            });
        }
        if !entry.ensure(key_len + 8).map_err(|_| ResourceError::HostFailure {
            detail: "spill read failed",
        })? {
            return Err(ResourceError::HostFailure {
                detail: "spill entry truncated",
            });
        }
        let key_start = out.len();
        out.extend_from_slice(&entry.buf[entry.buf_pos..entry.buf_pos + key_len + 8]);
        entry.buf_pos += key_len + 8;
        let position = u64::from_le_bytes(out[key_start + key_len..key_start + key_len + 8].try_into().map_err(
            |_| ResourceError::HostFailure {
                detail: "spill position width",
            },
        )?);
        Ok(Some(position))
    }

    fn delete_run(&self, id: RunId) -> Result<(), ResourceError> {
        let mut inner = self.inner.borrow_mut();
        if let Some(index) = inner.runs.iter().position(|run| run.id == id) {
            // The file was unlinked at create; dropping the handle closes the inode. There is nothing left to remove
            // from the directory.
            inner.runs.swap_remove(index);
        }
        Ok(())
    }
}

/// `mkdir` at mode 0700, exclusive: fails when the path exists. The mode is set at CREATION — masked by the process
/// umask, so exclusivity, not the mode, is the load-bearing half — and never chmod'd afterward, which would leave a
/// window where the directory exists at the wrong mode.
#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    // No permission bits on Windows; exclusivity is the load-bearing half.
    fs::create_dir(path)
}

/// Opens a NEW run file at mode 0600. `create_new` refuses an existing path — which includes a symlink, never following
/// it — so a run file can never be redirected through one. The handle must be readable as well as writable, because
/// cursors read through CLONES of it.
#[cfg(unix)]
fn create_private_run_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_run_file(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

/// `pread` — the position-independent read that shares no file offset state, so cloned handles never race.
#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

/// Windows' positional read; a duplicated handle owns its own file pointer there, but `seek_read` is symmetric and
/// keeps one read law across targets.
#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

/// Targets with neither `pread` nor `seek_read` (wasm and friends): seek + read. Nothing on those targets spills today
/// — the store's host side is filesystem-backed — so this arm exists to keep the module compilable, never as a second
/// read law.
#[cfg(not(any(unix, windows)))]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::io::{Read as _, Seek, SeekFrom};
    let mut guard = file;
    guard.seek(SeekFrom::Start(offset))?;
    Ok(guard.read(buf)?)
}

/// 16 bytes of hex from `/dev/urandom`, the unpredictable half of the name. Exclusivity — never adoption — is the
/// actual security boundary, so a failure here degrades to the time/pid fallback instead of refusing to spill at all.
#[cfg(unix)]
fn random_suffix() -> String {
    // `Read` is unix-only in this file (the windows `read_at` uses `seek_read`, never the trait), so the import lives
    // beside its use.
    use std::io::Read;
    let mut bytes = [0u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_ok()
    {
        return format!("{:032x}", u128::from_be_bytes(bytes));
    }
    fallback_suffix()
}

#[cfg(not(unix))]
fn random_suffix() -> String {
    fallback_suffix()
}

/// Time/pid/counter fallback for platforms without `/dev/urandom`. Still unpredictable in practice (a per-process
/// counter, the pid, and the sub-second clock).
fn fallback_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let subsec_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let pid = std::process::id();
    // `swap_bytes` keeps the little-endian nibble order the previous byte-array spelling produced, so the suffix
    // alphabet is unchanged.
    format!(
        "{:016x}{:08x}{:08x}",
        counter.swap_bytes(),
        pid.swap_bytes(),
        subsec_nanos.swap_bytes()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_run_raises_not_clean_end() {
        let store = TempDirSpillStore::try_new(None).expect("store");
        let id = store.create_run().expect("run");
        // A run cut mid-header: only two of the entry's four key-length bytes are present. `read_next` must raise,
        // never report a clean end — a truncated run is corruption, and treating it as the run's end would silently
        // drop the remainder.
        store.write_run(id, &[0x02, 0x00]).expect("write");
        let cursor = store.open_run(id).expect("open");
        let mut buf = Vec::new();
        match store.read_next(cursor, &mut buf) {
            Err(ResourceError::HostFailure { .. }) => {}
            other => panic!("truncated run must raise, got {other:?}"),
        }
    }

    #[test]
    fn empty_run_is_a_clean_end() {
        let store = TempDirSpillStore::try_new(None).expect("store");
        let id = store.create_run().expect("run");
        // Zero bytes is the legitimate end of a run: the same read must report `None`, not the truncation error above.
        store.write_run(id, &[]).expect("write");
        let cursor = store.open_run(id).expect("open");
        let mut buf = Vec::new();
        assert_eq!(store.read_next(cursor, &mut buf).expect("read"), None);
    }

    #[test]
    fn corrupt_key_length_refuses_before_the_window_grows() {
        let store = TempDirSpillStore::try_new(None).expect("store");
        let id = store.create_run().expect("run");
        // A header declaring a key longer than any legitimate entry is corruption. The cursor must refuse it at once —
        // never grow its readahead window toward the run size one refill at a time before discovering the file has no
        // such entry.
        let mut run = Vec::new();
        run.extend_from_slice(&u32::MAX.to_le_bytes());
        run.extend_from_slice(b"trailing bytes that are not the key");
        store.write_run(id, &run).expect("write");
        let cursor = store.open_run(id).expect("open");
        let mut buf = Vec::new();
        match store.read_next(cursor, &mut buf) {
            Err(ResourceError::HostFailure { detail }) => {
                assert_eq!(detail, "spill entry length exceeds bound");
            }
            other => panic!("corrupt key length must refuse, got {other:?}"),
        }
    }
}
