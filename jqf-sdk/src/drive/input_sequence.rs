//! The input-sequence drive family: the shared-cursor drives that serve the
//! reference's input-family model (`input`/`inputs`/`input_filename`).

use super::{
    AccessRequirement, Array, Box, Cell, CodecCatalog, CodecError, CodecInputOutcome, CodecRequirementPolicy,
    CodecRunContext, CompiledProgram, DialectId, EncodeRequest, EngineResult, EngineRun, FacadeFraming, FormatId,
    InputSource, ItemSink, OrderedEncodingPolicy, OwnedInputCursor, PipelineError, PipelineFailure, PipelinePolicy,
    Publication, RaisedError, RefCell, ResolvedSource, ResourceContext, ReusableAccessSession, ReusableEncoderSession,
    SequenceError, SequenceReport, SequenceValueError, StreamStop, StreamingInputCursor, String, UNKNOWN_INPUT_LINE,
    Value, ValueOutcome, Vec, allocation_failure, decode_eager_sequence, drive_run_stream, input_cursor_failure,
    materialize_sequence_value, note_single_document_output, overflow, pushdown_error, report_and_fail_codec,
    require_forward_progress, set_lazy_deferred, skip_value_separator, try_lower_root_requirement, validate_credits,
};

/// Derives the pulled-record kept-subtree hint for the streamed `-n inputs`
/// fold drive, once per request: lower `CompiledProgram::try_pulled_record_
/// requirement` (compile-cached derivation, no per-record work), refuse the
/// hint when a lazy frontier is armed (the provider's own condition), and
/// decline to `None` — the whole-decode floor byte for byte — on ANY
/// failure, because delivering more than the tree names is always sound.
fn pulled_record_prune_hint(
    program: &CompiledProgram,
    resources: &ResourceContext<'_>,
) -> Option<jqf_engine::PruneHint> {
    let requirement = program.try_pulled_record_requirement(resources).ok()?;
    if requirement.lazy_frontier() != 0 {
        return None;
    }
    let tree = requirement.prune()?;
    jqf_engine::PruneHint::from_tree(tree)
}

/// Runs one compiled program over an input SEQUENCE with the reference's shared-cursor
/// input family: the drive pulls the current value from the cursor, marks it,
/// runs the program (whose `input`/`inputs` may pull further values), reports
/// per-value errors and continues, exactly like `execute_sequence`.
///
/// The cursor is decoded EAGERLY from the whole source before any value runs,
/// which is jqf's whole-buffer model; a decode failure therefore stops before
/// any publication (the same stop-on-decode-error law `execute_sequence` keeps,
/// and a documented timing divergence from the reference's on-demand parse for this niche
/// family). The drive attaches the cursor to the request as the engine's host
/// extension, so the input laws read it through the ordinary resource seam.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the same boundary inventory execute_sequence threads; splitting would duplicate               it, and the shared-cursor drive is one loop read as a single obligation"
)]
pub(crate) fn execute_input_sequence<Sink: ItemSink>(
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
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut reuse = ReusableAccessSession::new();
    let mut items = 0u64;
    let mut value_index = 0u64;
    let mut last_error: Option<SequenceError> = None;
    let mut codec_value_errors = 0u64;
    // Eager whole-buffer decode: every adjacent value becomes an owned cursor
    // entry with its line number, stopping on the first decode failure (jqf's
    // stop-on-error decode law). Shared with the `-n` and `-s` drives so the
    // three input models never disagree about which bytes decode.
    let decoded_seq = decode_eager_sequence(
        &mut provider,
        &mut reuse,
        &handle,
        source,
        files,
        input_format,
        input_dialect,
        policy.cooperative_credits,
        policy.decode.value_separator,
        resources,
        &publication,
    )?;
    let cursor = decoded_seq.cursor(source, 0, None);
    resources.set_host_extension(Box::new(jqf_engine::InputSourceHandle::new(Box::new(cursor))));
    loop {
        let current = pull_marked_input(resources, &publication)?
            .map_err(|_| input_cursor_failure::<Sink::Error>(&publication))?;
        let Some(current) = current else {
            break;
        };
        let (outcome, advanced) = run_one_owned_value(
            program,
            CodecInputOutcome::Result(EngineResult::owned(current)),
            &factory,
            &mut reused_encoder,
            items,
            policy.max_iterations,
            encoding_policy,
            framing,
            resources,
            sink,
            &mut publication,
        )?;
        items = advanced;
        let input_cursor = resources
            .host_extension()
            .and_then(|extension| extension.downcast_ref::<jqf_engine::InputSourceHandle>())
            .ok_or_else(|| input_cursor_failure::<Sink::Error>(&publication))?;
        let input_line = input_cursor.current_line();
        let filename = input_cursor.current_filename();
        match outcome {
            Some(ValueOutcome::Mismatch(error)) => {
                let mismatch = error.mismatch;
                sink.report_value_error(error.into_sequence_error(value_index, input_line, filename))
                    .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                last_error = Some(SequenceError::Mismatch(mismatch));
            }
            Some(ValueOutcome::Codec(error)) => {
                sink.report_value_error(SequenceValueError::try_for_codec(
                    value_index,
                    input_line,
                    filename,
                    &error,
                ))
                .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                codec_value_errors = codec_value_errors.saturating_add(1);
                last_error = Some(SequenceError::Codec(error));
            }
            Some(ValueOutcome::Raised(value)) => {
                let reported = value.clone();
                let report = SequenceValueError::try_for_raised(value_index, input_line, filename, reported)
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
        value_index = value_index
            .checked_add(1)
            .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
    }
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

/// Decodes every document of `source` into its OWNED VALUE, in order — the
/// codec catalog's whole-document decode with no program in the loop (plan
/// 107 seam 1: the `--diff` lane's per-file decode, the same drive the edit
/// lane's one-shot and adjacent-value branches run).
///
/// The drive is keyed on [`jqf_codec_core::DecodeRequest::allow_adjacent_values`] exactly as
/// `execute_source_edit`'s is: an adjacent-capable source (JSON, YAML,
/// NDJSON, json-seq, CSV) loops with forward-progress accounting over every
/// document; a single-document format (TOML, CBOR, XML, HTML, jqft family) is
/// exactly one text per source — one decode, no consumed-offset requirement.
/// Each document materializes through the ordinary whole-document route bound
/// from [`try_lower_root_requirement`], so decode refusals keep their codec
/// diagnostics and the count of returned values is the source's document
/// count (the caller's exactly-one law sits on top).
#[allow(
    clippy::too_many_arguments,
    reason = "the decode keeps the same boundary inventory every SDK drive threads"
)]
pub fn decode_source_values<E>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'_>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<Vec<Value>, PipelineError<E>> {
    let publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    // The whole-document route under the request's own validation and
    // diagnostic policy, EAGER (`Some(0)`, the explicit override of the
    // `try_lower_root_requirement` LAZY default): this drive materializes
    // every value in full, so deferral would only re-parse each span on
    // materialization.
    let requirement = try_lower_root_requirement(
        CodecRequirementPolicy::new(policy.decode.validation, policy.decode.diagnostics),
        Some(0),
        resources,
    )
    .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let handle = provider
        .bind(&requirement)
        .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?;
    let mut reuse = ReusableAccessSession::new();
    let mut values = Vec::new();
    if policy.decode.allow_adjacent_values {
        // The adjacent-value drive : the source is a stream
        // of documents whose spans attach to the segment holding exactly their
        // own text. Forward progress is REQUIRED — a route that does not
        // report where its value ended would loop forever over the same bytes.
        let mut offset = 0usize;
        loop {
            let start = skip_value_separator(source.bytes(), offset, policy.decode.value_separator);
            if start >= source.bytes().len() {
                break;
            }
            let start_offset = u64::try_from(start).map_err(|_| overflow::<E>(&publication))?;
            let item = decode_sequence_item(
                &mut provider,
                &mut reuse,
                &handle,
                start_offset,
                policy.cooperative_credits,
                resources,
                &publication,
            )?;
            let consumed = require_forward_progress::<E>(item.report().consumed_offset(), &publication)?;
            let consumed_usize = usize::try_from(consumed).map_err(|_| overflow::<E>(&publication))?;
            let end = start
                .checked_add(consumed_usize)
                .ok_or_else(|| overflow::<E>(&publication))?;
            values
                .try_reserve(1)
                .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
            values.push(materialize_sequence_value(item, resources, &publication)?);
            offset = end;
        }
    } else {
        // The one-shot single-document drive : a
        // single-document format is exactly ONE text per source, so one
        // decode — no forward-progress requirement, because the provider
        // reports no consumed offset.
        let item = decode_sequence_item(
            &mut provider,
            &mut reuse,
            &handle,
            0,
            policy.cooperative_credits,
            resources,
            &publication,
        )?;
        values
            .try_reserve(1)
            .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
        values.push(materialize_sequence_value(item, resources, &publication)?);
    }
    Ok(values)
}

/// The identity pair the value-direct eager JSON decode is pinned to —
/// `jqf_codec_json::FORMAT_ID` and `RFC8259_DIALECT_ID`, asserted equal in
/// this module's tests (the SDK deliberately carries no json-codec
/// dependency, so the identities are text here).
pub(crate) const DIRECT_JSON_FORMAT: &str = "json";

/// See [`DIRECT_JSON_FORMAT`].
pub(crate) const DIRECT_JSON_DIALECT: &str = "rfc8259";

/// Runs one SYNTHESIZED input value through the compiled program and publishes
/// its stream, returning the per-value outcome and the advanced item count.
/// Shared by the four single-run drives (`-n`/`-s`, adjacent-value and record).
///
/// The value came from the DRIVE — `null`, or the array of every decoded input
/// — not out of a decode, so nothing resolved the program's pushed-down
/// prefix and the whole program runs
/// ([`CompiledProgram::try_run_whole_value`]).
#[allow(
    clippy::too_many_arguments,
    reason = "one run-per-value seam shared by the four single-run drives"
)]
pub(crate) fn run_one_owned_value<Sink: ItemSink>(
    program: &CompiledProgram,
    input: CodecInputOutcome<'_>,
    factory: &jqf_codec_core::ErasedEncoderFactory,
    reused_encoder: &mut ReusableEncoderSession,
    items: u64,
    max_iterations: Option<u64>,
    encoding_policy: OrderedEncodingPolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<(Option<ValueOutcome>, u64), PipelineError<Sink::Error>> {
    let mut items = items;
    let outcome = match program
        .try_run_whole_value(input, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        .with_iteration_cap(max_iterations)
    {
        EngineRun::Suppressed => None,
        EngineRun::Pushdown(error) => Some(ValueOutcome::Mismatch(pushdown_error(error))),
        EngineRun::Stream { stream, .. } => match drive_run_stream(
            factory,
            reused_encoder,
            stream,
            items,
            encoding_policy,
            framing,
            resources,
            sink,
            publication,
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
            StreamStop::Halt { status, message } => {
                return Err(publication.fail(PipelineFailure::Halt { status, message }));
            }
        },
        EngineRun::ReboundWhole => {
            return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "-n/-s runs Whole; Exact count miss cannot rebound",
                },
            ))));
        }
    };
    Ok((outcome, items))
}

/// Reports a single-run drive's outcome the way the per-value loop's tail does:
/// a mismatch or raise is reported to the sink and returned as the failure
/// that ends the request (the reference's `-n`/`-s` run once, so there is no next value).
///
/// `input_line` is the reference's error location for the run's error: the
/// line of the LAST input value the run consumed (`-s '.[]|.b'` over `1\n2\n3\n`
/// reports 3, `-n 'input,(input|.b)'` over `1\n2\n` reports 2), or
/// [`UNKNOWN_INPUT_LINE`] when the run never touched the input (`-n '1|.b'`
/// errors at `<unknown>`), which the CLI renders without a `` `file:line` `` frame.
#[allow(
    clippy::too_many_arguments,
    reason = "one report per single-run outcome; the drives thread the same inventory"
)]
pub(crate) fn report_single_run<Sink: ItemSink>(
    outcome: Option<ValueOutcome>,
    items: u64,
    input_line: u64,
    resources: &ResourceContext<'_>,
    publication: &mut Publication,
    sink: &mut Sink,
) -> Result<SequenceReport, PipelineError<Sink::Error>> {
    // The error frame is the run's own location: the line the drive threaded
    // (the last value a slurp collected, the value an input-family program
    // pulled, or the UNKNOWN sentinel when nothing was pulled) plus the
    // filename of the attached input cursor, when one exists.
    let filename = input_cursor_handle(resources).and_then(jqf_engine::InputSourceHandle::current_filename);
    match outcome {
        Some(ValueOutcome::Mismatch(error)) => {
            let mismatch = error.mismatch;
            sink.report_value_error(error.into_sequence_error(0, input_line, filename))
                .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
            Err(publication.fail(mismatch.into_failure()))
        }
        Some(ValueOutcome::Codec(error)) => Err(report_and_fail_codec(sink, publication, error, input_line, filename)),
        Some(ValueOutcome::Raised(value)) => {
            let reported = value.clone();
            let report = SequenceValueError::try_for_raised(0, input_line, filename, reported)
                .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
            sink.report_value_error(report)
                .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
            Err(publication.fail(PipelineFailure::Raised(RaisedError { value })))
        }
        Some(ValueOutcome::SplitName { index, detail }) => {
            Err(publication.fail(PipelineFailure::SplitName { index, detail }))
        }
        None => Ok(SequenceReport {
            publication: publication.status(),
            items,
            codec_value_errors: 0,
        }),
    }
}

/// The error location a null-first single-run drive reports: the line of the
/// LAST input value the run consumed (the cursor's current line), or
/// [`UNKNOWN_INPUT_LINE`] when the run never pulled from the cursor — the
/// reference's `-n '1|.b'` errors at `<unknown>`. A pull that found the
/// stream empty — the `break` raise — reports the cursor's line, 0, exactly
/// as the reference does.
pub(crate) fn null_first_input_line(resources: &ResourceContext<'_>) -> u64 {
    match input_cursor_handle(resources) {
        Some(handle) if handle.pulls() > 0 => handle.current_line(),
        _ => UNKNOWN_INPUT_LINE,
    }
}

/// The error location a SLURPED single-run drive reports: the line of the
/// slurp array's LAST element — the whole input was consumed, so the location
/// is always known (0 for empty input), never the UNKNOWN sentinel. The slurp
/// drives park the cursor on the last consumed value.
pub(crate) fn slurped_input_line(resources: &ResourceContext<'_>) -> u64 {
    input_cursor_handle(resources).map_or(0, jqf_engine::InputSource::current_line)
}

/// The attached shared input cursor, when one is present.
pub(crate) fn input_cursor_handle<'request>(
    resources: &'request ResourceContext<'_>,
) -> Option<&'request jqf_engine::InputSourceHandle> {
    resources
        .host_extension()?
        .downcast_ref::<jqf_engine::InputSourceHandle>()
}

/// Pulls the next shared-cursor value and marks it current. Takes the host
/// extension out of the request so the pull may mutate the context.
fn pull_marked_input<E>(
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Result<Option<Value>, jqf_engine::InputSourceError>, PipelineError<E>> {
    jqf_engine::with_input_source(resources, |source, resources| {
        let pulled = source.next(resources);
        if matches!(pulled, Ok(Some(_))) {
            source.mark_current();
        }
        pulled
    })
    .ok_or_else(|| input_cursor_failure(publication))
}

/// Runs one compiled program ONCE over `null` with the reference's shared-cursor input
/// family attached — the CLI's `-n`/`--null-input` drive.
///
/// The drive decodes the whole source eagerly (the standing whole-buffer law,
/// shared with [`execute_input_sequence`]) and attaches the cursor, but never
/// pulls it: the reference's `-n` runs the filter exactly once with `null` as the input,
/// and the input family pulls from the cursor within that one run (`.-n
/// 'input'` reads the first stdin value, `. -n '.'` reads nothing at all).
/// Because the drive itself never pulls, a program that never calls the input
/// family leaves the source entirely untouched — which is why the CLI can skip
/// reading stdin for such programs, exactly as the reference's `-n` never parses it.
///
/// The eager decode is the documented divergence corner of this drive: a
/// program that DOES read input over a malformed stream reports the codec
/// error at the drive level (the reference raises it inside `input`, where `try input
/// catch .` can catch it). Recorded in the cleanup ledger with the eager
/// model's other input-family divergences.
#[allow(
    clippy::too_many_arguments,
    reason = "the same boundary inventory execute_input_sequence threads; splitting would duplicate               it, and the null-first drive is one run read as a single obligation"
)]
pub(crate) fn execute_null_first_sequence<Sink: ItemSink>(
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
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut reuse = ReusableAccessSession::new();
    let decoded_seq = decode_eager_sequence(
        &mut provider,
        &mut reuse,
        &handle,
        source,
        files,
        input_format,
        input_dialect,
        policy.cooperative_credits,
        policy.decode.value_separator,
        resources,
        &publication,
    )?;
    let cursor = decoded_seq.cursor(source, 0, None);
    resources.set_host_extension(Box::new(jqf_engine::InputSourceHandle::new(Box::new(cursor))));
    let (outcome, items) = run_one_owned_value(
        program,
        CodecInputOutcome::Result(EngineResult::owned(Value::Null)),
        &factory,
        &mut reused_encoder,
        0,
        policy.max_iterations,
        encoding_policy,
        framing,
        resources,
        sink,
        &mut publication,
    )?;
    report_single_run(
        outcome,
        items,
        null_first_input_line(resources),
        resources,
        &mut publication,
        sink,
    )
}

/// Runs one compiled program over a STREAMING adjacent-value sequence with
/// the reference's shared-cursor input family — [`execute_input_sequence`] with the
/// eager whole-buffer decode replaced by the demand-driven
/// `StreamingInputCursor` (the input-family completion of 058 W4).
///
/// Values are parsed from the live source as the driver or the program's own
/// `input`/`inputs` pulls them: a query that stops pulling stops reading, and
/// bytes the query never demands are never parsed — a truncated tail behind
/// the last demanded value raises nothing, the reference's own laziness over a pipe.
/// Published items are flushed before every driver pull, because the pull may
/// block on a source that never delivers (the flush-cadence law). Pulls driven
/// INSIDE one program run cannot take that cadence — the sink is loaned for
/// the run — so this lane publishes at flush-per-item cadence instead.
///
/// A driver-pull parse refusal stops the request with the message as the
/// failure and the published prefix standing — the whole-read route's
/// stop-on-decode-error law at the streaming cadence. A pull refusal INSIDE
/// the program (`input`/`inputs`) is a catch-eligible raise at the pull site,
/// exactly like the `--stream` event cursor's.
///
/// # Errors
///
/// Returns the pipeline's own failure; the read callback's error text
/// surfaces as the failure message of the pull that hit it.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the same boundary inventory execute_input_sequence threads; splitting would duplicate it, and the streaming drive is one loop read as a single obligation"
)]
pub(crate) fn execute_input_sequence_streaming<Sink, ReadFn>(
    catalog: CodecCatalog<'_, '_>,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    label: &str,
    read: ReadFn,
) -> Result<SequenceReport, PipelineError<Sink::Error>>
where
    Sink: ItemSink,
    ReadFn: FnMut(&mut [u8]) -> Result<usize, String> + 'static,
{
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, false)?;

    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut encoding_policy = policy.encoding();
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
    // Engine-driven pulls block inside a program run, where no driver-pull
    // flush can run; publish-to-observable per item instead.
    encoding_policy.flush_each_item = true;
    let mut reused_encoder = ReusableEncoderSession::new();
    resources.set_host_extension(Box::new(jqf_engine::InputSourceHandle::new(Box::new(
        StreamingInputCursor::new(label, read),
    ))));
    let mut items = 0u64;
    let mut value_index = 0u64;
    let mut last_error: Option<SequenceError> = None;
    let mut codec_value_errors = 0u64;
    loop {
        // The pull may block until the source delivers: published items must
        // be observable first.
        sink.flush()
            .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
        let pulled = pull_marked_input(resources, &publication)?;
        let current = match pulled {
            Ok(current) => current,
            Err(jqf_engine::InputSourceError::Refused(message)) => {
                let value = Value::try_string(&message)
                    .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
                return Err(publication.fail(PipelineFailure::Raised(RaisedError { value })));
            }
            Err(jqf_engine::InputSourceError::Allocation) => {
                return Err(publication.fail(PipelineFailure::Codec(allocation_failure())));
            }
        };
        let Some(current) = current else {
            break;
        };
        let (outcome, advanced) = run_one_owned_value(
            program,
            CodecInputOutcome::Result(EngineResult::owned(current)),
            &factory,
            &mut reused_encoder,
            items,
            policy.max_iterations,
            encoding_policy,
            framing,
            resources,
            sink,
            &mut publication,
        )?;
        items = advanced;
        let input_cursor = resources
            .host_extension()
            .and_then(|extension| extension.downcast_ref::<jqf_engine::InputSourceHandle>())
            .ok_or_else(|| input_cursor_failure::<Sink::Error>(&publication))?;
        let input_line = input_cursor.current_line();
        let filename = input_cursor.current_filename();
        match outcome {
            Some(ValueOutcome::Mismatch(error)) => {
                let mismatch = error.mismatch;
                sink.report_value_error(error.into_sequence_error(value_index, input_line, filename))
                    .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                last_error = Some(SequenceError::Mismatch(mismatch));
            }
            Some(ValueOutcome::Codec(error)) => {
                sink.report_value_error(SequenceValueError::try_for_codec(
                    value_index,
                    input_line,
                    filename,
                    &error,
                ))
                .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                codec_value_errors = codec_value_errors.saturating_add(1);
                last_error = Some(SequenceError::Codec(error));
            }
            Some(ValueOutcome::Raised(value)) => {
                let reported = value.clone();
                let report = SequenceValueError::try_for_raised(value_index, input_line, filename, reported)
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
        value_index = value_index
            .checked_add(1)
            .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
    }
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

/// Runs one compiled program ONCE over `null` with the STREAMING shared input
/// cursor attached — [`execute_null_first_sequence`] with the eager
/// whole-buffer decode replaced by the demand-driven
/// `StreamingInputCursor`.
///
/// A program that never pulls never reads the source at all (the reference's `-n` law),
/// `limit(n; inputs)` reads only as far as its n-th value's bytes, and a
/// pull-time parse refusal is a catch-eligible raise at the pull site rather
/// than a pre-run decode failure — the reference's own timing, closing the whole-read
/// drive's documented eager-decode divergence for this lane.
///
/// # Errors
///
/// Returns the pipeline's own failure; the read callback's error text
/// surfaces as the message of the pull that hit it.
#[allow(
    clippy::too_many_arguments,
    reason = "the same boundary inventory execute_null_first_sequence threads; splitting would duplicate it, and the streaming drive is one run read as a single obligation"
)]
pub(crate) fn execute_null_first_sequence_streaming<Sink, ReadFn>(
    catalog: CodecCatalog<'_, '_>,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    label: &str,
    read: ReadFn,
) -> Result<SequenceReport, PipelineError<Sink::Error>>
where
    Sink: ItemSink,
    ReadFn: FnMut(&mut [u8]) -> Result<usize, String> + 'static,
{
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, false)?;

    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut encoding_policy = policy.encoding();
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
    // Engine-driven pulls block inside the single program run, where no
    // driver-pull flush can run; publish-to-observable per item instead.
    encoding_policy.flush_each_item = true;
    let mut reused_encoder = ReusableEncoderSession::new();
    // B1: derive the pulled-record kept-subtree hint ONCE from the compiled
    // program and hand it to the cursor, so every `inputs` pull decodes
    // field-pruned exactly as far as the fold body reads. Any failure or
    // decline here is `None` — the whole-decode floor byte for byte.
    let prune_hint = pulled_record_prune_hint(program, resources);
    resources.set_host_extension(Box::new(jqf_engine::InputSourceHandle::new(Box::new(
        StreamingInputCursor::new(label, read).with_hint(prune_hint),
    ))));
    let (outcome, items) = run_one_owned_value(
        program,
        CodecInputOutcome::Result(EngineResult::owned(Value::Null)),
        &factory,
        &mut reused_encoder,
        0,
        policy.max_iterations,
        encoding_policy,
        framing,
        resources,
        sink,
        &mut publication,
    )?;
    report_single_run(
        outcome,
        items,
        null_first_input_line(resources),
        resources,
        &mut publication,
        sink,
    )
}

/// Runs one compiled program ONCE over the array of every decoded input value
/// — the CLI's `-s`/`--slurp` drive.
///
/// Every adjacent value is decoded eagerly (the standing whole-buffer law) and
/// collected into one owned array in input order; the program runs once over
/// that array. A cursor IS attached, parked in the reference's POST-SLURP state (`next`
/// at the end, `current` on the LAST value): `input`/`inputs` read nothing and
/// raise the `break` error exactly as at end of stream (as slurped `input`
/// answers), while `input_filename`/`input_line_number`
/// report the last consumed value's file and line — `jq -s 'input_filename'`
/// over `h1 h2` answers `"h2"` and `input_line_number` the last value's line,
/// which requires the parked cursor (jqf previously reported `null`/`0`).
#[allow(
    clippy::too_many_arguments,
    reason = "the same boundary inventory execute_input_sequence threads; splitting would duplicate               it, and the slurp drive is one run read as a single obligation"
)]
#[allow(
    clippy::too_many_lines,
    reason = "one linear slurp path; the arms are the drive's own facets"
)]
pub(crate) fn execute_slurped_sequence<Sink: ItemSink>(
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
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut reuse = ReusableAccessSession::new();
    let decoded_seq = decode_eager_sequence(
        &mut provider,
        &mut reuse,
        &handle,
        source,
        files,
        input_format,
        input_dialect,
        policy.cooperative_credits,
        policy.decode.value_separator,
        resources,
        &publication,
    )?;
    let value_count = decoded_seq.lines.len();
    let mut array = Array::try_new().map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
    for value in decoded_seq.values {
        let value = value.ok_or_else(|| {
            publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "slurp decode produced an empty cursor slot",
                },
            )))
        })?;
        array
            .try_push(value)
            .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
    }
    // Park the cursor on the last consumed value: the array took every value,
    // so the cursor holds none but keeps the line/filename table and the
    // post-slurp position the reference's builtins observe.
    let cursor = OwnedInputCursor {
        values: RefCell::new(Vec::new()),
        lines: decoded_seq.lines,
        filename: if decoded_seq.filenames.is_empty() {
            Some(std::string::ToString::to_string(&source.label()))
        } else {
            None
        },
        filenames: decoded_seq.filenames,
        next: Cell::new(0),
        current: Cell::new(Some(value_count.saturating_sub(1))),
        pulls: Cell::new(0),
    };
    resources.set_host_extension(Box::new(jqf_engine::InputSourceHandle::new(Box::new(cursor))));
    let (outcome, items) = run_one_owned_value(
        program,
        CodecInputOutcome::Result(EngineResult::owned(Value::Array(array))),
        &factory,
        &mut reused_encoder,
        0,
        policy.max_iterations,
        encoding_policy,
        framing,
        resources,
        sink,
        &mut publication,
    )?;
    report_single_run(
        outcome,
        items,
        slurped_input_line(resources),
        resources,
        &mut publication,
        sink,
    )
}

/// Opens and decodes exactly one adjacent value at `start_offset`, without
/// re-charging input, and hands it to the engine-visible interpretation.
///
/// The session is opened through `reuse`, which recycles the previous value's
/// retained workspaces; a value that failed leaves the slot reset, never
/// poisoned, so the next adjacent value decodes exactly as a fresh session
/// would.
pub(crate) fn decode_sequence_item<'source, E>(
    provider: &mut jqf_codec_core::ErasedProvider<'source>,
    reuse: &mut ReusableAccessSession<'source>,
    handle: &jqf_codec_core::AccessHandle<'_>,
    start_offset: u64,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<jqf_engine::CodecInputResult<'source>, PipelineError<E>> {
    let access = provider
        .open_at_reusing(handle, start_offset, reuse, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let receipt = access.physical_route_receipt().ok_or_else(|| {
        publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "opened access session physical receipt",
            },
        )))
    })?;
    let outcome = {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(credits);
        access
            .decode(&mut run)
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
    };
    let engine = jqf_engine::CodecInputResult::try_from_access(outcome)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    set_lazy_deferred(resources, engine.outcome());
    if engine.report().route() != Some(receipt) {
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "access report route matches opened session receipt",
            },
        ))));
    }
    Ok(engine)
}
