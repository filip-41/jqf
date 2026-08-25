//! The cursor drive family: the shared input cursor behind the input-family drives.

use super::{
    Cell, CodecError, CodecInputOutcome, CodecInputResult, DIRECT_JSON_DIALECT, DIRECT_JSON_FORMAT, DialectId,
    EngineResult, FormatId, InputLines, InputSource, PipelineError, PipelineFailure, Publication, RefCell,
    ResolvedSource, ResourceContext, ReusableAccessSession, STREAMING_READ_CHUNK, String, Value, Vec,
    allocation_failure, decode_sequence_item, overflow, parser_machine_error, require_forward_progress,
    skip_value_separator,
};

/// The shared input cursor an input-family program reads: every adjacent value
/// decoded up front (jqf's whole-buffer model), with its line number, served
/// through the engine's [`jqf_engine::InputSource`] seam.
///
/// The cursor is the WHOLE sequence: the drive pulls the current value from it
/// (then marks it current), and the program's `input`/`inputs` pull from the
/// same cursor, so `jq '., inputs'` consumes every remaining value exactly as
/// the reference's shared input stream does. Values are `take`n out (no clone), which is
/// why the interior is a `RefCell<Vec<Option<Value>>>`.
pub(crate) struct OwnedInputCursor {
    pub(crate) values: RefCell<Vec<Option<Value>>>,
    pub(crate) lines: Vec<u64>,
    /// The single source label every value reports, for a non-multi-file
    /// input. Unused when `filenames` is non-empty.
    pub(crate) filename: Option<String>,
    /// Input filename runs for a multi-file concatenation: one
    /// `(first_value_index, label)` per file boundary, sorted by index and
    /// starting at 0. A value's filename is the label of the last run at or
    /// before it. Empty for a single source, where every value reports
    /// `filename`.
    pub(crate) filenames: Vec<(usize, String)>,
    pub(crate) next: Cell<usize>,
    /// The index of the CURRENT input value, `None` until one has been pulled
    /// (the reference's pre-read state: `-n 'input_filename'` answers `null` and
    /// `input_line_number` `0` before any `input` pull).
    pub(crate) current: Cell<Option<usize>>,
    /// How many times the program pulled from this cursor, successful or not —
    /// the reference's "input was touched" signal the null-first drives read
    /// after the run to decide between an UNKNOWN error location (`-n '1|.b'`,
    /// never pulled) and the `break`-on-empty `<stdin>:0` (pulled, found
    /// nothing).
    pub(crate) pulls: Cell<u64>,
}

impl jqf_engine::InputSource for OwnedInputCursor {
    fn next(
        &mut self,
        _resources: &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<Option<Value>, jqf_engine::InputSourceError> {
        self.pulls.set(self.pulls.get().saturating_add(1));
        let index = self.next.get();
        if index >= self.values.borrow().len() {
            return Ok(None);
        }
        self.next.set(index + 1);
        Ok(self.values.borrow_mut().get_mut(index).and_then(Option::take))
    }

    fn current_filename(&self) -> Option<&str> {
        let current = self.current.get()?;
        if self.filenames.is_empty() {
            self.filename.as_deref()
        } else {
            // Runs start at 0, so the last run at or before `current` always
            // exists; binary search keeps per-pull lookup off the table size.
            let run = self.filenames.partition_point(|(start, _)| *start <= current);
            Some(self.filenames[run - 1].1.as_str())
        }
    }

    fn current_line(&self) -> u64 {
        self.current
            .get()
            .and_then(|current| self.lines.get(current))
            .copied()
            .unwrap_or(0)
    }

    fn mark_current(&self) {
        self.current.set(Some(self.next.get().saturating_sub(1)));
    }

    fn pulls(&self) -> u64 {
        self.pulls.get()
    }
}

/// The eagerly decoded adjacent-value table the shared-cursor drives serve
/// from: every value plus the line its end falls on. Shared by
/// [`execute_input_sequence`] and the two single-run drives so the three input
/// models can never disagree about which bytes decode (jqf's stop-on-error
/// whole-buffer law).
pub(crate) struct EagerSequence {
    pub(crate) values: Vec<Option<Value>>,
    pub(crate) lines: Vec<u64>,
    /// Input filename runs for a multi-file concatenation, EMPTY for a single
    /// source (where every value's filename is the source label). One
    /// `(first_value_index, label)` per file boundary — the label changes
    /// only at boundaries, so one run is materialized per file, never one
    /// entry per value.
    pub(crate) filenames: Vec<(usize, String)>,
}

impl EagerSequence {
    /// Wraps these decoded values as the shared input cursor, positioning it at
    /// `next`/`current`. A single source reports the source label for every
    /// value; a multi-file concatenation reports each value's ending file.
    /// `current` is `None` when nothing has been pulled yet (the null-first
    /// drive's pre-read state).
    pub(crate) fn cursor(self, source: ResolvedSource<'_>, next: usize, current: Option<usize>) -> OwnedInputCursor {
        OwnedInputCursor {
            values: RefCell::new(self.values),
            lines: self.lines,
            filename: if self.filenames.is_empty() {
                Some(std::string::ToString::to_string(&source.label()))
            } else {
                None
            },
            filenames: self.filenames,
            next: Cell::new(next),
            current: Cell::new(current),
            pulls: Cell::new(0),
        }
    }
}

/// Materializes one decoded sequence item's outcome into its owned VALUE,
/// exactly as the eager sequence decode does (Owned passes through, Located
/// materializes the node; any other outcome is an internal contract
/// violation).
pub(crate) fn materialize_sequence_value<E>(
    engine: CodecInputResult<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Value, PipelineError<E>> {
    let (codec_outcome, _access_report) = engine.into_parts();
    match codec_outcome {
        CodecInputOutcome::Result(result) => match result {
            EngineResult::Owned(value) => Ok(value),
            EngineResult::Located(located) => located
                .product()
                .document()
                .materialize_node(located.node(), resources)
                .map_err(|_| {
                    publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "input-sequence value materialization",
                        },
                    )))
                }),
        },
        _other => Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "input-sequence decode produced a non-result outcome",
            },
        )))),
    }
}

/// Records one value's ending-file label in the eager sequence's filename-run
/// table, opening a new `(first_value, label)` run only when the label differs
/// from the last recorded one — the label changes only at file boundaries, so
/// one heap String is materialized per FILE, never per value.
pub(crate) fn push_filename_run<E>(
    filenames: &mut Vec<(usize, String)>,
    first_value: usize,
    label: &str,
    publication: &Publication,
) -> Result<(), PipelineError<E>> {
    if filenames.last().is_some_and(|(_, existing)| existing == label) {
        return Ok(());
    }
    filenames
        .try_reserve(1)
        .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
    filenames.push((first_value, String::from(label)));
    Ok(())
}

/// Decodes every adjacent value of `source` into an [`EagerSequence`],
/// stopping on the first decode failure before anything is published.
#[allow(
    clippy::too_many_arguments,
    reason = "the eager sequence decoder threads the same boundary inventory the SDK drives do;               splitting it would hand fragments the stop-on-error law that only holds end to end"
)]
pub(crate) fn decode_eager_sequence<'source, E>(
    provider: &mut jqf_codec_core::ErasedProvider<'source>,
    reuse: &mut ReusableAccessSession<'source>,
    handle: &jqf_codec_core::AccessHandle<'_>,
    source: ResolvedSource<'_>,
    files: Option<&[jqf_source::SourceFileRange<'_>]>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    credits: u32,
    separator: &[u8],
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<EagerSequence, PipelineError<E>> {
    // The DEFAULT adjacent-JSON dialect decodes value-direct through the
    // engine's reference-faithful reader — the same acceptor the streaming
    // input-family lane pulls through — skipping the per-value codec access
    // session with its document build and materialization (the tiny-record
    // decode floor: on ~300-byte NDJSON records that session is ~90% of the
    // whole run). Only a CLEAN parse takes the lane: any refusal, and any
    // non-JSON or non-UTF-8 request, falls through to the codec decode below,
    // so which bytes decode and how failures report stay exactly the standing
    // path's.
    if input_format.as_str() == DIRECT_JSON_FORMAT
        && input_dialect.as_str() == DIRECT_JSON_DIALECT
        && let Ok(text) = core::str::from_utf8(source.bytes())
        && let Some(sequence) = decode_eager_sequence_direct(text, source.bytes(), files, resources, publication)?
    {
        return Ok(sequence);
    }
    let mut values = Vec::new();
    let mut lines = Vec::new();
    let mut filenames = Vec::new();
    let mut offset = 0usize;
    let mut line_scan = InputLines::with_files_or_new(files);
    loop {
        let start = skip_value_separator(source.bytes(), offset, separator);
        if start >= source.bytes().len() {
            break;
        }
        let start_offset = u64::try_from(start).map_err(|_| overflow::<E>(publication))?;
        let engine = decode_sequence_item(provider, reuse, handle, start_offset, credits, resources, publication)?;
        let consumed = require_forward_progress::<E>(engine.report().consumed_offset(), publication)?;
        let (codec_outcome, _access_report) = engine.into_parts();
        let value = match codec_outcome {
            CodecInputOutcome::Result(result) => match result {
                EngineResult::Owned(value) => value,
                EngineResult::Located(located) => located
                    .product()
                    .document()
                    .materialize_node(located.node(), resources)
                    .map_err(|_| {
                        publication.fail(PipelineFailure::Codec(CodecError::new(
                            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                                contract: "input-sequence value materialization",
                            },
                        )))
                    })?,
            },
            _other => {
                return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                    jqf_codec_core::CodecFailureKind::InternalContractViolation {
                        contract: "input-sequence decode produced a non-result outcome",
                    },
                ))));
            }
        };
        let consumed_usize = usize::try_from(consumed).map_err(|_| overflow::<E>(publication))?;
        let end = start
            .checked_add(consumed_usize)
            .ok_or_else(|| overflow::<E>(publication))?;
        lines.push(line_scan.at_value_end(source.bytes(), end));
        if let Some(label) = line_scan.current_file_label() {
            push_filename_run::<E>(&mut filenames, values.len(), label, publication)?;
        }
        values
            .try_reserve(1)
            .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
        values.push(Some(value));
        offset = end;
    }
    Ok(EagerSequence {
        values,
        lines,
        filenames,
    })
}

/// Decodes a complete adjacent-JSON source value-direct through the engine's
/// reference-faithful reader ([`jqf_engine::json_stream_next`] with the stream
/// finished), producing the same cursor table [`decode_eager_sequence`]'s
/// codec path builds: every value in order, each with the line its end falls
/// on (and its ending file's label for a multi-file concatenation).
///
/// Answers `Ok(None)` on any parse refusal — the caller reruns the codec
/// decode from the start, so a malformed source stops before any run and
/// reports exactly as it always has. The lane changes only the cost of
/// success.
pub(crate) fn decode_eager_sequence_direct<E>(
    text: &str,
    source_bytes: &[u8],
    files: Option<&[jqf_source::SourceFileRange<'_>]>,
    resources: &ResourceContext<'_>,
    publication: &Publication,
) -> Result<Option<EagerSequence>, PipelineError<E>> {
    // One stream-start BOM is stream metadata, stripped uncounted (the
    // `json_sequence` law); the reader would fault on it as token bytes. The
    // strip shifts parse offsets off the raw bytes, so the line scan adds the
    // BOM's width back.
    let stripped = text.strip_prefix('\u{feff}');
    let bom = if stripped.is_some() { '\u{feff}'.len_utf8() } else { 0 };
    let text = stripped.unwrap_or(text);
    let mut values = Vec::new();
    let mut lines = Vec::new();
    let mut filenames = Vec::new();
    let mut line_scan = InputLines::with_files_or_new(files);
    let mut offset = 0usize;
    loop {
        // Prefix positions 0/0: they shape only the refusal message's
        // clause, and every refusal here is discarded for the codec rerun.
        match jqf_engine::json_stream_next(&text[offset..], true, 0, 0, resources)
            .map_err(|error| publication.fail(PipelineFailure::Codec(parser_machine_error(error))))?
        {
            jqf_engine::JsonStreamStep::Value { value, consumed } => {
                offset += consumed;
                lines
                    .try_reserve(1)
                    .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
                lines.push(line_scan.at_value_end(source_bytes, offset + bom));
                if let Some(label) = line_scan.current_file_label() {
                    push_filename_run::<E>(&mut filenames, values.len(), label, publication)?;
                }
                values
                    .try_reserve(1)
                    .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
                values.push(Some(value));
            }
            jqf_engine::JsonStreamStep::Exhausted => break,
            // `NeedMore` is unreachable with the stream finished; refuse the
            // lane rather than trust it.
            jqf_engine::JsonStreamStep::NeedMore | jqf_engine::JsonStreamStep::Refused(_) => {
                return Ok(None);
            }
        }
    }
    Ok(Some(EagerSequence {
        values,
        lines,
        filenames,
    }))
}

/// the reference's shared input cursor over a STREAMING byte source — the input-family
/// completion of the seekability rule. `input`/`inputs` pull values
/// parsed ON DEMAND from a window the read callback refills, so a program
/// that stops pulling stops reading (a countdown cut over an unbounded pipe
/// answers promptly), and bytes the program never demands are never parsed —
/// a truncated tail behind the last pulled value raises no error, the reference's own
/// laziness.
///
/// The parse is the ENGINE's reference-faithful reader
/// ([`jqf_engine::json_stream_next`]), not the codec's: a pull happens inside
/// the engine through [`InputSource::next`]'s shared request context, where no
/// codec provider can be driven, and the engine reader is the same acceptor
/// `fromjson` and `--slurpfile` use. Bytes are appended under the incremental
/// lossy-UTF-8 law (each invalid sequence one replacement character — the
/// `--slurpfile` precedent), with an incomplete multi-byte tail held raw
/// until its completion arrives.
pub(crate) struct StreamingInputCursor<ReadFn> {
    /// The caller's byte source; its error is the rendered message a failing
    /// pull raises.
    read: RefCell<ReadFn>,
    /// The growing parse window and its stream-absolute bookkeeping.
    window: RefCell<StreamingInputWindow>,
    /// The label `input_filename` reports once a value has been marked.
    filename: String,
    /// The line of the most recently pulled value's end (the whole-read
    /// cursor table's `at_value_end` law, kept absolute from the stream
    /// start).
    pulled_line: Cell<u64>,
    /// The MARKED (current) value's line: `None` until `mark_current` runs,
    /// the reference's pre-read state (`-n 'input_filename'` answers `null` and
    /// `input_line_number` `0` before any pull — the whole-read cursor's own
    /// law).
    marked_line: Cell<Option<u64>>,
    /// How many times the program pulled from this cursor, successful or not —
    /// the reference's "input was touched" signal the streaming null-first
    /// drive reads after the run, exactly as [`OwnedInputCursor::pulls`].
    pub(crate) pulls: Cell<u64>,
    /// The pulled-record kept-subtree hint derived once at drive setup from
    /// `CompiledProgram::try_pulled_record_requirement`: every pull decodes
    /// field-pruned. `None` (no derivation, whole-root hint, lowering
    /// failure) is the whole-decode floor byte for byte.
    pub(crate) prune: Option<jqf_engine::PruneHint>,
}

/// The [`StreamingInputCursor`]'s window: decoded text the parser reads,
/// plus the raw tail and stream-absolute counters that keep refills exact.
pub(crate) struct StreamingInputWindow {
    /// The decoded text: the live window is `text[start..]`. Consumed values
    /// only advance `start`; the prefix is compacted away at the next refill,
    /// so a fold over many small values pays one compaction per read chunk
    /// instead of one memmove per value.
    text: String,
    /// The live window's first byte within `text`.
    start: usize,
    /// Raw bytes of an incomplete multi-byte character at the stream's read
    /// point, held until the next chunk completes (or ends) it.
    pending: Vec<u8>,
    /// The reusable read chunk. Heap-allocated like every other read scratch
    /// in this crate's streaming drives.
    scratch: Vec<u8>,
    /// Newlines drained ahead of the window — the absolute-position base for
    /// refusal messages and per-value lines.
    newlines_drained: u64,
    /// Bytes since the last drained newline, at the window's first byte.
    column_base: u64,
    /// The window length at the last held parse attempt: a held value is
    /// re-parsed only once the window has DOUBLED past it (or at stream
    /// end), so a large single value is decoded O(log n) times instead of
    /// once per chunk — the streaming adjacent-value drive's own backoff.
    retry_floor: usize,
    /// Whether the read callback has reported end of stream.
    eof: bool,
    /// Whether the one stream-start BOM check has run (the reference strips exactly one
    /// leading BOM, uncounted — the `json_sequence` law).
    bom_checked: bool,
}

impl StreamingInputWindow {
    /// Reads one chunk from `read` into the window. At end of stream an
    /// incomplete held tail is lossy-converted (its completion can no longer
    /// arrive), exactly as `from_utf8_lossy` ends a truncated stream.
    fn refill<ReadFn>(&mut self, read: &mut ReadFn) -> Result<(), jqf_engine::InputSourceError>
    where
        ReadFn: FnMut(&mut [u8]) -> Result<usize, String>,
    {
        if self.eof {
            return Ok(());
        }
        // Compact the consumed prefix away before growing the text: the
        // window stays chunk-sized and parse offsets stay small.
        if self.start > 0 {
            self.text.drain(..self.start);
            self.start = 0;
        }
        if self.scratch.is_empty() {
            self.scratch
                .try_reserve_exact(STREAMING_READ_CHUNK)
                .map_err(|_| jqf_engine::InputSourceError::Allocation)?;
            self.scratch.resize(STREAMING_READ_CHUNK, 0);
        }
        let count = read(&mut self.scratch).map_err(jqf_engine::InputSourceError::Refused)?;
        if count == 0 {
            self.eof = true;
            append_window_bytes(&mut self.text, &mut self.pending, &[], true)?;
        } else {
            append_window_bytes(&mut self.text, &mut self.pending, &self.scratch[..count], false)?;
        }
        // One stream-start BOM is stream metadata, stripped uncounted the
        // moment the first decoded character exists (the `json_sequence`
        // law); anything else fixes the check closed.
        if !self.bom_checked && !self.text.is_empty() {
            if self.text.starts_with('\u{feff}') {
                self.text.drain(..'\u{feff}'.len_utf8());
            }
            self.bom_checked = true;
        }
        Ok(())
    }

    /// The line to report for a value ending at `consumed` — the whole-read
    /// cursor table's `at_value_end` law over the live window: trim trailing
    /// separator whitespace, extend through the first newline after the
    /// value's true end, count absolutely from the stream start.
    // ponytail: a terminator newline that has not ARRIVED yet is not counted
    // (the whole-read law scans the retained buffer past the value); the
    // settle byte that completed the value is always in the window, so the
    // NDJSON-shaped streams this serves carry their newline here.
    fn line_at_value_end(&self, consumed: usize) -> u64 {
        let bytes = &self.text.as_bytes()[self.start..];
        let mut cut = consumed.min(bytes.len());
        while cut > 0 && bytes[cut - 1].is_ascii_whitespace() {
            cut -= 1;
        }
        let stop = bytes[cut..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |offset| cut + offset + 1);
        #[expect(
            clippy::naive_bytecount,
            reason = "counts newlines over one value's span at value boundaries only; \
                      a bytecount dependency is not warranted for this cold path"
        )]
        let newlines = bytes[..stop].iter().filter(|byte| **byte == b'\n').count();
        self.newlines_drained
            .saturating_add(u64::try_from(newlines).unwrap_or(u64::MAX))
    }

    /// Drains the window's first `count` bytes, advancing the absolute
    /// position base they carried. The bytes stay in `text` until the next
    /// refill compacts them.
    fn drain(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let drained = &self.text.as_bytes()[self.start..self.start + count];
        match drained.iter().rposition(|byte| *byte == b'\n') {
            Some(last) => {
                #[expect(
                    clippy::naive_bytecount,
                    reason = "counts newlines over one drained value's span at value boundaries \
                              only; a bytecount dependency is not warranted for this cold path"
                )]
                let newlines = drained.iter().filter(|byte| **byte == b'\n').count();
                self.newlines_drained = self
                    .newlines_drained
                    .saturating_add(u64::try_from(newlines).unwrap_or(u64::MAX));
                self.column_base = u64::try_from(count - last - 1).unwrap_or(u64::MAX);
            }
            None => {
                self.column_base = self
                    .column_base
                    .saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
            }
        }
        self.start += count;
    }
}

/// Appends raw bytes to the streaming window under the incremental
/// lossy-UTF-8 law: valid runs append as they stand, each invalid sequence
/// becomes one replacement character (the `--slurpfile` lossy precedent), and
/// an incomplete multi-byte tail is held raw in `pending` for the next chunk
/// — unless `at_end`, when no completion can arrive and the tail becomes one
/// replacement character too.
pub(crate) fn append_window_bytes(
    text: &mut String,
    pending: &mut Vec<u8>,
    bytes: &[u8],
    at_end: bool,
) -> Result<(), jqf_engine::InputSourceError> {
    let held;
    let mut rest: &[u8] = if pending.is_empty() {
        bytes
    } else {
        pending
            .try_reserve(bytes.len())
            .map_err(|_| jqf_engine::InputSourceError::Allocation)?;
        pending.extend_from_slice(bytes);
        held = core::mem::take(pending);
        &held
    };
    loop {
        match core::str::from_utf8(rest) {
            Ok(valid) => {
                text.try_reserve(valid.len())
                    .map_err(|_| jqf_engine::InputSourceError::Allocation)?;
                text.push_str(valid);
                return Ok(());
            }
            Err(error) => {
                let valid = core::str::from_utf8(&rest[..error.valid_up_to()])
                    .map_err(|_| jqf_engine::InputSourceError::Allocation)?;
                text.try_reserve(valid.len().saturating_add(4))
                    .map_err(|_| jqf_engine::InputSourceError::Allocation)?;
                text.push_str(valid);
                match error.error_len() {
                    // The stream ends inside a multi-byte character: its
                    // completion can no longer arrive.
                    None if at_end => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        return Ok(());
                    }
                    // The CHUNK ends inside a multi-byte character: hold its
                    // bytes for the next refill.
                    None => {
                        pending
                            .try_reserve(rest.len() - error.valid_up_to())
                            .map_err(|_| jqf_engine::InputSourceError::Allocation)?;
                        pending.extend_from_slice(&rest[error.valid_up_to()..]);
                        return Ok(());
                    }
                    Some(bad) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        rest = &rest[error.valid_up_to().saturating_add(bad)..];
                    }
                }
            }
        }
    }
}

impl<ReadFn> StreamingInputCursor<ReadFn>
where
    ReadFn: FnMut(&mut [u8]) -> Result<usize, String> + 'static,
{
    pub(crate) fn new(label: &str, read: ReadFn) -> Self {
        Self {
            read: RefCell::new(read),
            window: RefCell::new(StreamingInputWindow {
                text: String::new(),
                start: 0,
                pending: Vec::new(),
                scratch: Vec::new(),
                newlines_drained: 0,
                column_base: 0,
                retry_floor: 0,
                eof: false,
                bom_checked: false,
            }),
            filename: String::from(label),
            pulled_line: Cell::new(0),
            marked_line: Cell::new(None),
            pulls: Cell::new(0),
            prune: None,
        }
    }

    /// Attaches the pulled-record kept-subtree hint (see the field's law).
    pub(crate) fn with_hint(mut self, prune: Option<jqf_engine::PruneHint>) -> Self {
        self.prune = prune;
        self
    }

    /// Pulls the next value: parse what the window holds, refill on a held
    /// value (with the doubling backoff), finish the tail at end of stream.
    fn pull(&self, resources: &ResourceContext<'_>) -> Result<Option<Value>, jqf_engine::InputSourceError> {
        let mut read = self.read.borrow_mut();
        let mut window = self.window.borrow_mut();
        loop {
            let at_eof = window.eof && window.pending.is_empty();
            let live = window.text.len() - window.start;
            if !at_eof && window.retry_floor > 0 && live < window.retry_floor.saturating_mul(2) {
                // The held value's backoff: the last attempt failed at the
                // window's end and the window has not doubled since.
                window.refill(&mut *read)?;
                continue;
            }
            match jqf_engine::json_stream_next_hinted(
                &window.text[window.start..],
                at_eof,
                window.newlines_drained,
                window.column_base,
                self.prune.as_ref(),
                resources,
            )
            .map_err(|_| jqf_engine::InputSourceError::Allocation)?
            {
                jqf_engine::JsonStreamStep::Value { value, consumed } => {
                    window.retry_floor = 0;
                    self.pulled_line.set(window.line_at_value_end(consumed));
                    window.drain(consumed);
                    return Ok(Some(value));
                }
                jqf_engine::JsonStreamStep::Exhausted => {
                    let length = window.text.len() - window.start;
                    window.drain(length);
                    if at_eof {
                        return Ok(None);
                    }
                    window.refill(&mut *read)?;
                }
                jqf_engine::JsonStreamStep::NeedMore => {
                    window.retry_floor = window.text.len() - window.start;
                    window.refill(&mut *read)?;
                }
                jqf_engine::JsonStreamStep::Refused(message) => {
                    return Err(jqf_engine::InputSourceError::Refused(message));
                }
            }
        }
    }
}

impl<ReadFn> InputSource for StreamingInputCursor<ReadFn>
where
    ReadFn: FnMut(&mut [u8]) -> Result<usize, String> + 'static,
{
    fn next(&mut self, resources: &mut ResourceContext<'_>) -> Result<Option<Value>, jqf_engine::InputSourceError> {
        self.pulls.set(self.pulls.get().saturating_add(1));
        self.pull(resources)
    }

    fn current_filename(&self) -> Option<&str> {
        // The pre-read state answers `null`, exactly like the whole-read
        // cursor before its first mark.
        self.marked_line.get().map(|_| self.filename.as_str())
    }

    fn current_line(&self) -> u64 {
        self.marked_line.get().unwrap_or(0)
    }

    fn mark_current(&self) {
        self.marked_line.set(Some(self.pulled_line.get()));
    }

    fn pulls(&self) -> u64 {
        self.pulls.get()
    }
}
