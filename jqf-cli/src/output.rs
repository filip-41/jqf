//! The CLI's output surface: the ONE destination path (stdout or a file committed atomically), the sinks that publish
//! item streams through it, and the buffered edit sink. The mode-preservation and symlink-following laws live here
//! once, shared by every write path.
//!
//! # The atomic-write durability guarantee
//!
//! An atomic write is temp-file + fsync(data) + rename + fsync(parent dir). The data fsync before the rename is what
//! makes a crash leave EITHER the original file or the complete new one, never a truncated replacement with a name that
//! already points at unflushed data. The parent-dir fsync after the rename is best-effort (some platforms cannot open a
//! directory for sync) and covers the rename metadata itself.
//!
//! What is guaranteed: against PROCESS failure (any error, any signal that lets the process unwind) the original file
//! is untouched until the rename, and the rename is all-or-nothing. Against POWER LOSS, the data fsync bounds the
//! window: without it the rename's directory metadata could land before the data blocks and the replacement could be
//! truncated at recovery. Not guaranteed: a crash between the temp-file write and the rename loses the new content (the
//! original survives); filesystems that do not honour fsync (some network mounts) are outside the promise.
//!
//! # The atomic-replace model
//!
//! An atomic replace publishes a NEW inode renamed over the old one. What survives: the original file's mode (preserved
//! onto the temp before the rename) and, best-effort, its owner (`chown` when the process may — a privileged run must
//! not re-own the file it replaces; a non-privileged run already owns both inodes). What does NOT survive: hardlinks
//! DETACH (a sibling name keeps the old inode's content — the same law `sed -i` and `mv` document), and
//! ACLs/xattrs/labels are not carried onto the new inode. `--no-atomic` is the same-inode escape: it writes the
//! original inode directly, so hardlinks and xattrs survive a successful run at the cost of a partial file on failure.

use std::fs;
use std::io::{self, Write as IoWrite};
use std::path::{Path, PathBuf};

use jqf_codec_core::RecordIssueSeverity;
use jqf_sdk::{ItemSink, RecordIssueReport, SequenceValueError};

use crate::errors::{CliFailure, render_codec_diagnostic};
use crate::{eprint_buffered, eprint_record_issue, eprint_value_error_at};

pub(crate) struct StdoutSink<W> {
    pub(crate) output: W,
    /// Flush the output after every completed item (`--unbuffered`).
    pub(crate) unbuffered: bool,
    /// Items published so far. The CLI reads it for `-e`: zero items means the "no results" exit class (the adopted
    /// exit 4).
    pub(crate) exit_items: u64,
    /// The LAST published item's value truthiness, when the publication path judged one. `-e` exits 0 for a truthy last
    /// value, 1 for false/null.
    pub(crate) exit_last_truthy: Option<bool>,
    /// The LAST published item's "empty array" verdict, when the publication path judged one. `--diff` reads it for the
    /// exit law (0 equal, 1 differ); every other lane leaves it `None`.
    pub(crate) exit_last_empty_array: Option<bool>,
    /// The framing codec that rendered the stream's record issues, chosen once per request from the input format
    /// (json-seq issues carry json-seq's own text, NDJSON issues carry NDJSON's).
    pub(crate) issue_text: fn(jqf_codec_core::RecordIssueCode) -> (&'static str, &'static str),
    /// Colour rendering: `Some` when this request renders colour — item bytes are buffered per item and rendered at
    /// `finish_item`, because the raw-text skip arrives in the item report. `None` is the pass-through path: the bytes
    /// this sink receives are the bytes it writes, verbatim.
    pub(crate) colour: Option<crate::colour::ColourRender>,
    /// The current item's buffered bytes (colour engaged only).
    pub(crate) pending: Vec<u8>,
    /// The rendered buffer, reused across items (colour engaged only).
    pub(crate) rendered: Vec<u8>,
    /// The split destination (`--split-exp`,): one file per published ITEM, its path the split expression's per-item
    /// string output. `Some` exactly when engaged; the per-item file opens at [`ItemSink:begin_item_named`], receives
    /// the item's bytes at [`ItemSink:write`], and is committed (flush + atomic rename) at [`ItemSink:finish_item`].
    /// The `output` writer is unused while split is engaged.
    pub(crate) split: Option<SplitDestination>,
}

/// The split destination's per-item file state (see [`StdoutSink:split`]).
///
/// One [`CliWriter`] is live at a time — the current item's — opened at `begin_item_named` and committed at
/// `finish_item`, so the `.jqf-tmp-{pid}` per-directory atomic temp name is never shared by two destinations (the 143
/// S2 constraint: commit one destination before opening the next, sequential by construction, which the item drive
/// already is).
pub(crate) struct SplitDestination {
    /// The current item's open writer; committed and closed at `finish_item`.
    current: Option<CliWriter>,
    /// Write file destinations directly instead of atomically (`--no-atomic`).
    atomic: bool,
}

impl SplitDestination {
    /// The split state for one request.
    pub(crate) fn new(atomic: bool) -> Self {
        Self { current: None, atomic }
    }

    /// Opens the current item's destination file. A missing parent directory is the error naming the path (D20: jqf
    /// never `mkdir -p` a path a program derived from untrusted input).
    fn open(&mut self, name: &str) -> io::Result<()> {
        // The previous item's writer was committed at `finish_item`; a live writer here is an internal contract
        // violation, not a recovery.
        debug_assert!(self.current.is_none());
        let writer = CliWriter::file(name, self.atomic).map_err(|failure| io::Error::other(format!("{failure}")))?;
        self.current = Some(writer);
        Ok(())
    }

    /// Flushes and commits the current item's file (atomic rename unless `--no-atomic`). Consumes the writer so each
    /// destination commits once.
    fn commit(&mut self) -> io::Result<()> {
        if let Some(writer) = self.current.take() {
            writer
                .commit()
                .map_err(|failure| io::Error::other(format!("{failure}")))?;
        }
        Ok(())
    }
}

/// The host stderr channel for `stderr/0`: writes each compact value exactly as jq does — the rendered bytes with NO
/// trailing newline.
pub(crate) struct CliStderr;

impl jqf_resource::StderrSink for CliStderr {
    fn write_compact(&self, bytes: &[u8]) -> Result<(), jqf_resource::ResourceError> {
        // The same buffered channel every other stderr write uses; a write failure is the channel's business, never the
        // run's.
        eprint_buffered(bytes);
        Ok(())
    }
}

impl<W: IoWrite> ItemSink for StdoutSink<W> {
    type Error = io::Error;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        // With a split destination the SDK calls `begin_item_named` (the policy carries the split program); an unnamed
        // begin here means a route bypassed the name seam, which would silently drop items into nowhere. The one direct
        // `begin_item` caller outside `encode_one` — the roundtrip echo — declines when split is engaged, so this is
        // defensive.
        if self.split.is_some() {
            return Err(io::Error::other("the split destination requires an item name"));
        }
        Ok(())
    }

    fn begin_item_named(&mut self, _index: u64, name: &str) -> Result<(), Self::Error> {
        if let Some(split) = &mut self.split {
            split.open(name)
        } else {
            // No split destination: the SDK calls this only when the policy carries a split program, so reaching it
            // here is a no-op.
            Ok(())
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        // Split writes go DIRECTLY to the current item's file, bypassing the colour buffer: colour is a rendering of
        // JSON-family output toward a terminal-shaped destination, and a split file is a file destination exactly like
        // `--output`'s. A forced `-C` over split files writes plain bytes, the same recorded corner `--no-atomic
        // --output` keeps for files.
        if let Some(split) = &mut self.split {
            return match &mut split.current {
                Some(writer) => writer.write(bytes),
                None => Err(io::Error::other("the split destination has no open item")),
            };
        }
        if self.colour.is_some() {
            // Colour renders at ITEM boundaries: buffer the item's bytes and render them in `finish_item`, where the
            // item report carries the raw-text fact the raw-arm skip reads. The colour-off path stays the direct write
            // it always was.
            self.pending.extend_from_slice(bytes);
            return Ok(bytes.len());
        }
        self.output.write(bytes)
    }

    fn finish_item(&mut self, _index: u64, report: jqf_sdk::EncodedItemReport) -> Result<(), Self::Error> {
        // The split destination commits its per-item file FIRST — the bytes must be on disk for the item to count
        // toward the exit facts — and the next item opens its own destination.
        if let Some(split) = &mut self.split {
            split.commit()?;
        }
        if let Some(colour) = &self.colour
            && !self.pending.is_empty()
        {
            // the adopted `-r` raw arm law: a ROOT text item's bytes ARE the string — the value, not a rendering — so a
            // raw-printed root string (or projected-text scalar) is written verbatim with no colour. Every other item
            // renders its JSON tokens.
            let raw_text = report.raw_text_root() && colour.raw_arm && !colour.ascii;
            if raw_text {
                self.output.write_all(&self.pending)?;
            } else {
                self.rendered.clear();
                if colour.terminal_tree {
                    crate::colour::render_terminal(&colour.palette, &self.pending, &mut self.rendered);
                } else {
                    crate::colour::render(&colour.palette, &self.pending, &mut self.rendered);
                }
                self.output.write_all(&self.rendered)?;
            }
            self.pending.clear();
        }
        // `-e`/`--exit-status` and `--unbuffered` are observed HERE, at the one boundary every route (sequence, record,
        // the single-document drives, and the relayed parallel path) shares. The item count and the last value's
        // truthiness come straight from the report the SDK computed at encode time, so the exit-status law never
        // re-derives a fact it could get wrong from bytes (`-r` output of the string "false" is truthy while the
        // boolean false is not — the same five bytes).
        self.exit_items = self.exit_items.saturating_add(1);
        if report.value_truthy().is_some() {
            self.exit_last_truthy = report.value_truthy();
        }
        if report.value_empty_array().is_some() {
            self.exit_last_empty_array = report.value_empty_array();
        }
        if self.unbuffered {
            self.output.flush()?;
        }
        Ok(())
    }

    fn report_record_issue(&mut self, issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        crate::output::render_record_issue(&issue, 0, 0, self.issue_text);
        Ok(())
    }

    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        // jq reports each per-value runtime error to stderr and continues to the next value; a failing stderr write
        // must not abort the run, so the result is discarded (the exit class still reflects the last value).
        //
        // The frame is this facade's ONLY contribution: the location, the `(not a string)` clause jq places before the
        // colon, and the message all arrive pre-rendered from the engine's raise site, which is the only place the
        // operands jq interpolates still exist. The line goes through the buffered channel WITHOUT a fmt pass — the
        // measured hot path for error-heavy streams.
        eprint_value_error_at(
            error.filename().unwrap_or("<stdin>"),
            error.input_line(),
            error.frame_note(),
            error.message(),
        );
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        // The streaming drive's law: before the drive blocks on the next read, already-published items must be
        // observable — a buffered sink would otherwise hold a live tail until EOF, which reads exactly like the hang
        // the stream route exists to fix. This is the per-refill cadence; `--unbuffered`'s per-item flush rides
        // `finish_item` above.
        self.output.flush()
    }
}

/// Renders one record issue to the buffered stderr channel in the CLI's own `jqf: record <severity> (record N, byte O):
/// code: detail` shape, with the offset re-based by `base_offset` (0 for the ordinary whole-input sink; a live stream's
/// absolute base for a cycle-driven route) and the code/message text from the FRAMING codec that raised it (json-seq
/// issues carry json-seq's own text, NDJSON issues carry NDJSON's — the sink's per-request choice). One rendering law:
/// the record, follow, stream, and serve routes re-base and re-text, they never re-render — a divergence in the issue
/// spelling between the static route and a live route is a bug this helper exists to make impossible.
pub(crate) fn render_record_issue(
    issue: &RecordIssueReport<'_>,
    base_offset: u64,
    base_record: u64,
    issue_text: fn(jqf_codec_core::RecordIssueCode) -> (&'static str, &'static str),
) {
    let (code, message) = issue_text(issue.code());
    let severity = match issue.severity() {
        RecordIssueSeverity::Advisory => "advisory",
        RecordIssueSeverity::Error => "error",
    };
    let detail = issue
        .cause()
        .and_then(render_codec_diagnostic)
        .unwrap_or_else(|| message.to_owned());
    eprint_record_issue(
        severity,
        issue.ordinal().saturating_add(base_record),
        issue.offset().saturating_add(base_offset),
        code,
        &detail,
    );
}

/// Writes one complete output buffer to stdout or a file destination.
pub(crate) fn write_output_bytes(bytes: &[u8], path: Option<&Path>, no_atomic: bool) -> Result<(), CliFailure> {
    match path {
        None => io::stdout()
            .lock()
            .write_all(bytes)
            .map_err(|error| CliFailure::from(format!("cannot write stdout: {error}"))),
        Some(path) => {
            if no_atomic {
                fs::write(path, bytes)
                    .map_err(|error| CliFailure::from(format!("cannot write {}: {error}", path.display())))
            } else {
                write_atomic(path, bytes)
            }
        }
    }
}

/// Resolves a destination path to the physical file an atomic replace should touch, plus the original mode to preserve.
///
/// A symlink is followed to its target, so an atomic rename updates the LINK'S TARGET rather than replacing the link
/// with a regular file; the mode of the (possibly symlink-resolved) target is captured so a replacement keeps it. A
/// dangling symlink is followed to its missing target — the link stays a link. A path that does not exist yet and is
/// not a symlink resolves to itself with no mode. Never fails: the fallible reads degrade to the self-path / no-mode
/// defaults.
fn resolve_destination(path: &Path) -> (PathBuf, Option<std::fs::Permissions>) {
    let target = resolve_write_target(path);
    let mode = fs::metadata(&target).ok().map(|metadata| metadata.permissions());
    (target, mode)
}

/// Follows a destination through any symlink chain to the path a write should touch. `canonicalize` requires the target
/// to exist, so a dangling link would otherwise resolve to the link itself and an atomic rename would replace the
/// symlink with a regular file, orphaning the intended target.
fn resolve_write_target(path: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(path) {
        return canonical;
    }
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        let Ok(meta) = fs::symlink_metadata(&current) else {
            return current;
        };
        if !meta.file_type().is_symlink() {
            return current;
        }
        let Ok(link) = fs::read_link(&current) else {
            return current;
        };
        current = if link.is_absolute() {
            link
        } else {
            match current.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.join(link),
                _ => Path::new(".").join(link),
            }
        };
        if let Ok(canonical) = fs::canonicalize(&current) {
            return canonical;
        }
    }
    current
}

/// The original target's owner (`uid`, `gid`) when it can be read, for best-effort preservation across the atomic
/// replace. An atomic replace creates a NEW inode owned by the running process; without a chown a root-run edit would
/// re-own a user's file. The chown is best-effort (`fchown`-class permission rules: only a privileged process may
/// change ownership) — a non-privileged run where the running user already owns the file needs no chown, and a
/// privileged run preserves the original owner.
#[cfg(unix)]
fn target_owner(path: &std::path::Path) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt as _;
    fs::metadata(path).ok().map(|metadata| (metadata.uid(), metadata.gid()))
}

/// Best-effort owner preservation on a replacement inode: `chown` the temp file to the original owner when one was
/// captured. Errors are ignored — a non-privileged process cannot chown (and does not need to when the file is already
/// theirs); the atomic-replace model is documented, not fought.
#[cfg(unix)]
fn preserve_owner(temp: &std::path::Path, owner: Option<(u32, u32)>) {
    use std::os::unix::fs::chown;
    if let Some((uid, gid)) = owner {
        let _ = chown(temp, Some(uid), Some(gid));
    }
}

#[cfg(not(unix))]
fn target_owner(_path: &std::path::Path) -> Option<(u32, u32)> {
    None
}

#[cfg(not(unix))]
fn preserve_owner(_temp: &std::path::Path, _owner: Option<(u32, u32)>) {}

/// Fsyncs a parent directory after a rename, so the rename's metadata cannot land before the temp file's data blocks:
/// on a crash the directory entry for the replacement must not point at unflushed data. Best-effort — some platforms
/// cannot open a directory for sync; the data fsync above is the load-bearing half.
fn sync_parent(dir: &std::path::Path) {
    if let Ok(directory) = fs::File::open(dir) {
        let _ = directory.sync_all();
    }
}

/// Writes a file atomically: a same-directory temp file renamed over the target, so a failed or partial write never
/// truncates the original. The final commit differs from the non-atomic path only in that it is a rename; the
/// destination resolution, mode preservation, and symlink handling are shared with `CliWriter:file`.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CliFailure> {
    let (target, mode) = resolve_destination(path);
    let owner = target_owner(&target);
    let dir = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let name = target.file_name().and_then(|name| name.to_str()).unwrap_or("out");
    let temp = dir.join(format!(".{name}.jqf-tmp-{}", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file = fs::File::create(&temp).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        // Preserve the original file's mode on the replacement: a fresh temp file would otherwise publish the
        // process-default 0644 over a `chmod 600` config.
        if let Some(mode) = mode {
            file.set_permissions(mode)
                .map_err(|error| format!("cannot preserve mode on {}: {error}", path.display()))?;
        }
        //: best-effort owner preservation — a privileged run must
        // not re-own the file it replaces.
        preserve_owner(&temp, owner);
        file.write_all(bytes)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        // 126 P1: the data must reach disk BEFORE the rename publishes the new name — a crash between the two otherwise
        // leaves the directory entry pointing at unflushed (or zero-length) data blocks.
        file.sync_all()
            .map_err(|error| format!("cannot fsync {}: {error}", path.display()))?;
        drop(file);
        fs::rename(&temp, &target).map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
        sync_parent(dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(CliFailure::from)
}

/// The ordinary sink's destination: stdout, or a file committed atomically.
pub(crate) enum CliWriter {
    Stdout(io::BufWriter<io::StdoutLock<'static>>),
    File {
        writer: io::BufWriter<fs::File>,
        /// Pending temp file to rename over `target` on commit.
        temp: Option<std::path::PathBuf>,
        target: std::path::PathBuf,
    },
}

impl CliWriter {
    pub(crate) fn stdout() -> Self {
        // Buffer explicitly: `Stdout` is line-buffered even when piped, so per-item newline framing would otherwise
        // force one write syscall per published item (measured ~20% of wall on 36k-item fan-outs).
        Self::Stdout(io::BufWriter::with_capacity(64 * 1024, io::stdout().lock()))
    }

    pub(crate) fn file(path: impl AsRef<Path>, atomic: bool) -> Result<Self, CliFailure> {
        // Destination resolution, symlink following, and mode preservation are shared with `write_atomic`, so the two
        // write paths cannot disagree
        let path = path.as_ref();
        let (target, mode) = resolve_destination(path);
        let owner = target_owner(&target);
        if atomic {
            let dir = target
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            let temp = dir.join(format!(".jqf-tmp-{}", std::process::id()));
            let file = fs::File::create(&temp).map_err(|error| format!("cannot create {}: {error}", path.display()))?;
            // Everything between the temp's creation and the writer's construction must clean the temp up on failure —
            // the Drop guard only exists once `Self:File` does.
            let prepared: Result<(), String> = (|| {
                if let Some(mode) = mode {
                    file.set_permissions(mode)
                        .map_err(|error| format!("cannot preserve mode on {}: {error}", path.display()))?;
                }
                //: best-effort owner preservation (see `write_atomic`).
                preserve_owner(&temp, owner);
                Ok(())
            })();
            if let Err(message) = prepared {
                let _ = fs::remove_file(&temp);
                return Err(CliFailure::from(message));
            }
            Ok(Self::File {
                writer: io::BufWriter::with_capacity(64 * 1024, file),
                temp: Some(temp),
                target,
            })
        } else {
            let file =
                fs::File::create(&target).map_err(|error| format!("cannot create {}: {error}", path.display()))?;
            Ok(Self::File {
                writer: io::BufWriter::with_capacity(64 * 1024, file),
                temp: None,
                target,
            })
        }
    }

    /// Flushes, then — for an atomic file destination — renames the temp file over the target. Consumes the writer so
    /// the rename happens exactly once.
    pub(crate) fn commit(mut self) -> Result<(), CliFailure> {
        match &mut self {
            Self::Stdout(writer) => writer
                .flush()
                .map_err(|error| CliFailure::from(format!("cannot flush stdout: {error}"))),
            Self::File { writer, temp, target } => {
                writer
                    .flush()
                    .map_err(|error| format!("cannot flush {}: {error}", target.display()))?;
                if let Some(temp_path) = temp.as_ref() {
                    // 126 P1: fsync the data before the rename publishes the new name (see `write_atomic`).
                    writer
                        .get_ref()
                        .sync_all()
                        .map_err(|error| format!("cannot fsync {}: {error}", target.display()))?;
                    fs::rename(temp_path, target.as_path())
                        .map_err(|error| format!("cannot replace {}: {error}", target.display()))?;
                    // The rename CONSUMED the temp file; clear the slot so a later Drop neither re-removes it nor —
                    // worse — removes a DIFFERENT writer's temp, which reuses this process's `.jqf-tmp-{pid}` name.
                    *temp = None;
                    sync_parent(
                        target
                            .parent()
                            .filter(|parent| !parent.as_os_str().is_empty())
                            .unwrap_or_else(|| std::path::Path::new(".")),
                    );
                }
                Ok(())
            }
        }
    }
}

impl Drop for CliWriter {
    fn drop(&mut self) {
        if let Self::File { temp: Some(temp), .. } = self {
            let _ = fs::remove_file(temp);
        }
    }
}

impl IoWrite for CliWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(writer) => writer.write(bytes),
            Self::File { writer, .. } => writer.write(bytes),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stdout(writer) => writer.flush(),
            Self::File { writer, .. } => writer.flush(),
        }
    }
}

/// Buffers one edit run's published bytes so a failing edit publishes nothing to the destination.
pub(crate) struct EditBufferSink<'a> {
    pub(crate) bytes: &'a mut Vec<u8>,
}

impl ItemSink for EditBufferSink<'_> {
    type Error = io::Error;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: jqf_sdk::EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }

    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        // Without this override the trait's default silently drops the notification (037 finding 6): the SDK's
        // `report_and_fail_raised`/ `report_and_fail_runtime` still call this before building the terminal
        // `PipelineFailure`, and `render_pipeline_failure` maps that failure to `CliFailure:Reported` — a variant whose
        // `Display` is deliberately empty because it trusts the sink already streamed the diagnostic (`StdoutSink`
        // does, below). A buffered edit run has no per-value stream to piggyback on, so it must render the same frame
        // `StdoutSink` does, right here, or the diagnostic never reaches stderr at all.
        eprint_value_error_at(
            error.filename().unwrap_or("<stdin>"),
            error.input_line(),
            error.frame_note(),
            error.message(),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{CliWriter, write_output_bytes};
    use std::fs;
    use std::io::Write as _;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("jqf-output-{name}-{}", std::process::id()));
        path
    }

    /// A missing path that is not a symlink still creates a regular file.
    #[test]
    fn missing_output_path_creates_a_regular_file() {
        let path = temp_path("fresh");
        let _ = fs::remove_file(&path);
        write_output_bytes(b"x\n", Some(path.as_path()), false)
            .unwrap_or_else(|error| panic!("atomic write of a new path: {error}"));
        let meta = fs::symlink_metadata(&path).expect("created");
        assert!(!meta.file_type().is_symlink(), "a new path is a regular file");
        assert_eq!(fs::read(&path).expect("readable"), b"x\n");
        let _ = fs::remove_file(&path);
    }

    /// A dangling symlink is followed to its missing target: both write modes create the target and leave the link in
    /// place. See [`super:resolve_destination`].
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_output_writes_the_target_and_keeps_the_link() {
        for (name, no_atomic) in [("atomic", false), ("no-atomic", true)] {
            let target = temp_path(&format!("{name}-missing"));
            let link = temp_path(&format!("{name}-link"));
            let _ = fs::remove_file(&target);
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(&target, &link).expect("dangling symlink");
            write_output_bytes(b"{\"a\":1}\n", Some(link.as_path()), no_atomic)
                .unwrap_or_else(|error| panic!("write through dangling symlink: {error}"));
            let link_meta = fs::symlink_metadata(&link).expect("link remains");
            assert!(
                link_meta.file_type().is_symlink(),
                "{name}: the destination stays a symlink"
            );
            assert_eq!(
                fs::read(&target).expect("target created"),
                b"{\"a\":1}\n",
                "{name}: bytes land on the intended target"
            );
            let _ = fs::remove_file(&target);
            let _ = fs::remove_file(&link);
        }
    }

    /// The streaming writer shares [`super:resolve_destination`], so it must keep the same dangling-symlink law as
    /// [`write_output_bytes`].
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_cli_writer_writes_the_target_and_keeps_the_link() {
        for (name, atomic) in [("atomic", true), ("no-atomic", false)] {
            let target = temp_path(&format!("writer-{name}-missing"));
            let link = temp_path(&format!("writer-{name}-link"));
            let _ = fs::remove_file(&target);
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(&target, &link).expect("dangling symlink");
            let mut writer = CliWriter::file(link.to_str().expect("utf-8"), atomic)
                .unwrap_or_else(|error| panic!("open through link: {error}"));
            writer.write_all(b"{\"a\":1}\n").expect("write");
            writer.commit().unwrap_or_else(|error| panic!("commit: {error}"));
            let link_meta = fs::symlink_metadata(&link).expect("link remains");
            assert!(
                link_meta.file_type().is_symlink(),
                "{name}: the destination stays a symlink"
            );
            assert_eq!(
                fs::read(&target).expect("target created"),
                b"{\"a\":1}\n",
                "{name}: bytes land on the intended target"
            );
            let _ = fs::remove_file(&target);
            let _ = fs::remove_file(&link);
        }
    }

    /// A live symlink still updates the target, not the link.
    #[cfg(unix)]
    #[test]
    fn live_symlink_output_replaces_the_target_not_the_link() {
        let target = temp_path("live-target");
        let link = temp_path("live-link");
        let _ = fs::remove_file(&target);
        let _ = fs::remove_file(&link);
        fs::write(&target, b"old\n").expect("target writes");
        std::os::unix::fs::symlink(&target, &link).expect("live symlink");
        write_output_bytes(b"new\n", Some(link.as_path()), false)
            .unwrap_or_else(|error| panic!("atomic write through live symlink: {error}"));
        let link_meta = fs::symlink_metadata(&link).expect("link remains");
        assert!(
            link_meta.file_type().is_symlink(),
            "a live symlink is not replaced by a regular file"
        );
        assert_eq!(fs::read(&target).expect("target"), b"new\n");
        let _ = fs::remove_file(&target);
        let _ = fs::remove_file(&link);
    }

    /// A FAILED commit must leave no `.jqf-tmp-{pid}` residue: the temp slot stays filled until the rename succeeds, so
    /// the Drop guard removes what the failure abandoned.
    #[cfg(unix)]
    #[test]
    fn a_failed_commit_removes_its_temp_file() {
        let dir = std::env::temp_dir().join(format!(
            "jqf-output-commit-fail-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::create_dir_all(&dir).expect("fixture dir");
        // A DIRECTORY as the atomic destination: create/fsync succeed but the final rename cannot replace a directory,
        // which is the failure arm.
        let dest = dir.join("destination-dir");
        fs::create_dir_all(&dest).expect("destination directory");
        let mut writer =
            CliWriter::file(dest.clone(), true).unwrap_or_else(|error| panic!("open atomic writer: {error}"));
        writer.write_all(b"bytes").expect("write");
        assert!(writer.commit().is_err(), "renaming over a directory fails");
        let residue = fs::read_dir(&dir)
            .expect("list fixture dir")
            .filter_map(std::result::Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with(".jqf-tmp-"));
        assert!(!residue, "a failed commit left its temp file behind");
        let _ = fs::remove_dir_all(&dir);
    }
}
