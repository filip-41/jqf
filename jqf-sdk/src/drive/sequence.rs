//! The sequence drive family: multi-value sequence drives over adjacent inputs.

use super::{
    AccessRequirement, CodecCatalog, CodecError, CompiledProgram, DialectId, ElementAnswer, EncodeRequest,
    EngineResult, EngineRun, FacadeFraming, FormatId, InputLines, ItemSink, LabelStyle, PipelineError, PipelineFailure,
    PipelinePolicy, Publication, RaisedError, ResolvedSource, ResourceContext, ReusableAccessSession,
    ReusableEncoderSession, SequenceError, SequenceReport, SequenceValueError, SourceId, SourceKind, SourceRef,
    StreamStop, StreamingSequenceError, ValueOutcome, Vec, count_answer, decode_sequence_item, drive_run_stream,
    element_answer, encode_one, note_single_document_output, overflow, pushdown_error, require_forward_progress,
    validate_credits,
};

/// Skips the policy-declared insignificant whitespace separating adjacent
/// complete values (empty by default — every byte reaches the decoder;
/// JSON/NDJSON/json-seq pass [`jqf_codec_json::VALUE_SEPARATORS`]).
pub(crate) fn skip_value_separator(bytes: &[u8], mut offset: usize, separator: &[u8]) -> usize {
    while offset < bytes.len() && separator.contains(&bytes[offset]) {
        offset += 1;
    }
    offset
}

/// Executes every complete value adjacent in one retained source (optional-
/// whitespace-separated, e.g. NDJSON or space-separated JSON texts) through
/// exact access, engine handoff, and ordered encoding — one program run per
/// value, in order, exactly as if [`execute_value_document`] were called once per value.
///
/// `policy.decode` must opt in with `allow_adjacent_values: true` so the
/// bound route reports each value's consumed offset instead of rejecting
/// what follows it as trailing content; a route that does not honor the
/// opt-in fails the first value with an internal contract violation rather
/// than silently truncating the stream.
///
/// One provider is constructed for the complete retained source and its
/// input bytes are charged exactly once (the ledger's request scope is the
/// whole call, matching one CLI invocation), then reopened once per value
/// with [`jqf_codec_core::ErasedProvider::open_at`] — no per-value input
/// re-charge. Each value is still its own independently accounted, verified
/// decode and engine result; only the input-bytes admission charge is shared.
///
/// Two failure regimes, matching jq exactly. A per-value RUNTIME error (a typed
/// index mismatch or an `.[]` iterate mismatch, whether raised at the pushdown
/// or mid-fan-out) is reported to the sink via
/// [`ItemSink::report_value_error`], the value's already-published prefix is
/// kept, and the sequence CONTINUES to the next adjacent value; the sequence's
/// exit class is the LAST value's class (a trailing runtime failure returns
/// `Err`, an earlier one that a later success follows returns `Ok`). A
/// DECODE/parse failure (a malformed later text) still stops the sequence after
/// every earlier value has been published — the reference stops there too.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the pipeline keeps each ownership boundary explicit; the adjacent-value loop is one \
              linear orchestration whose per-value continue-on-error disposition is clearer inline \
              than split across helpers"
)]
pub(crate) fn execute_sequence<Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'_>,
    files: Option<&[jqf_source::SourceFileRange<'_>]>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    requirement: &AccessRequirement,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<SequenceReport, PipelineError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, false)?;

    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let handle = provider
        .bind(requirement)
        .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?;
    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let encoding_policy = policy.encoding();
    let factory = encoder
        .create_factory(
            EncodeRequest {
                format: output_format,
                dialect: output_dialect,
                diagnostics: encoding_policy.diagnostics,
                preservation: encoding_policy.preservation,
                options: encoding_policy.options,
            },
            resources,
        )
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    // One recycled encoder for the whole sequence's publication: each item
    // restarts the retained staging buffer and frame stack instead of
    // allocating and dropping a complete encoder per published item.
    let mut reused_encoder = ReusableEncoderSession::new();
    // One recycled access session for the whole sequence: each value resets the
    // previous value's retained workspaces instead of constructing and dropping
    // a session per value. Its charge is one session's retained capacity, not
    // one per decoded value.
    let mut reuse = ReusableAccessSession::new();
    let mut items = 0u64;
    let mut offset = 0usize;
    let mut value_index = 0u64;
    let mut lines = InputLines::with_files_or_new(files);
    // The exit class is the LAST value's class: reset to `None` on every value
    // that succeeds (published, suppressed, or missing-null-forwarded), set to
    // the runtime mismatch on every value that fails. A value that fails
    // publishes whatever prefix it produced, its error is reported, and the loop
    // continues to the next adjacent value.
    let mut last_error: Option<SequenceError> = None;
    let mut codec_value_errors = 0u64;
    loop {
        let start = skip_value_separator(source.bytes(), offset, policy.decode.value_separator);
        if start >= source.bytes().len() {
            break;
        }
        let start_offset = u64::try_from(start).map_err(|_| overflow::<Sink::Error>(&publication))?;
        let engine = decode_sequence_item(
            &mut provider,
            &mut reuse,
            &handle,
            start_offset,
            policy.cooperative_credits,
            resources,
            &publication,
        )?;
        let consumed = require_forward_progress::<Sink::Error>(engine.report().consumed_offset(), &publication)?;
        let (codec_outcome, _access_report) = engine.into_parts();
        // The COUNT fast-path: a count-class program's value is
        // served by the document-core consumer from the lazy document's span
        // skeleton — no executor run, no leaf materialization. A decline
        // falls through to the ordinary residual run over the same outcome.
        if let Some(count) = count_answer(&codec_outcome, program, resources)
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        {
            let consumed_usize = usize::try_from(consumed).map_err(|_| overflow::<Sink::Error>(&publication))?;
            let end = start
                .checked_add(consumed_usize)
                .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
            encode_one(
                &factory,
                &mut reused_encoder,
                &EngineResult::owned(count),
                items,
                encoding_policy,
                framing,
                resources,
                sink,
                &mut publication,
            )?;
            items = items
                .checked_add(1)
                .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
            last_error = None;
            offset = end;
            value_index = value_index
                .checked_add(1)
                .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
            continue;
        }

        // Plan 133 R6's ELEMENT-ITERATION fast-path: a fan-out/fold program's
        // values are served by the document-core consumer iterating the lazy
        // document's span skeleton — no executor run, no whole-tree
        // materialization. A decline falls through to the ordinary residual
        // run over the same outcome.
        match element_answer(
            &codec_outcome,
            program,
            &factory,
            &mut reused_encoder,
            encoding_policy,
            framing,
            resources,
            sink,
            &mut publication,
        )? {
            ElementAnswer::FanOut { items: advanced } => {
                let consumed_usize = usize::try_from(consumed).map_err(|_| overflow::<Sink::Error>(&publication))?;
                let end = start
                    .checked_add(consumed_usize)
                    .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
                items = items
                    .checked_add(advanced)
                    .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
                last_error = None;
                offset = end;
                value_index = value_index
                    .checked_add(1)
                    .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
                continue;
            }
            ElementAnswer::Fold(state) => {
                let consumed_usize = usize::try_from(consumed).map_err(|_| overflow::<Sink::Error>(&publication))?;
                let end = start
                    .checked_add(consumed_usize)
                    .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
                encode_one(
                    &factory,
                    &mut reused_encoder,
                    &EngineResult::owned(state),
                    items,
                    encoding_policy,
                    framing,
                    resources,
                    sink,
                    &mut publication,
                )?;
                items = items
                    .checked_add(1)
                    .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
                last_error = None;
                offset = end;
                value_index = value_index
                    .checked_add(1)
                    .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
                continue;
            }
            ElementAnswer::None => {}
        }

        // The engine owns the reference-parity reading under the program's `?` flags and
        // pushdown boundary. A resolved value or a pushed-down missing/null
        // streams through the residual (an identity residual forwards one item; an
        // `.[]` residual fans out or errors). A flagged-step pushdown mismatch is
        // suppressed. A per-value runtime mismatch — pushed-down non-null, or one
        // discovered mid-fan-out — is a RUNTIME error: it is reported to the sink
        // and the sequence continues to the next value (the reference's continue-on-error).
        // Decode/parse failures stopped the loop above and keep stop-on-error.
        let outcome: Option<ValueOutcome> = match program
            .try_run(codec_outcome, resources)
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
            .with_iteration_cap(policy.max_iterations)
        {
            EngineRun::Suppressed => None,
            EngineRun::Pushdown(error) => Some(ValueOutcome::Mismatch(pushdown_error(error))),
            EngineRun::Stream { stream, .. } => match drive_run_stream(
                &factory,
                &mut reused_encoder,
                stream,
                items,
                encoding_policy,
                framing,
                resources,
                sink,
                &mut publication,
            )? {
                StreamStop::Complete(advanced) => {
                    items = advanced;
                    None
                }
                StreamStop::Runtime { items: advanced, error } => {
                    items = advanced;
                    Some(ValueOutcome::Mismatch(error))
                }
                StreamStop::Raised { items: advanced, value } => {
                    items = advanced;
                    Some(ValueOutcome::Raised(value))
                }
                StreamStop::ValueFailure { items: advanced, error } => {
                    items = advanced;
                    Some(ValueOutcome::Codec(error))
                }
                StreamStop::SplitName {
                    items: advanced,
                    index,
                    detail,
                } => {
                    items = advanced;
                    Some(ValueOutcome::SplitName { index, detail })
                }
                // `halt`/`halt_error`: stop the whole sequence NOW — no further
                // adjacent values run, already-published stdout stands, and the
                // message (if any) is printed raw to stderr.
                StreamStop::Halt { status, message } => {
                    return Err(publication.fail(PipelineFailure::Halt { status, message }));
                }
            },
        };
        let consumed_usize = usize::try_from(consumed).map_err(|_| overflow::<Sink::Error>(&publication))?;
        let end = start
            .checked_add(consumed_usize)
            .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
        // the reference's `<stdin>:N` is a LEXER position, so the line is read off this
        // value's END — after the consumed span is known and BEFORE the loop
        // advances. It is computed ONLY in the error arms: the scan walks input
        // bytes and must never run for a value that succeeds.
        match outcome {
            Some(ValueOutcome::Mismatch(error)) => {
                let input_line = lines.at_value_end(source.bytes(), end);
                let filename = lines.current_file_label();
                let mismatch = error.mismatch;
                sink.report_value_error(error.into_sequence_error(value_index, input_line, filename))
                    .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                last_error = Some(SequenceError::Mismatch(mismatch));
            }
            Some(ValueOutcome::Codec(error)) => {
                let input_line = lines.at_value_end(source.bytes(), end);
                sink.report_value_error(SequenceValueError::try_for_codec(
                    value_index,
                    input_line,
                    lines.current_file_label(),
                    &error,
                ))
                .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                codec_value_errors = codec_value_errors.saturating_add(1);
                last_error = Some(SequenceError::Codec(error));
            }
            Some(ValueOutcome::Raised(value)) => {
                let input_line = lines.at_value_end(source.bytes(), end);
                // Report a clone (the reference's per-value stderr), keep the original as the
                // sequence's trailing exit class.
                let reported = value.clone();
                let report =
                    SequenceValueError::try_for_raised(value_index, input_line, lines.current_file_label(), reported)
                        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
                sink.report_value_error(report)
                    .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                last_error = Some(SequenceError::Raised(value));
            }
            Some(ValueOutcome::SplitName { index, detail }) => {
                last_error = Some(SequenceError::SplitName { index, detail });
            }
            None => last_error = None,
        }
        offset = end;
        value_index = value_index
            .checked_add(1)
            .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
    }
    // Exit class is the last value's class: a trailing runtime failure surfaces
    // as the sequence's `Err` (already reported to the sink as it occurred),
    // otherwise the sequence succeeds.
    match last_error {
        Some(SequenceError::Mismatch(mismatch)) => Err(publication.fail(mismatch.into_failure())),
        Some(SequenceError::Raised(value)) => Err(publication.fail(PipelineFailure::Raised(RaisedError { value }))),
        Some(SequenceError::Codec(error)) => Err(publication.fail(PipelineFailure::Codec(error))),
        Some(SequenceError::SplitName { index, detail }) => {
            Err(publication.fail(PipelineFailure::SplitName { index, detail }))
        }
        None => Ok(SequenceReport {
            publication: publication.status(),
            items,
            codec_value_errors,
        }),
    }
}

/// The read chunk of the streaming adjacent-value drive: 64 KiB, the same
/// size the whole-stdin reader and the follow route use, so a streaming run
/// pays the same syscall shape as the read-whole runs.
pub(crate) const STREAMING_READ_CHUNK: usize = 64 * 1024;

/// Whether a decode failure means "the value would complete with more input"
/// rather than "the input is invalid": the primary label must start EXACTLY at
/// the end of the window the codec was decoding. That is the JSON grammar's
/// own cut — a parse error strictly before the window's end is a definite
/// wrong byte (a bare `x`, a `]` after a comma), while an error at the end
/// means the input ended where a token could continue (`{"a":`, `[1,`,
/// `"abc`, `1e`). A value held this way is re-decoded from scratch on the
/// next refill and, at stream end, fails exactly as the whole-input run fails
/// it over the same bytes.
pub(crate) fn decode_stopped_at_window_end<E>(error: &PipelineError<E>, remaining: usize) -> bool {
    match error.failure() {
        PipelineFailure::Codec(codec) => codec
            .diagnostic()
            .and_then(|diagnostic| {
                diagnostic
                    .labels()
                    .iter()
                    .find(|label| label.style() == LabelStyle::Primary)
            })
            .is_some_and(|label| usize::try_from(label.span().start()).unwrap_or(usize::MAX) == remaining),
        _ => false,
    }
}

/// Executes the adjacent-value sequence over a GROWING byte source, publishing
/// each complete value as its bytes arrive — the reference's default input path over a
/// non-seekable stdin .
///
/// This is `execute_sequence` with the publish point moved: the same
/// per-value decode loop over the same RFC 8259 adjacency law (values
/// separated by arbitrary whitespace, never inferred as NDJSON), driven
/// against a window the caller's `read` refills instead of one fixed retained
/// buffer. Two outcomes mean "the value would complete with more input", and
/// the drive HOLDS the window from the value's start and refills for both. A
/// decode FAILURE whose primary label starts exactly at the window's end ended
/// where a token could continue (`{"a":`, `[1,`, `"abc`, `1e`), which
/// `decode_stopped_at_window_end` recognizes. A decode SUCCESS the codec
/// reports as open-ended ([`jqf_codec_core::AccessReport::open_ended`]) parsed
/// a token the bytes ended rather than a delimiter (a bare `1234` that the
/// next read turns into `1234567`) — no error is raised there, so without this
/// half the drive would publish a value that depends on where read(2) cut. Any
/// other failure is the whole-input route's stop-on-error law, reported
/// unchanged. At stream end the held tail is decoded one final time, with both
/// holds lifted, which reproduces the whole-input run's outcome over the same
/// bytes exactly — including its failure.
///
/// A held value smaller than one [`STREAMING_READ_CHUNK`] is always
/// re-decoded on the next refill so a completed tail-f record publishes
/// immediately. A larger held value is NOT re-decoded on every refill: the
/// whole window would have to re-parse from scratch, which is quadratic over
/// a large single document. The drive re-attempts those only when the
/// window has DOUBLED past the last held attempt, so a large value is
/// decoded O(log n) times instead of O(n) — the final attempt (or the
/// stream-end finalize) still decodes the same bytes exactly, so the
/// outcome is byte-identical and only the cadence of an end-of-window
/// failure's report is later, never wrong.
///
/// The window is drained of completed values on every refill, so a long
/// stream's retained memory stays bounded by the current partial value plus
/// one chunk. Per-value error line numbers are absolute from the stream start
/// (the drive re-bases the drained newlines), matching the whole-input run.
///
/// # Errors
///
/// Returns the caller's read failure, or the pipeline's own failure. The exit
/// class is the LAST value's class, exactly as `execute_sequence` keeps it.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the pipeline keeps each ownership boundary explicit; the streaming adjacent-value loop is \
              one linear orchestration whose hold/finalize disposition is clearer inline than split \
              across helpers"
)]
pub(crate) fn execute_sequence_streaming<Sink, ReadError, ReadFn>(
    catalog: CodecCatalog<'_, '_>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    requirement: &AccessRequirement,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    mut read: ReadFn,
) -> Result<SequenceReport, StreamingSequenceError<Sink::Error, ReadError>>
where
    Sink: ItemSink,
    ReadFn: FnMut(&mut [u8]) -> Result<usize, ReadError>,
{
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication).map_err(StreamingSequenceError::Pipeline)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, false)
        .map_err(StreamingSequenceError::Pipeline)?;

    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Registry(error))))?;
    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Registry(error))))?;
    let encoding_policy = policy.encoding();
    let factory = encoder
        .create_factory(
            EncodeRequest {
                format: output_format,
                dialect: output_dialect,
                diagnostics: encoding_policy.diagnostics,
                preservation: encoding_policy.preservation,
                options: encoding_policy.options,
            },
            resources,
        )
        .map_err(|error| StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Codec(error))))?;
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut items = 0u64;
    let mut value_index = 0u64;
    let mut lines = InputLines::new();
    // The exit class is the LAST value's class, exactly as the whole-input
    // sequence keeps it.
    let mut last_error: Option<SequenceError> = None;
    let mut codec_value_errors = 0u64;
    let mut window: Vec<u8> = Vec::new();
    // The reusable read chunk, sized through the fallible reserve so a ceiling
    // below the chunk size refuses at the drive start instead of aborting
    // (125 H2: single-oversized allocations are the audited `try_reserve`
    // class).
    // Reads go into the window's spare capacity. A bounce buffer would
    // memcpy every piped byte twice.
    // The window length at the last HELD decode attempt. The drive re-attempts
    // a held value only once the window has doubled past this, so a large
    // single document is re-parsed O(log n) times rather than once per chunk;
    // reset to zero when a value completes (the floor belongs to one value).
    let mut hold_retry_floor: usize = 0;
    // Newlines drained from the window ahead of a reported value: the window
    // re-bases to the held tail, so per-value line numbers re-base with it to
    // stay absolute from the stream start.
    let mut line_base: u64 = 0;
    // The window's absolute offset in the stream: the codec's diagnostics
    // re-base to the value start through it, so a truncated final value's
    // `source input#0 base=N` matches the whole-input run's exactly.
    let mut window_base: u64 = 0;
    // The final pass after read-EOF decodes the held tail with HOLDING
    // disabled: a value that ends exactly at the window's end is then the
    // whole-input route's own failure, never a silent drop.
    let mut finalize = false;
    loop {
        // Decode every value the current window completes. The provider
        // borrows the window, so it is created per refill and dropped before
        // the completed prefix is drained.
        if !window.is_empty() {
            let source = ResolvedSource::new(
                SourceRef::new(SourceId::new(0), SourceKind::Input),
                "<stdin>",
                &window,
                window_base,
            );
            let mut provider = decoder
                .create_provider(source, policy.decode, resources)
                .map_err(|error| StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Codec(error))))?;
            let handle = provider.bind(requirement).map_err(|error| {
                StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::AccessBind(error)))
            })?;
            // One recycled access session per refill: the provider borrows
            // this window, so the session (which borrows the provider) lives
            // beside it and is dropped before the completed prefix is drained.
            let mut reuse = ReusableAccessSession::new();
            let mut cursor = 0usize;
            loop {
                let start = skip_value_separator(&window, cursor, policy.decode.value_separator);
                if start >= window.len() {
                    cursor = start;
                    break;
                }
                // The held value's retry backoff applies only above one
                // STREAMING_READ_CHUNK: a small held value (the tail-f
                // shape) is always re-decoded as soon as more bytes arrive.
                // A large held value waits for the window to double so a
                // byte-at-a-time stream stays O(log n), never quadratic.
                if !finalize
                    && hold_retry_floor > STREAMING_READ_CHUNK
                    && window.len() < hold_retry_floor.saturating_mul(2)
                {
                    cursor = start;
                    break;
                }
                let start_offset = u64::try_from(start)
                    .map_err(|_| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                let engine = match decode_sequence_item(
                    &mut provider,
                    &mut reuse,
                    &handle,
                    start_offset,
                    policy.cooperative_credits,
                    resources,
                    &publication,
                ) {
                    Ok(engine) => engine,
                    Err(error) if !finalize && decode_stopped_at_window_end(&error, window.len() - start) => {
                        // The value would complete with more input: hold the
                        // window from the value's start and refill. The retry
                        // floor marks THIS attempt's HELD VALUE length (the
                        // window also holds the completed prefix, which the
                        // drain below removes), so the next attempt waits for
                        // the value to double — the prefix never counts.
                        hold_retry_floor = window.len() - start;
                        cursor = start;
                        break;
                    }
                    Err(error) => return Err(StreamingSequenceError::Pipeline(error)),
                };
                // The other half of "would complete with more input", and the
                // silent half: the decode SUCCEEDED, but its last token ran
                // out of bytes instead of meeting a delimiter, so the next
                // read would have extended it (`1234` + `567` is the one
                // number `1234567`, not two). Publishing it here would make
                // the output depend on where read(2) cut the stream. Hold it
                // on the same terms an end-of-window failure is held.
                if !finalize && engine.report().open_ended() {
                    hold_retry_floor = window.len() - start;
                    cursor = start;
                    break;
                }
                let consumed = require_forward_progress::<Sink::Error>(engine.report().consumed_offset(), &publication)
                    .map_err(StreamingSequenceError::Pipeline)?;
                let (codec_outcome, _access_report) = engine.into_parts();
                // The COUNT fast-path: a count-class program's value
                // is served by the document-core consumer from the lazy
                // document's span skeleton — no executor run, no leaf
                // materialization. A decline falls through to the ordinary
                // residual run over the same outcome (which, for a count
                // program, is the whole program over the whole document).
                if let Some(count) = count_answer(&codec_outcome, program, resources).map_err(|error| {
                    StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Codec(error)))
                })? {
                    let consumed_usize = usize::try_from(consumed)
                        .map_err(|_| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                    let end = start
                        .checked_add(consumed_usize)
                        .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                    encode_one(
                        &factory,
                        &mut reused_encoder,
                        &EngineResult::owned(count),
                        items,
                        encoding_policy,
                        framing,
                        resources,
                        sink,
                        &mut publication,
                    )
                    .map_err(StreamingSequenceError::Pipeline)?;
                    items = items
                        .checked_add(1)
                        .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                    last_error = None;
                    cursor = end;
                    value_index = value_index
                        .checked_add(1)
                        .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                    hold_retry_floor = 0;
                    continue;
                }
                // Plan 133 R6's ELEMENT-ITERATION fast-path: a fan-out/fold
                // program's values are served by the document-core consumer
                // iterating the lazy document's span skeleton — no executor
                // run, no whole-tree materialization. A decline falls through
                // to the ordinary residual run over the same outcome.
                match element_answer(
                    &codec_outcome,
                    program,
                    &factory,
                    &mut reused_encoder,
                    encoding_policy,
                    framing,
                    resources,
                    sink,
                    &mut publication,
                )
                .map_err(StreamingSequenceError::Pipeline)?
                {
                    ElementAnswer::FanOut { items: advanced } => {
                        let consumed_usize = usize::try_from(consumed)
                            .map_err(|_| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                        let end = start
                            .checked_add(consumed_usize)
                            .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                        items = items
                            .checked_add(advanced)
                            .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                        last_error = None;
                        cursor = end;
                        value_index = value_index
                            .checked_add(1)
                            .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                        hold_retry_floor = 0;
                        continue;
                    }
                    ElementAnswer::Fold(state) => {
                        let consumed_usize = usize::try_from(consumed)
                            .map_err(|_| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                        let end = start
                            .checked_add(consumed_usize)
                            .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                        encode_one(
                            &factory,
                            &mut reused_encoder,
                            &EngineResult::owned(state),
                            items,
                            encoding_policy,
                            framing,
                            resources,
                            sink,
                            &mut publication,
                        )
                        .map_err(StreamingSequenceError::Pipeline)?;
                        items = items
                            .checked_add(1)
                            .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                        last_error = None;
                        cursor = end;
                        value_index = value_index
                            .checked_add(1)
                            .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                        hold_retry_floor = 0;
                        continue;
                    }
                    ElementAnswer::None => {}
                }
                // The engine owns the reference-parity reading under the program's `?`
                // flags and pushdown boundary. A resolved value or a pushed-down
                // missing/null streams through the residual; a flagged-step
                // pushdown mismatch is suppressed; a per-value runtime mismatch
                // is reported to the sink and the sequence continues to the next
                // value (the reference's continue-on-error). Decode/parse failures stopped
                // the loop above and keep stop-on-error.
                let outcome: Option<ValueOutcome> = match program
                    .try_run(codec_outcome, resources)
                    .map_err(|error| publication.fail(PipelineFailure::Codec(error)))
                    .map_err(StreamingSequenceError::Pipeline)?
                    .with_iteration_cap(policy.max_iterations)
                {
                    EngineRun::Suppressed => None,
                    EngineRun::Pushdown(error) => Some(ValueOutcome::Mismatch(pushdown_error(error))),
                    EngineRun::Stream { stream, .. } => match drive_run_stream(
                        &factory,
                        &mut reused_encoder,
                        stream,
                        items,
                        encoding_policy,
                        framing,
                        resources,
                        sink,
                        &mut publication,
                    )
                    .map_err(StreamingSequenceError::Pipeline)?
                    {
                        StreamStop::Complete(advanced) => {
                            items = advanced;
                            None
                        }
                        StreamStop::Runtime { items: advanced, error } => {
                            items = advanced;
                            Some(ValueOutcome::Mismatch(error))
                        }
                        StreamStop::Raised { items: advanced, value } => {
                            items = advanced;
                            Some(ValueOutcome::Raised(value))
                        }
                        StreamStop::ValueFailure { items: advanced, error } => {
                            items = advanced;
                            Some(ValueOutcome::Codec(error))
                        }
                        StreamStop::SplitName {
                            items: advanced,
                            index,
                            detail,
                        } => {
                            items = advanced;
                            Some(ValueOutcome::SplitName { index, detail })
                        }
                        // `halt`/`halt_error`: stop the whole stream NOW — no
                        // further values run, already-published stdout stands,
                        // and the message (if any) is printed raw to stderr.
                        StreamStop::Halt { status, message } => {
                            return Err(StreamingSequenceError::Pipeline(
                                publication.fail(PipelineFailure::Halt { status, message }),
                            ));
                        }
                    },
                };
                let consumed_usize = usize::try_from(consumed)
                    .map_err(|_| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                let end = start
                    .checked_add(consumed_usize)
                    .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                // the reference's `<stdin>:N` is a LEXER position, so the line is read off
                // this value's END — after the consumed span is known and only
                // in the error arms, exactly as the whole-input loop keeps it.
                let filename = lines.current_file_label();
                match outcome {
                    Some(ValueOutcome::Mismatch(error)) => {
                        let input_line = lines.at_value_end(&window, end).saturating_add(line_base);
                        let mismatch = error.mismatch;
                        sink.report_value_error(error.into_sequence_error(value_index, input_line, filename))
                            .map_err(|error| {
                                StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                            })?;
                        last_error = Some(SequenceError::Mismatch(mismatch));
                    }
                    Some(ValueOutcome::Codec(error)) => {
                        let input_line = lines.at_value_end(&window, end).saturating_add(line_base);
                        sink.report_value_error(SequenceValueError::try_for_codec(
                            value_index,
                            input_line,
                            filename,
                            &error,
                        ))
                        .map_err(|error| {
                            StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                        })?;
                        codec_value_errors = codec_value_errors.saturating_add(1);
                        last_error = Some(SequenceError::Codec(error));
                    }
                    Some(ValueOutcome::Raised(value)) => {
                        let input_line = lines.at_value_end(&window, end).saturating_add(line_base);
                        let reported = value.clone();
                        let report = SequenceValueError::try_for_raised(value_index, input_line, filename, reported)
                            .map_err(|error| {
                                StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Codec(error)))
                            })?;
                        sink.report_value_error(report).map_err(|error| {
                            StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Sink(error)))
                        })?;
                        last_error = Some(SequenceError::Raised(value));
                    }
                    Some(ValueOutcome::SplitName { index, detail }) => {
                        last_error = Some(SequenceError::SplitName { index, detail });
                    }
                    None => last_error = None,
                }
                cursor = end;
                value_index = value_index
                    .checked_add(1)
                    .ok_or_else(|| StreamingSequenceError::Pipeline(overflow::<Sink::Error>(&publication)))?;
                // A value completed: its hold history is spent, so the next
                // value starts with a clean retry floor.
                hold_retry_floor = 0;
            }
            // The session outlives the provider it borrows, so it drops
            // first; only then can the completed prefix be drained (the
            // handle borrows the provider and needs no explicit drop).
            drop(reuse);
            drop(provider);
            if cursor > 0 {
                // The drained prefix's newlines are the next window's line
                // base, and the fresh window restarts the line scan.
                let drained_newlines = memchr::memchr_iter(b'\n', &window[..cursor]).count() as u64;
                line_base = line_base.saturating_add(drained_newlines);
                window_base = window_base.saturating_add(cursor as u64);
                window.drain(..cursor);
                lines = InputLines::new();
            }
        }
        if finalize {
            break;
        }
        // The next read blocks: make the already-published items visible
        // before the drive waits for bytes that may never come (the sink's
        // `flush` default is a no-op; the CLI overrides it).
        sink.flush()
            .map_err(|error| StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Sink(error))))?;
        window.try_reserve(STREAMING_READ_CHUNK).map_err(|_| {
            StreamingSequenceError::Pipeline(publication.fail(PipelineFailure::Codec(CodecError::from(
                jqf_resource::ResourceError::AllocationFailed,
            ))))
        })?;
        let spare = window.spare_capacity_mut();
        let spare_len = spare.len();
        // SAFETY: `Read::read` writes initialized bytes into the spare
        // prefix; `set_len` below publishes exactly that prefix.
        let buf = unsafe { core::slice::from_raw_parts_mut(spare.as_mut_ptr().cast::<u8>(), spare_len) };
        let read = read(buf).map_err(StreamingSequenceError::Read)?;
        if read == 0 {
            finalize = true;
        } else if read > spare_len {
            // A host read callback claiming more bytes than the buffer offered
            // would make `set_len` publish uninitialized memory — the same
            // sink-contract violation class an overlong `write` answer is.
            return Err(StreamingSequenceError::Pipeline(
                publication.fail(PipelineFailure::SinkContract),
            ));
        } else {
            // SAFETY: `read` initialized `window.len()..window.len()+read`.
            unsafe {
                window.set_len(window.len() + read);
            }
        }
    }
    // Exit class is the last value's class: a trailing runtime failure
    // surfaces as the drive's `Err` (already reported to the sink as it
    // occurred), otherwise the drive succeeds.
    match last_error {
        Some(SequenceError::Mismatch(mismatch)) => Err(StreamingSequenceError::Pipeline(
            publication.fail(mismatch.into_failure()),
        )),
        Some(SequenceError::Raised(value)) => Err(StreamingSequenceError::Pipeline(
            publication.fail(PipelineFailure::Raised(RaisedError { value })),
        )),
        Some(SequenceError::Codec(error)) => Err(StreamingSequenceError::Pipeline(
            publication.fail(PipelineFailure::Codec(error)),
        )),
        Some(SequenceError::SplitName { index, detail }) => Err(StreamingSequenceError::Pipeline(
            publication.fail(PipelineFailure::SplitName { index, detail }),
        )),
        None => Ok(SequenceReport {
            publication: publication.status(),
            items,
            codec_value_errors,
        }),
    }
}
