//! The resident streaming feed (pull-buffer): the record route fed incrementally.
//!
//! One feed is ONE program over ONE framed record stream whose input arrives in pieces. `try_push` appends bytes and
//! recomputes the completed record range (the NDJSON framer's exact cut: a record is complete only after its physical
//! terminator — everything after the last line feed is HELD, never framed, never faulted, until more bytes arrive);
//! `poll` runs the program's compiled requirement over ONE batch of the completed range through [`drive_record_range`]
//! — the ONE record drive in the process, unchanged — and publishes the batch into a caller-provided buffer with the
//! `snprintf` required-size convention (a too-small buffer is a detectable re-call, and re-calling `poll` re-delivers
//! the SAME batch, never the next one).
//!
//! The batch bound is the record route's own [`jqf_sdk::RECORD_BATCH_ENTRIES`] / [`jqf_sdk::RECORD_BATCH_TARGET_BYTES`]
//! pair — not a new number. The feed finds each batch's boundary with the codec's OWN framer (one `poll` of a fresh
//! record provider over the completed-but-unpublished range with the record route's limit), so the framing law is never
//! duplicated: the boundary poll frames with the exact `frame_at` classification, and the drive then re-frames the same
//! bytes with the same law.
//!
//! # The failure law
//!
//! A strict-profile fault is TERMINAL: the faulting record stays in the retained input (never drained, never
//! published), the failure is recorded on the diagnostics, and every later `poll` reports [`FeedPoll::Failed`]. Like
//! the framer's own deferral law, a fault discovered after earlier records were already framed does not discard them:
//! the batch containing them is delivered first, and the death is reported by the NEXT `poll`. A per-VALUE runtime
//! error is NOT terminal: the record drive reports it to the sink (recorded as a diagnostic) and keeps going — the
//! whole-input record route's stream-continues law, carried across the feed's batch boundaries. A recovering profile
//! turns framing and payload faults into ordered issues (recorded as diagnostics) and continues after the next physical
//! line feed, exactly as the record route does.
//!
//! # Residency
//!
//! The retained input is a [`Vec<u8>`] counted by the ambient allocator: `new` establishes it, every `try_push` grows
//! it, and dropping the feed releases it. The published prefix is drained after each delivered batch, so a long feed's
//! retained input stays bounded by the current batch plus the held partial record — the same compacting cycle the
//! `--follow` route keeps. A feed can never outlive its engine: dropping the handle drops every live feed with it,
//! releasing the retained residency while the account is still alive.

use crate::records::{
    RecordDriveError, RecordDriveSpec, RecordInputKind, RecordOutputSpec, RecordRunModel, drive_record_range,
};
use jqf_codec_core::RecordIssueSeverity;
use jqf_codec_json::{
    JsonEncodeOptions,
    ndjson::{NdjsonProfile, NdjsonTerminator},
};
use jqf_engine::CompiledProgram;
use jqf_resource::{
    ResourceContext, ResourceError, ResourceLimit, ResourceLimits, UsageSnapshot,
    diag::{DiagnosticRecord, DiagnosticSink, Severity, codes},
};
use jqf_sdk::{
    Diagnostics, EncodedItemReport, ItemSink, PipelineFailure, RECORD_BATCH_ENTRIES, RECORD_BATCH_TARGET_BYTES,
    RecordIssueReport, RuntimeMismatchClass, SequenceValueError, is_per_value_codec_kind,
};

/// One framing codec's stable issue-text table: the codec's own spelling for each shared issue code
/// (`jqf_codec_json::ndjson::issue_text` and its siblings), handed in by the host because the runtime links no codec.
pub type RecordIssueText = fn(jqf_codec_core::RecordIssueCode) -> (&'static str, &'static str);

/// Cooperative credits installed on the feed's boundary-poller resumes, the same budget the SDK's record sequence
/// installs on its own.
const FEED_COOPERATIVE_CREDITS: u32 = 64;

/// Whether a record drive's pipeline failure is a per-VALUE error — the class the feed continues past across its batch
/// boundaries — rather than a terminal framing, decode, halt, sink, or setup failure.
///
/// This is the record route's own classification (the `--follow` route keeps the same match); the feed needs it because
/// a resident stream has no "run end" for the drive's last-record-law `Err` to mean. The per-value class leaves the
/// feed alive; everything else is [`FeedPoll::Failed`].
pub fn is_per_value_failure<SinkError>(failure: &PipelineFailure<SinkError>) -> bool {
    matches!(
        failure,
        PipelineFailure::TypeMismatch { .. }
            | PipelineFailure::IterateMismatch { .. }
            | PipelineFailure::ObjectKeyMismatch { .. }
            | PipelineFailure::NoLength { .. }
            | PipelineFailure::NoKeys { .. }
            | PipelineFailure::ArithmeticError(_)
            | PipelineFailure::SliceIndices
            | PipelineFailure::MismatchRaised { .. }
            | PipelineFailure::EngineCardinality { .. }
            | PipelineFailure::Raised(_)
    ) || matches!(
        failure,
        // The ONE admitted codec kind is the SDK's shared admission test (`jqf_sdk::is_per_value_codec_kind`) — this
        // route does not restate which kind qualifies.
        PipelineFailure::Codec(error) if is_per_value_codec_kind(error.kind())
    )
}

/// One feed poll's outcome, in the ABI's own terms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedPoll {
    /// One batch's output is available. The payload is its full length (the `snprintf`-required size, possibly larger
    /// than the caller's buffer, which received only the first `out.len()` bytes). Re-polling with a larger buffer
    /// re-delivers the SAME batch.
    Batch(usize),
    /// No output available and the feed is healthy: nothing is completed and unpublished. The host pushes more input
    /// and polls again.
    Empty,
    /// The feed is terminally failed: the ABI reports `-1`. The failure was recorded on the diagnostics when it
    /// happened; a later poll reports it again without touching the stream, so the death record stays readable.
    Failed,
}

/// What one route/cost emission described: the resource snapshot the cost row carried, plus the drive position (records
/// consumed) at that moment. A batch publication always advances the record count, so every real transition differs
/// from the last emission; an idle poll moves neither field unless resource use itself moved.
#[derive(Clone, Copy, PartialEq)]
struct LastEmitted {
    usage: UsageSnapshot,
    stream_records: u64,
}

/// The resident feed state machine.
pub struct ResidentFeed {
    /// The framing profile selected at open, like the record route's request build: strict faults are terminal,
    /// recovering faults are ordered issues.
    profile: NdjsonProfile,
    /// The process-lifetime catalog of the record formats the feed serves catalog, installed once through
    /// [`crate::records::install_record_catalog`].
    catalog: jqf_sdk::CodecCatalog<'static, 'static>,
    /// The framing codec's own issue text. A format's spelling is the format's to own, and the runtime links no codec,
    /// so the host hands the framer's table in exactly as it hands in the catalog.
    issue_text: RecordIssueText,
    /// The retained (unpublished) input, charged to the request ledger.
    retained: Vec<u8>,
    /// End of the last COMPLETE record (the cut at the last line feed), as an index into `retained`. Everything at or
    /// after it is a held partial record that must never be framed, faulted, or emitted.
    complete_end: usize,
    /// Index into `retained` of the first record not yet published. Always at most `complete_end`.
    published_end: usize,
    /// The host called [`Self::finish`]: the stream's end is known, the held tail is a FINAL record, and further pushes
    /// are accepted-and-ignored.
    finishing: bool,
    /// The current batch's published output, pending the caller's buffer. A batch that did not fit the caller's buffer
    /// stays here so the next poll re-delivers it; a poll never skips ahead to the next batch while one is pending.
    staged: Option<Vec<u8>>,
    /// The recycled staging capacity of the last fully delivered batch. A resident host draining a long stream would
    /// otherwise pay one allocation plus growth reallocs per batch for the feed's lifetime; handing the delivered
    /// buffer back here keeps the high-water capacity allocated exactly once.
    staging_pool: Vec<u8>,
    /// The feed hit its terminal failure; every poll reports `-1` once any staged batch is fully delivered.
    terminal: bool,
    /// Retained-input ceiling. `usize::MAX` is unlimited (the FFI host that cannot yet hand a ledger). Configurable via
    /// [`Self::with_max_retained_bytes`] / [`Self::with_limits`].
    max_retained_bytes: usize,
    /// Stream-absolute byte position of `retained[0]`: every delivered and compacted prefix adds here, so issue offsets
    /// re-base to the stream the host is following, not to the undelivered window.
    stream_base: u64,
    /// Ordinals delivered so far, for the same stream-absolute re-basing.
    stream_records: u64,
    /// The drive state the last route/cost emission described. A poll whose state equals this emits nothing:
    /// steady-state idle polling must format no diagnostic records at all.
    last_emitted: Option<LastEmitted>,
}

impl ResidentFeed {
    /// Opens a feed with an empty retained-input buffer; the ambient allocator grows it with the first `push`. The
    /// catalog is the process- lifetime record catalog, and `issue_text` is the framing codec's own message table.
    #[must_use]
    pub fn new(
        profile: NdjsonProfile,
        catalog: jqf_sdk::CodecCatalog<'static, 'static>,
        issue_text: RecordIssueText,
    ) -> Self {
        Self {
            profile,
            catalog,
            issue_text,
            retained: Vec::new(),
            complete_end: 0,
            published_end: 0,
            finishing: false,
            staged: None,
            staging_pool: Vec::new(),
            terminal: false,
            max_retained_bytes: usize::MAX,
            stream_base: 0,
            stream_records: 0,
            last_emitted: None,
        }
    }

    /// Caps retained input at `bytes`. A push that would grow past the cap is refused; the held partial record is kept.
    #[must_use]
    pub fn with_max_retained_bytes(mut self, bytes: u64) -> Self {
        self.max_retained_bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        self
    }

    /// Caps retained input at the tighter of the request's input and memory ceilings. Until the FFI host can install a
    /// ledger, this is how a `JqfLimits` ceiling bounds a feed.
    #[must_use]
    pub fn with_limits(self, limits: ResourceLimits) -> Self {
        self.with_max_retained_bytes(limits.max_input_bytes().min(limits.max_memory_bytes()))
    }

    /// Whether the feed has a pending batch not yet fully delivered.
    #[must_use]
    pub const fn has_pending_batch(&self) -> bool {
        self.staged.is_some()
    }

    /// Whether the feed is terminally failed.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Fallible push: reserve before extend, refuse past the configured cap.
    ///
    /// The completed-record cut is scanned in the appended piece only, so cost is linear in the piece, not in the
    /// retained buffer.
    ///
    /// # Errors
    ///
    /// Returns a memory-limit error when the push would grow the retained buffer past the configured cap, or an
    /// allocation failure when the reserve itself is refused.
    pub fn try_push(&mut self, bytes: &[u8]) -> Result<usize, ResourceError> {
        if self.finishing {
            // The host said EOF; there is no reopening. The push is accepted and ignored so a late write cannot
            // resurrect a finished stream.
            return Ok(self.retained.len());
        }
        let added = bytes.len();
        let new_len = self.retained.len().saturating_add(added);
        if new_len > self.max_retained_bytes {
            return Err(ResourceError::LimitExceeded {
                limit_kind: ResourceLimit::InputBytes,
                limit: self.max_retained_bytes as u64,
                current: self.retained.len() as u64,
                requested_delta: added as u64,
            });
        }
        self.retained
            .try_reserve(added)
            .map_err(|_| ResourceError::AllocationFailed)?;
        let old_len = self.retained.len();
        self.retained.extend_from_slice(bytes);
        // The last line feed in THIS piece is the new complete cut. A piece with no LF leaves the previous cut in place
        // — no whole-buffer scan.
        if let Some(pos) = bytes.iter().rposition(|&byte| byte == b'\n') {
            self.complete_end = old_len.saturating_add(pos).saturating_add(1);
        }
        Ok(self.retained.len())
    }

    /// Marks the end of the input stream. The held partial record (if any) becomes the stream's FINAL record and is
    /// delivered by subsequent polls under the profile's own tail law — exactly the whole-input route's answer over the
    /// same bytes: a complete final value without its terminator is accepted under BOTH profiles (JSON Lines permits
    /// it), and a truncated one is a strict fault or a recovering issue. Unlike a terminal failure this is a CLEAN end
    /// of stream: polls drain to [`FeedPoll::Empty`], never to `Failed`, and later pushes are ignored.
    pub fn finish(&mut self) {
        if self.terminal {
            return;
        }
        self.finishing = true;
        // Every remaining byte is now in range for the batch walk; the drive's own framer applies the profile's tail
        // law to whatever is held, so the framing decision is never duplicated here.
        self.complete_end = self.retained.len();
    }

    /// Runs the feed's next poll: deliver any pending batch, compute the next one (one record batch through the
    /// ordinary record drive), and publish it into `out` under the `snprintf` convention.
    pub fn poll(
        &mut self,
        program: &CompiledProgram,
        resources: &mut ResourceContext<'_>,
        diagnostics: &Diagnostics,
        out: &mut [u8],
    ) -> FeedPoll {
        // 1. A pending batch is delivered before anything else: re-polling
        // with a larger buffer must return the SAME batch, never the next one, and a death discovered while computing
        // it is reported only after its batch is fully delivered (the framer's own deferral law).
        if self.staged.is_some() {
            return self.deliver_pending(out);
        }
        if self.terminal {
            return FeedPoll::Failed;
        }
        // 2. Compute the next batch. `complete_end` is at a record boundary,
        // so any completed-but-unpublished range holds at least one record.
        let Some((batch_end, batch_records)) = self.next_batch_end() else {
            // No completed records: a healthy, idle poll. The route/cost pair emits only when the drive state moved
            // since the last emission (see `emit_route_and_cost`), so a steady-state poll formats no diagnostic
            // records.
            self.emit_route_and_cost(diagnostics, resources);
            return FeedPoll::Empty;
        };
        let bytes = &self.retained.as_slice()[self.published_end..batch_end];
        // Issue ordinals and offsets are re-based to the STREAM before the drive runs: the codec reports them relative
        // to the undelivered range it was handed, but a live host's window into the stream is the absolute position
        // (the follow route's own re-basing law).
        let offset_base = self.stream_base.saturating_add(self.published_end as u64);
        let ordinal_base = self.stream_records;
        let spec = RecordDriveSpec {
            input: bytes,
            source_name: "<feed>",
            files: None,
            kind: RecordInputKind::Ndjson,
            profile: self.profile,
            json_seq_profile: jqf_codec_json::seq::JsonSeqProfile::Strict,
            csv_delimiter: None,
            csv_textdata: false,
            // A record cannot exceed the retained buffer it arrived in, so the retained cap IS this feed's per-record
            // ceiling.
            max_record_bytes: self.max_retained_bytes as u64,
            catalog: self.catalog,
            output: RecordOutputSpec {
                target: crate::output::OutputTarget::Json,
                terminator: NdjsonTerminator::Lf,
                json: JsonEncodeOptions::default(),
                no_newline: false,
            },
            model: RecordRunModel::PerRecord,
            edit: false,
            cooperative_credits: FEED_COOPERATIVE_CREDITS,
            // The feed surface carries no iteration dial; a host that needs one drives the request through the request
            // API instead.
            max_iterations: None,
        };
        let mut sink = FeedSink {
            // Reuse the last delivered batch's staging capacity; a fresh buffer is only built on the very first batch.
            bytes: std::mem::take(&mut self.staging_pool),
            diagnostics,
            issue_text: self.issue_text,
            byte_cap: self.max_retained_bytes,
            item_bytes: 0,
            current_item: 0,
            offset_base,
            ordinal_base,
        };
        let outcome = drive_record_range(spec, bytes, program, resources, &mut sink, None);
        // The feed CONTINUES for exactly two outcomes: a clean completion, and a per-VALUE runtime error the drive
        // reported through the sink (recorded as a diagnostic) while processing the whole batch — that `Err` is only
        // the last-record law, which a resident stream has no run end for. Both publish the batch and mirror a
        // successful run's record set (route + cost).
        let batch_completed = match &outcome {
            Ok(_) => true,
            Err(RecordDriveError::Pipeline(failure)) => match failure {
                jqf_sdk::Failure::Pipeline(pipeline) => is_per_value_failure(pipeline.failure()),
                _ => false,
            },
            Err(_) => false,
        };
        if batch_completed {
            self.published_end = batch_end;
            self.stream_records = self.stream_records.saturating_add(batch_records);
            self.staged = Some(sink.bytes);
            // The batch advanced the drive, so this always emits (see `emit_route_and_cost`).
            self.emit_route_and_cost(diagnostics, resources);
            self.deliver_pending(out)
        } else {
            let Err(error) = outcome else {
                unreachable!("a clean completion always takes the continuing branch");
            };
            // A terminal failure: a strict payload fault, a program halt, or a setup failure. The drive published every
            // record before the failing one; those bytes are delivered as this batch, and the death is reported by the
            // next poll. The faulting record stays in the retained input.
            let bytes = sink.bytes;
            self.terminal = true;
            record_drive_failure(diagnostics, &error);
            if bytes.is_empty() {
                self.recycle_staging(bytes);
                return FeedPoll::Failed;
            }
            self.staged = Some(bytes);
            self.deliver_pending(out)
        }
    }

    /// Emits the drive's route+cost diagnostic pair exactly when the drive state moved past the last emission (the
    /// first emission included): the route row names the lane and the cost row carries this moment's resource snapshot,
    /// byte-shaped as any ungated emission. An unchanged state — the steady-state idle poll — formats neither record,
    /// so repeated polling over an unchanged feed allocates nothing diagnostic.
    fn emit_route_and_cost(&mut self, diagnostics: &Diagnostics, resources: &ResourceContext<'_>) {
        let state = LastEmitted {
            usage: resources.snapshot(),
            stream_records: self.stream_records,
        };
        if self.last_emitted == Some(state) {
            return;
        }
        diagnostics.record_route_named("record");
        diagnostics.record_cost(&state.usage);
        self.last_emitted = Some(state);
    }

    /// Delivers the pending batch into `out`, clearing it when it fits.
    fn deliver_pending(&mut self, out: &mut [u8]) -> FeedPoll {
        let Some(staged) = &self.staged else {
            return if self.terminal {
                FeedPoll::Failed
            } else {
                FeedPoll::Empty
            };
        };
        let required = staged.len();
        let written = required.min(out.len());
        out[..written].copy_from_slice(&staged[..written]);
        if required <= out.len() {
            // Fully delivered: recycle the staging capacity and, for a LIVING feed, drain the published prefix so the
            // retained input stays bounded by the current batch plus the held tail. A dead feed keeps its retained
            // input: the faulting record is retained, never drained.
            if let Some(buffer) = self.staged.take() {
                self.recycle_staging(buffer);
            }
            if !self.terminal {
                self.compact();
            }
        }
        FeedPoll::Batch(required)
    }

    /// Takes back a delivered batch's staging buffer, cleared but with its capacity intact, for the next batch to
    /// reuse.
    fn recycle_staging(&mut self, mut buffer: Vec<u8>) {
        buffer.clear();
        self.staging_pool = buffer;
    }

    /// Drops the published prefix of the retained input, re-basing the completed range. The drained bytes were already
    /// delivered; the held partial record and every unpublished record shift to the front.
    fn compact(&mut self) {
        let published = self.published_end;
        if published == 0 {
            return;
        }
        self.retained.drain(..published);
        self.stream_base = self.stream_base.saturating_add(published as u64);
        self.complete_end = self.complete_end.saturating_sub(published);
        self.published_end = 0;
    }

    /// Finds the end of the next batch of completed NDJSON records by walking line feeds up to the record-route batch
    /// bound. That is the framer's own cut, so the drive that follows re-frames the same range instead of a second
    /// provider being opened just to learn an offset. Returns the batch's end and its RECORD COUNT (the finalize tail
    /// is one record) — the ordinal base the next batch's issue reports add. Returns `None` when no record is completed
    /// and unpublished.
    fn next_batch_end(&mut self) -> Option<(usize, u64)> {
        let start = self.published_end;
        let end = self.complete_end;
        if start >= end {
            return None;
        }
        let slice = &self.retained.as_slice()[start..end];
        // NDJSON records end at the first line feed. Walking LFs up to the record-route batch bound is the framer's own
        // cut, so the drive that follows re-frames the same range instead of a second provider being opened just to
        // learn an offset.
        let mut records = 0u64;
        let mut last = 0usize;
        for offset in memchr::memchr_iter(b'\n', slice) {
            last = offset + 1;
            records += 1;
            let payload = last as u64;
            if records >= u64::from(RECORD_BATCH_ENTRIES) || payload >= RECORD_BATCH_TARGET_BYTES {
                break;
            }
        }
        if last == 0 {
            if !self.finishing {
                return None;
            }
            // The finalize pass: no line feed left in [start..end) means the whole remaining tail is ONE final
            // unterminated record. The drive's own framer decides it — accepted when complete (both profiles), an
            // ordered issue when recovering-truncated, terminal when strict-truncated — never a second framing law
            // here.
            return Some((end, 1));
        }
        Some((start.saturating_add(last), records))
    }
}

/// Records one record drive's failure on the diagnostics, mirroring the record route's own failure classes: a setup
/// failure is `MACHINE_SETUP`, a pipeline failure is the failure record the run entry points retain.
fn record_drive_failure<E: core::fmt::Display>(diagnostics: &Diagnostics, error: &RecordDriveError<E>) {
    match error {
        RecordDriveError::Setup { step, error } => {
            // Display, never Debug: a diagnostic payload is host-visible words, not Rust syntax soup.
            diagnostics.record_setup_failure(&format!("feed setup failure at {step}: {error}"));
        }
        RecordDriveError::Pipeline(failure) => {
            // A SINK failure's own text IS the diagnosis (the batch cap names its record, progress, and limit):
            // recording the bare structured MACHINE_SINK row would erase it. Every other pipeline failure keeps the
            // structured mapping.
            match failure.pipeline_failure() {
                Some(jqf_sdk::PipelineFailure::Sink(message)) => {
                    diagnostics.record_setup_failure(&format!("feed output sink failed: {message}"));
                }
                Some(pipeline) => diagnostics.record_failure(pipeline),
                None => diagnostics.record_setup_failure(&failure.to_string()),
            }
        }
        RecordDriveError::Sink(error) => {
            diagnostics.record_setup_failure(&format!("feed output sink failed: {error}"));
        }
        RecordDriveError::Resource(error) => {
            diagnostics.record_setup_failure(&format!("feed resource refusal: {error}"));
        }
        RecordDriveError::Control(error) => {
            diagnostics.record_setup_failure(&format!("feed control stop: {error}"));
        }
    }
}

/// The feed's output sink: collects the batch's published bytes and mirrors the record route's ordered issues and
/// per-value errors onto the diagnostics (the feed has no stderr channel, so the stream is the host's only window into
/// them).
///
/// The staged batch is BOUND: a program whose output expands past the retained-input cap fails the batch instead of
/// doubling the feed's resident footprint outside every account. Issues are re-based to stream-absolute ordinals and
/// offsets before recording.
///
/// A single record whose ENCODED output alone breaches the cap is refused here rather than split across polls: the poll
/// contract ("re-calling re-delivers the SAME batch") makes progressive delivery unobservable to a host following it —
/// a host that grew its buffer after a too-large required count would receive continuation bytes instead of the batch
/// head it was promised. The refusal is the honest terminal, and the diagnostic names the record, its progress into the
/// cap, and the cap itself so a host can raise its retained-input ceiling deliberately.
struct FeedSink<'a> {
    bytes: Vec<u8>,
    /// The staged batch's ceiling (the retained-input cap): the same dial that bounds what the feed may hold bounds
    /// what one batch may emit.
    byte_cap: usize,
    /// Bytes written toward the CURRENT record: the per-record share of the batch accounting, so a single-record
    /// overflow is diagnosable as such instead of surfacing as an anonymous batch total.
    item_bytes: u64,
    /// The current record's ordinal (`begin_item`'s index).
    current_item: u64,
    /// Stream-absolute values the drive's range-relative reports add to.
    offset_base: u64,
    ordinal_base: u64,
    diagnostics: &'a Diagnostics,
    issue_text: RecordIssueText,
}

impl ItemSink for FeedSink<'_> {
    type Error = String;

    fn begin_item(&mut self, index: u64) -> Result<(), Self::Error> {
        self.item_bytes = 0;
        self.current_item = index;
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        let new_len = self.bytes.len().saturating_add(bytes.len());
        if new_len > self.byte_cap {
            // The refusal NAMES the record, its progress into the cap, and the cap: a host reading the stream can raise
            // its retained-input ceiling deliberately instead of guessing.
            return Err(format!(
                "feed batch output exceeds its {}-byte cap at record {} \
                 ({} bytes into that record's encoded output)",
                self.byte_cap,
                self.current_item,
                self.item_bytes + bytes.len() as u64
            ));
        }
        if self.bytes.try_reserve(bytes.len()).is_err() {
            return Err("feed batch output allocation refused".to_owned());
        }
        self.bytes.extend_from_slice(bytes);
        self.item_bytes += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }

    fn report_value_error(&mut self, error: SequenceValueError) -> Result<(), Self::Error> {
        record_value_error(self.diagnostics, &error);
        Ok(())
    }

    fn report_record_issue(&mut self, issue: RecordIssueReport<'_>) -> Result<(), Self::Error> {
        // The drive numbers issues relative to the undelivered range it was handed; record_issue re-bases them to the
        // stream the host follows.
        record_issue(
            self.diagnostics,
            &issue,
            self.issue_text,
            self.ordinal_base,
            self.offset_base,
        );
        Ok(())
    }
}

/// Records one ordered record-stream issue as a diagnostic: the issue's own ordinal and absolute offset, the framing
/// codec's own text as payload, and the severity the issue carries (an error-severity issue forces the record route's
/// exit class; the feed has no exit class, so the diagnostic's severity carries the same signal). The bases re-base
/// both values to the stream: the drive reports them relative to the undelivered range it was handed, and a live host's
/// window is the absolute position (the follow route's own re-basing law).
fn record_issue(
    diagnostics: &Diagnostics,
    issue: &RecordIssueReport<'_>,
    issue_text: RecordIssueText,
    ordinal_base: u64,
    offset_base: u64,
) {
    let (code, message) = issue_text(issue.code());
    let mut record = DiagnosticRecord::new(codes::MACHINE_INPUT);
    record.kind = Some(code);
    record.severity = match issue.severity() {
        RecordIssueSeverity::Error => Severity::Error,
        RecordIssueSeverity::Advisory => Severity::Warning,
    };
    record.input_ordinal = Some(issue.ordinal().saturating_add(ordinal_base));
    record.byte_offset = Some(issue.offset().saturating_add(offset_base));
    record.payload = Some(message);
    diagnostics.record(record);
}

/// Records one per-value runtime error as a diagnostic, mapped to the raise family its class names (the same codes the
/// uncaught run entry points retain), with the error's own message as payload.
fn record_value_error(diagnostics: &Diagnostics, error: &SequenceValueError) {
    let code = match error.class() {
        RuntimeMismatchClass::Index => codes::RAISE_INDEX,
        RuntimeMismatchClass::Iterate => codes::RAISE_ITERATE,
        RuntimeMismatchClass::ObjectKey => codes::RAISE_OBJECT_KEY,
        RuntimeMismatchClass::NoLength => codes::RAISE_NO_LENGTH,
        RuntimeMismatchClass::NoKeys => codes::RAISE_NO_KEYS,
        RuntimeMismatchClass::Arithmetic => codes::RAISE_ARITHMETIC,
        RuntimeMismatchClass::SliceIndices => codes::RAISE_SLICE_INDICES,
        // The one per-value codec kind (RawNulByte under `--raw-output0`): the machine code for a raw NUL in a root
        // string.
        RuntimeMismatchClass::Codec => codes::MACHINE_RAW_NUL,
        // The strict-dial raise has no message of its own; the SDK renders the cell name into the error's message, and
        // the record carries the same code the uncaught entry point uses — one code for all cells, the name in the
        // payload.
        //
        RuntimeMismatchClass::MismatchRaised => codes::MISMATCH_STRICT,
        RuntimeMismatchClass::EngineCardinality => codes::RAISE_ENGINE_CARDINALITY,
        RuntimeMismatchClass::Raised => codes::RAISE_PROGRAM,
    };
    let mut record = DiagnosticRecord::new(code);
    record.severity = Severity::Error;
    record.payload = Some(error.message());
    diagnostics.record(record);
}

#[cfg(test)]
mod tests {
    use super::is_per_value_failure;
    use crate::records;
    use jqf_codec_core::{DiagnosticPolicy, ValidationMode};
    use jqf_engine::CodecRequirementPolicy;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_sdk::PipelineFailure;

    /// The two variants the whole-input route continues past must be per-value for the feed too, so a strict-mismatch
    /// raise or a cardinality violation on ONE record leaves the resident stream alive.
    #[test]
    fn mismatch_and_cardinality_are_per_value() {
        assert!(is_per_value_failure(&PipelineFailure::<()>::MismatchRaised { cell: 0 }));
        assert!(is_per_value_failure(&PipelineFailure::<()>::EngineCardinality {
            constructor: "generator",
            phase: "init",
        }));
    }

    /// A single record whose ENCODED output alone breaches the retained cap is refused terminally, and the diagnostic
    /// NAMES the record, its progress into the cap, and the cap itself — never an anonymous batch total (the PART law:
    /// refusal over splitting, because the poll contract's re-delivery semantics make progressive delivery unobservable
    /// to a compliant host).
    #[test]
    fn a_single_record_over_the_cap_is_refused_naming_record_and_limit() {
        static CONTROL: ContinueControl = ContinueControl;
        let catalog = records::install_record_catalog(
            jqf_codec_json::registration().expect("static registration"),
            jqf_codec_json::ndjson::registration().expect("static registration"),
            jqf_codec_json::seq::registration().expect("static registration"),
            jqf_codec_delimited::registration().expect("static registration"),
            jqf_codec_delimited::registration_tsv().expect("static registration"),
            jqf_codec_render::registration().expect("static registration"),
            jqf_codec_yaml::registration().expect("static registration"),
            jqf_codec_xml::registration().expect("static registration"),
            jqf_codec_html::registration().expect("static registration"),
        );
        let account = RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("unlimited account");
        let work = WorkMeter::try_new_v1(64).expect("work meter");
        let mut resources = ResourceContext::new(account, &CONTROL, work).expect("context");
        let diagnostics = jqf_sdk::Diagnostics::new(DiagnosticPolicy::All).expect("diagnostics");

        let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
        let compiled =
            jqf_engine::try_compile_program("[range(5000)]", policy, jqf_engine::CompileOptions::new(), &resources)
                .expect("the fixture program compiles");

        // The encoded array `[0,1,…,4999]` is ~25 KB; the cap is 4 KB.
        let mut feed = super::ResidentFeed::new(
            jqf_codec_json::ndjson::NdjsonProfile::Strict,
            catalog,
            jqf_codec_json::ndjson::issue_text,
        )
        .with_max_retained_bytes(4096);
        feed.try_push(b"null\n").expect("under the cap");
        let outcome = feed.poll(&compiled, &mut resources, &diagnostics, &mut []);
        assert!(
            matches!(outcome, super::FeedPoll::Failed),
            "a single record over the cap is terminal, got {outcome:?}"
        );
        let texts: Vec<String> = diagnostics
            .records()
            .iter()
            .filter_map(|record| record.payload().map(str::to_owned))
            .collect();
        assert!(
            texts
                .iter()
                .any(|text| text.contains("exceeds its 4096-byte cap at record 0")),
            "the refusal must name the record and the limit, got {texts:?}"
        );
    }
}
