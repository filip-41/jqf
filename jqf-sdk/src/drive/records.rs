//! The records drive family: record-stream drives over framed inputs.

use super::{
    AccessRequirement, Array, Box, CodecCatalog, CodecError, CodecInputOutcome, CodecRequirementPolicy,
    CodecRunContext, CompiledProgram, DialectId, EagerSequence, EncodeRequest, EncodedItemReport, EngineResult,
    EngineRun, ErasedEncoderFactory, ErasedProvider, FacadeFraming, FormatId, InputLines, ItemSink, PipelineError,
    PipelineFailure, PipelinePolicy, Publication, PublicationStatus, RECORD_BATCH_ENTRIES, RECORD_BATCH_TARGET_BYTES,
    RaisedError, RecordBatch, RecordBatchLimit, RecordEntry, RecordIssueCode, RecordIssueReport, RecordIssueSeverity,
    RecordPoll, ResolvedSource, ResourceContext, ReusableAccessSession, ReusableEncoderSession, RouteSlot,
    SequenceError, SequenceValueError, StreamStop, String, Value, ValueOutcome, Vec, allocation_failure, checked_delta,
    drive_run_stream, execute_or_rebind_whole, materialize_sequence_value, may_rebind_whole,
    note_single_document_output, null_first_input_line, overflow, publish_all, pushdown_error, report_single_run,
    resume, run_one_owned_value, set_lazy_deferred, try_lower_root_requirement, validate_credits,
};

use core::cell::Cell;

/// Decodes every RECORD of a record-stream provider into its owned VALUE, in
/// order — the record sibling of `decode_source_values` (the record sibling's
/// record half). A record-only format (NDJSON, json-seq, CSV) registers no
/// access-ladder decoder; the provider FRAMES payload ranges and each payload
/// is decoded through the payload codec's ordinary ladder over that exact
/// range (`allow_adjacent_values` must be OFF — a payload is exactly one
/// text). Stops at TWO values: the exactly-one-document law only needs to
/// distinguish 0, 1, and several, and a multi-record file is refused anyway.
/// A framing ISSUE (the recovering profiles) is a decode refusal — a diff
/// file is one clean document or nothing.
#[allow(
    clippy::too_many_arguments,
    reason = "the record decode threads the same boundary inventory the record drives do"
)]
pub fn decode_record_values<E>(
    catalog: CodecCatalog<'_, '_>,
    mut records: jqf_codec_core::ErasedRecordStreamProvider<'_>,
    record_slot: RouteSlot,
    source: ResolvedSource<'_>,
    payload_format: &FormatId,
    payload_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<Vec<Value>, PipelineError<E>> {
    let publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    if policy.decode.allow_adjacent_values {
        // A record payload is exactly one complete text. Decoding it as an
        // adjacent value would silently accept a second value inside one
        // physical record and report only the first.
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record payload decode must not allow adjacent values",
            },
        ))));
    }
    let decoder = catalog
        .decoder(payload_format, payload_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut payload_provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let requirement = try_lower_root_requirement(
        CodecRequirementPolicy::new(policy.decode.validation, policy.decode.diagnostics),
        // Eager (`Some(0)`): every record's payload is materialized into an
        // owned value in full, so deferral would only re-parse each span on
        // materialization. The LAZY default lives in
        // `try_lower_root_requirement`.
        Some(0),
        resources,
    )
    .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let handle = payload_provider
        .bind(&requirement)
        .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?;
    let mut stream = records
        .open_record_route(record_slot, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let limit = RecordBatchLimit::new(RECORD_BATCH_ENTRIES, RECORD_BATCH_TARGET_BYTES).ok_or_else(|| {
        publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record batch limit",
            },
        )))
    })?;
    let mut reuse = ReusableAccessSession::new();
    let mut batch = RecordBatch::new();
    let mut values = Vec::new();
    loop {
        batch.clear();
        let poll = {
            let mut run = CodecRunContext::new(resources);
            run.set_cooperative_credits(policy.cooperative_credits);
            stream
                .poll(limit, &mut batch, &mut run)
                .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        };
        match poll {
            RecordPoll::Pending => {
                resume(resources, policy.cooperative_credits, &publication)?;
                continue;
            }
            RecordPoll::End(_) => break,
            RecordPoll::Filled => {}
        }
        for entry in batch.entries() {
            match entry {
                RecordEntry::Issue(issue) => {
                    // A framing fault is not a document: the file is refused
                    // exactly as the record route would report it, carrying
                    // the issue's identity (code, ordinal, absolute offset)
                    // so the failure reads as words about the record.
                    return Err(publication.fail(PipelineFailure::Codec(record_issue_error(issue))));
                }
                RecordEntry::Record(record) => {
                    let start = record.lease().payload_start();
                    let end = record.lease().payload_end();
                    let item = decode_record_item(
                        &mut payload_provider,
                        &mut reuse,
                        &handle,
                        start,
                        end,
                        policy.cooperative_credits,
                        resources,
                        &publication,
                    )?;
                    values
                        .try_reserve(1)
                        .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
                    values.push(materialize_sequence_value(item, resources, &publication)?);
                    if values.len() >= 2 {
                        batch.release();
                        return Ok(values);
                    }
                }
            }
        }
        batch.release();
    }
    Ok(values)
}

/// The diff-lane framing-fault refusal: the record route's own `InvalidInput`
/// class, but carrying the issue's identity as a message-only diagnostic so
/// the failure names the record instead of surfacing a bare class name. On a
/// diagnostic allocation refusal the bare failure survives — a refused
/// document never gets worse.
fn record_issue_error(issue: &jqf_codec_core::RecordIssue) -> CodecError {
    let base = CodecError::new(jqf_codec_core::CodecFailureKind::InvalidInput);
    let code = match issue.code() {
        jqf_codec_core::RecordIssueCode::BlankRecord => "blank record",
        jqf_codec_core::RecordIssueCode::InitialByteOrderMark => "byte-order mark before the first record",
        jqf_codec_core::RecordIssueCode::BareCarriageReturn => "bare carriage return where the framing law forbids one",
        jqf_codec_core::RecordIssueCode::MalformedPayload => {
            "record payload is not one complete value of the payload format"
        }
        jqf_codec_core::RecordIssueCode::OversizeRecord => "record exceeds the per-record byte ceiling",
        jqf_codec_core::RecordIssueCode::MissingFinalTerminator => "final record ended without its physical terminator",
        jqf_codec_core::RecordIssueCode::TruncatedTopLevelScalar => {
            "possibly truncated top-level scalar (not self-delimiting)"
        }
        jqf_codec_core::RecordIssueCode::UnframedInput => "input never began a text-sequence unit",
    };
    let message = format!("record {} at byte {}: {code}", issue.ordinal().get(), issue.offset());
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
        jqf_source::Namespace::new("pipeline").code("record-issue"),
        jqf_source::Severity::Error,
        &message,
    ) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

/// Completion summary of one record-stream execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordSequenceReport {
    publication: PublicationStatus,
    items: u64,
    records: u64,
    issues: u64,
    error_issues: u64,
    /// How many per-record CODEC failures (`RawNulByte`) the drive continued
    /// past. Under `--strictness strict` any nonzero count forces the
    /// failure class at run end.
    codec_value_errors: u64,
}

impl RecordSequenceReport {
    /// Final publication state across every item and its facade framing.
    #[must_use]
    pub const fn publication(self) -> PublicationStatus {
        self.publication
    }
    /// Ordered items published across every decoded record.
    #[must_use]
    pub const fn items(self) -> u64 {
        self.items
    }
    /// Records whose payload was decoded and run.
    #[must_use]
    pub const fn records(self) -> u64 {
        self.records
    }
    /// Ordinals that produced an issue instead of a value.
    #[must_use]
    pub const fn issues(self) -> u64 {
        self.issues
    }
    /// Issues whose severity FORCES the request's failure class.
    ///
    /// The recovering-dialect exit law lives here: a single error-severity
    /// issue anywhere in the stream makes the request fail even when every
    /// later record succeeds, while advisories alone leave the exit class to
    /// the program's own last-record result.
    #[must_use]
    pub const fn error_issues(self) -> u64 {
        self.error_issues
    }

    /// How many per-record codec failures the drive continued past.
    #[must_use]
    pub const fn codec_value_errors(self) -> u64 {
        self.codec_value_errors
    }
}

/// Executes every physically framed record of one retained source through
/// exact access, engine handoff, and ordered encoding — one program run per
/// record, in ordinal order.
///
/// This is `execute_sequence`'s sibling, and deliberately its near-twin. The
/// adjacent-value drive asks the payload codec where each value ENDED; the
/// record drive is TOLD where each record ends by a framing codec, and decodes
/// the payload over exactly that range with `allow_adjacent_values: false`, so
/// a second value or trailing content inside one record is the payload codec's
/// ordinary trailing-content failure at its true absolute offset.
///
/// Everything else is shared on purpose: ONE payload provider for the whole
/// source (its input bytes charged exactly once), ONE recycled access session
/// reset per record, ONE recycled encoder, ONE shared schema prototype. A
/// second, record-private decode ladder was measured at roughly twice the
/// shared path's cost, so the record route has none.
///
/// Two failure regimes, matching `execute_sequence` exactly. A per-record
/// RUNTIME error is reported to the sink, its published prefix is kept, and the
/// stream CONTINUES; the exit class is the LAST record's class. A payload
/// DECODE failure stops the stream after every earlier record has been
/// published — unless the framing profile recovers, in which case it becomes an
/// ordered error-severity issue and the stream continues, with
/// [`RecordSequenceReport::error_issues`] carrying the forced exit class.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the pipeline keeps each ownership boundary explicit; the record loop is one linear \
              orchestration whose per-record continue-on-error disposition is clearer inline"
)]
pub(crate) fn execute_record_sequence<'source, Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    mut records: jqf_codec_core::ErasedRecordStreamProvider<'source>,
    record_slot: RouteSlot,
    source: ResolvedSource<'source>,
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
) -> Result<RecordSequenceReport, PipelineError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, true)?;

    if policy.decode.allow_adjacent_values {
        // A record payload is exactly one complete text. Decoding it as an
        // adjacent value would silently accept a second value on the same
        // physical line and report only the first.
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record payload decode must not allow adjacent values",
            },
        ))));
    }
    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    // Identity over a record stream: arm the S4 canonicality probe so a
    // canonical payload can echo its source bytes instead of rendering.
    // Rebuilt from the program (AccessRequirement is not Clone); identity
    // is a whole-document requirement with no prune, so the rebuild is the
    // same lowering the caller already passed.
    let identity_requirement = if program.host_io() == jqf_engine::HostIo::Echo && policy.split.is_none() {
        Some(
            program
                .try_requirement(resources)
                .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
                .with_canonicality_probe(true),
        )
    } else {
        None
    };
    let requirement = identity_requirement.as_ref().unwrap_or(requirement);
    let handle = provider
        .bind(requirement)
        .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?;
    let whole_requirement = if may_rebind_whole(program, requirement) {
        Some(
            program
                .try_rebind_whole_requirement(resources)
                .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?,
        )
    } else {
        None
    };
    let whole_handle = match &whole_requirement {
        Some(req) => Some(
            provider
                .bind(req)
                .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?,
        ),
        None => None,
    };
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
    let mut stream = records
        .open_record_route(record_slot, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let limit = RecordBatchLimit::new(RECORD_BATCH_ENTRIES, RECORD_BATCH_TARGET_BYTES).ok_or_else(|| {
        publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record batch limit",
            },
        )))
    })?;
    // The RECOVERING law belongs to the framing route, not to the payload
    // decode request: a record's payload is always decoded strictly (strict
    // JSON is the payload authority), while the framer's own route guarantee
    // says whether a fault ends the stream or becomes an ordered issue.
    let recovering = records
        .record_route_descriptions()
        .iter()
        .find(|description| description.slot() == record_slot)
        .ok_or_else(|| {
            publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::ProviderRouteMismatch,
            )))
        })?
        .bundle()
        .validation()
        == jqf_codec_core::ValidationMode::Recover;
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut reuse = ReusableAccessSession::new();
    let mut batch = RecordBatch::new();
    let mut items = 0u64;
    let mut counted_records = 0u64;
    let mut issues = 0u64;
    let mut error_issues = 0u64;
    let mut codec_value_errors = 0u64;
    let mut last_error: Option<SequenceError> = None;
    let mut lines = InputLines::with_files_or_new(files);
    loop {
        batch.clear();
        let poll = {
            let mut run = CodecRunContext::new(resources);
            run.set_cooperative_credits(policy.cooperative_credits);
            stream
                .poll(limit, &mut batch, &mut run)
                .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        };
        match poll {
            RecordPoll::Pending => {
                resume(resources, policy.cooperative_credits, &publication)?;
                continue;
            }
            RecordPoll::End(_) => break,
            RecordPoll::Filled => {}
        }
        // The batch's entries borrow the retained source, not the stream, so
        // draining it here cannot outlive anything the next poll invalidates.
        let mut index = 0usize;
        while index < batch.len() {
            let (payload_start, payload_end, ordinal, _physical_end) = {
                let entry = batch
                    .entries()
                    .get(index)
                    .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
                match entry {
                    RecordEntry::Issue(issue) => {
                        issues = issues.saturating_add(1);
                        if issue.severity() == RecordIssueSeverity::Error {
                            error_issues = error_issues.saturating_add(1);
                        }
                        let report = RecordIssueReport {
                            ordinal: issue.ordinal().get(),
                            offset: issue.offset(),
                            severity: issue.severity(),
                            code: issue.code(),
                            cause: None,
                        };
                        sink.report_record_issue(report)
                            .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                        index += 1;
                        continue;
                    }
                    RecordEntry::Record(record) => (
                        record.lease().payload_start(),
                        record.lease().payload_end(),
                        record.ordinal().get(),
                        record.physical_end(),
                    ),
                }
            };
            index += 1;
            let engine = match decode_record_item(
                &mut provider,
                &mut reuse,
                &handle,
                payload_start,
                payload_end,
                policy.cooperative_credits,
                resources,
                &publication,
            ) {
                Ok(engine) => engine,
                Err(error) if recovering => {
                    // The recovering profile turns a malformed payload into an
                    // ordered error issue and continues after the record's
                    // physical terminator, with the exit class already forced.
                    let PipelineFailure::Codec(cause) = error.failure() else {
                        return Err(error);
                    };
                    issues = issues.saturating_add(1);
                    error_issues = error_issues.saturating_add(1);
                    sink.report_record_issue(RecordIssueReport {
                        ordinal,
                        offset: payload_start,
                        severity: RecordIssueSeverity::Error,
                        code: RecordIssueCode::MalformedPayload,
                        cause: Some(cause),
                    })
                    .map_err(|sink_error| publication.fail(PipelineFailure::Sink(sink_error)))?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            counted_records = counted_records.saturating_add(1);
            let (codec_outcome, _access_report) = engine.into_parts();
            if let Some(echo) = record_identity_echo(
                program,
                &codec_outcome,
                source,
                payload_start,
                payload_end,
                output_format,
                framing,
                &policy,
                &publication,
            )? {
                echo_record_item(
                    &echo,
                    items,
                    policy.cooperative_credits,
                    resources,
                    sink,
                    &mut publication,
                )?;
                items = items.saturating_add(1);
                last_error = None;
                continue;
            }
            let run = execute_or_rebind_whole(program, codec_outcome, resources, &publication, |resources| {
                let handle = whole_handle.as_ref().ok_or_else(|| {
                    publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "Exact count miss rebinds Whole",
                        },
                    )))
                })?;
                decode_record_item(
                    &mut provider,
                    &mut reuse,
                    handle,
                    payload_start,
                    payload_end,
                    policy.cooperative_credits,
                    resources,
                    &publication,
                )
                .map(|engine| engine.into_parts().0)
            })?;
            let outcome: Option<ValueOutcome> = match run.with_iteration_cap(policy.max_iterations) {
                EngineRun::Suppressed => None,
                EngineRun::Pushdown(error) => Some(ValueOutcome::Mismatch(pushdown_error(error))),
                EngineRun::ReboundWhole => {
                    return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "Exact count miss must rebind Whole before the graph",
                        },
                    ))));
                }
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
                    StreamStop::Halt { status, message } => {
                        return Err(publication.fail(PipelineFailure::Halt { status, message }));
                    }
                },
            };
            // Payload end, not physical end: a json-seq unit's trailing
            // newline is the next line's start. Counting the terminator
            // here puts the error one line past the unit.
            let end = usize::try_from(payload_end.saturating_sub(source.base_offset()))
                .map_err(|_| overflow::<Sink::Error>(&publication))?;
            match outcome {
                Some(ValueOutcome::Mismatch(error)) => {
                    let input_line = lines.at_value_end(source.bytes(), end);
                    let mismatch = error.mismatch;
                    sink.report_value_error(error.into_sequence_error(
                        ordinal,
                        input_line,
                        lines.current_file_label().or(Some(source.label())),
                    ))
                    .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                    last_error = Some(SequenceError::Mismatch(mismatch));
                }
                Some(ValueOutcome::Codec(error)) => {
                    let input_line = lines.at_value_end(source.bytes(), end);
                    sink.report_value_error(SequenceValueError::try_for_codec(
                        ordinal,
                        input_line,
                        lines.current_file_label().or(Some(source.label())),
                        &error,
                    ))
                    .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                    codec_value_errors = codec_value_errors.saturating_add(1);
                    last_error = Some(SequenceError::Codec(error));
                }
                Some(ValueOutcome::Raised(value)) => {
                    let input_line = lines.at_value_end(source.bytes(), end);
                    let reported = value.clone();
                    let report = SequenceValueError::try_for_raised(
                        ordinal,
                        input_line,
                        lines.current_file_label().or(Some(source.label())),
                        reported,
                    )
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
        }
    }
    batch.release();
    match last_error {
        Some(SequenceError::Mismatch(mismatch)) => Err(publication.fail(mismatch.into_failure())),
        Some(SequenceError::Raised(value)) => Err(publication.fail(PipelineFailure::Raised(RaisedError { value }))),
        Some(SequenceError::Codec(error)) => Err(publication.fail(PipelineFailure::Codec(error))),
        Some(SequenceError::SplitName { index, detail }) => {
            Err(publication.fail(PipelineFailure::SplitName { index, detail }))
        }
        None => Ok(RecordSequenceReport {
            publication: publication.status(),
            items,
            records: counted_records,
            issues,
            error_issues,
            codec_value_errors,
        }),
    }
}

/// Runs the source-preserving EDIT lane over a RECORD stream — the record
/// sibling of [`super::edit::execute_source_edit`]. Each framed
/// record payload decodes to a source-backed document (the delimited codec's
/// record extent on the root, raw authored span per field), the
/// program runs once per record with the exactly-one-output law, and the
/// patched payload is published with the record's OWN authored terminator
/// bytes — the authored-terminator splice, which is what keeps a CRLF file
/// CRLF and an unterminated final record unterminated. Under a headered
/// dialect the authored prefix `[0, first_data_start)` (header payload +
/// terminator) is published before the poll loop, because that row is
/// stream-prefix schema, not a record; identity and cell assignment then
/// splice data records as today. The per-record cycle is the shared
/// [`super::edit::edit_document_cycle`]: leaf spans splice through the
/// codec's `render_leaf`, structural growth and shrink through
/// `render_edit_append`/`render_edit_remove`, and any doubt falls to the
/// whole-record re-encode floor, verified by re-decode before publication.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the record-edit drive threads the same boundary inventory the record drives do, and the per-record loop is one linear obligation"
)]
pub(crate) fn execute_record_source_edit<'source, Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    mut records: jqf_codec_core::ErasedRecordStreamProvider<'source>,
    record_slot: RouteSlot,
    source: ResolvedSource<'source>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    _facade_framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<RecordSequenceReport, PipelineError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;

    if policy.decode.allow_adjacent_values {
        // A record payload is exactly one complete text (the record drive's
        // own law); the edit lane re-derives it so the two cannot drift.
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record payload decode must not allow adjacent values",
            },
        ))));
    }
    // The edit lane's requirement is the EAGER WHOLE DOCUMENT, exactly as
    // the non-record edit drive lowers it: the codec never pushed down a
    // prefix, and every record's root must materialize so the diff walk can
    // read its spans.
    let requirement = program
        .try_whole_document_requirement(resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let handle = provider
        .bind(&requirement)
        .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?;
    // The per-document encoder factory lives inside `edit_document_cycle`;
    // the record lane keeps no shared one.
    // The RECOVERING law belongs to the framing route, not to the payload
    // decode request — the same rule the record sequence drive keeps: under
    // a recovering route a malformed payload is an ordered error issue and
    // the drive continues; under a strict one it tears down.
    let recovering = record_route_recovers(&mut records, record_slot, &publication)?;
    let mut stream = records
        .open_record_route(record_slot, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    // The header row is stream-prefix schema, not a record. Identity `--edit`
    // publishes retained record payloads, so the authored prefix — header
    // payload plus its terminator — must land before the first data splice.
    // The extent is the framer's cursor after consume.
    if jqf_codec_delimited::is_headered_delimited_dialect(input_dialect.as_str()) {
        let prefix_end = stream
            .header_physical_end()
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
        if let Some(end) = prefix_end {
            let base = source.base_offset();
            let end = usize::try_from(end.saturating_sub(base)).map_err(|_| overflow::<Sink::Error>(&publication))?;
            let prefix = source
                .bytes()
                .get(..end)
                .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
            if !prefix.is_empty() {
                publish_all(prefix, resources, policy.cooperative_credits, sink, &mut publication)?;
            }
        }
    }
    let limit = RecordBatchLimit::new(RECORD_BATCH_ENTRIES, RECORD_BATCH_TARGET_BYTES).ok_or_else(|| {
        publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record batch limit",
            },
        )))
    })?;
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut reuse = ReusableAccessSession::new();
    let mut batch = RecordBatch::new();
    let mut items = 0u64;
    let mut issues = 0u64;
    let mut error_issues = 0u64;
    let mut lines = InputLines::new();
    loop {
        batch.clear();
        let poll = {
            let mut run = CodecRunContext::new(resources);
            run.set_cooperative_credits(policy.cooperative_credits);
            stream
                .poll(limit, &mut batch, &mut run)
                .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        };
        match poll {
            RecordPoll::Pending => {
                resume(resources, policy.cooperative_credits, &publication)?;
                continue;
            }
            RecordPoll::End(_) => break,
            RecordPoll::Filled => {}
        }
        let mut index = 0usize;
        while index < batch.len() {
            let (payload_start, payload_end, ordinal, physical_end) = {
                let entry = batch
                    .entries()
                    .get(index)
                    .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
                match entry {
                    RecordEntry::Issue(issue) => {
                        issues = issues.saturating_add(1);
                        if issue.severity() == RecordIssueSeverity::Error {
                            error_issues = error_issues.saturating_add(1);
                        }
                        let report = RecordIssueReport {
                            ordinal: issue.ordinal().get(),
                            offset: issue.offset(),
                            severity: issue.severity(),
                            code: issue.code(),
                            cause: None,
                        };
                        sink.report_record_issue(report)
                            .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                        index += 1;
                        continue;
                    }
                    RecordEntry::Record(record) => (
                        record.lease().payload_start(),
                        record.lease().payload_end(),
                        record.ordinal().get(),
                        record.physical_end(),
                    ),
                }
            };
            index += 1;
            let engine = match decode_record_item(
                &mut provider,
                &mut reuse,
                &handle,
                payload_start,
                payload_end,
                policy.cooperative_credits,
                resources,
                &publication,
            ) {
                Ok(engine) => engine,
                Err(error) if recovering => {
                    // Mirror of execute_record_sequence's recovering arm: the
                    // malformed payload becomes an ordered error issue and
                    // the drive continues after the record's terminator, with
                    // the exit class forced by the tally below.
                    let PipelineFailure::Codec(cause) = error.failure() else {
                        return Err(error);
                    };
                    issues = issues.saturating_add(1);
                    error_issues = error_issues.saturating_add(1);
                    sink.report_record_issue(RecordIssueReport {
                        ordinal,
                        offset: payload_start,
                        severity: RecordIssueSeverity::Error,
                        code: RecordIssueCode::MalformedPayload,
                        cause: Some(cause),
                    })
                    .map_err(|sink_error| publication.fail(PipelineFailure::Sink(sink_error)))?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            items = items.saturating_add(1);
            let base = source.base_offset();
            let start = usize::try_from(payload_start.saturating_sub(base))
                .map_err(|_| overflow::<Sink::Error>(&publication))?;
            let end =
                usize::try_from(payload_end.saturating_sub(base)).map_err(|_| overflow::<Sink::Error>(&publication))?;
            let physical = usize::try_from(physical_end.saturating_sub(base))
                .map_err(|_| overflow::<Sink::Error>(&publication))?;
            let input_line = lines.at_value_end(source.bytes(), end);
            // Ruling 4: the record's own authored terminator bytes travel
            // with the patched payload, never the facade's (the record
            // facade suffix is empty by design).
            let framing = FacadeFraming::item_suffix(source.bytes().get(end..physical).unwrap_or(&[]));
            if super::edit::edit_document_cycle::<Sink>(
                engine,
                start,
                end,
                input_line,
                program,
                source,
                catalog,
                input_format,
                input_dialect,
                output_format,
                output_dialect,
                &requirement,
                &mut reused_encoder,
                policy,
                framing,
                resources,
                sink,
                &mut publication,
            )?
            .is_none()
            {
                // A record document is always source-backed, so a cycle that
                // publishes nothing is the lane's own contract violation.
                return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                    jqf_codec_core::CodecFailureKind::InternalContractViolation {
                        contract: "record edit cycle published",
                    },
                ))));
            }
        }
    }
    batch.release();
    Ok(RecordSequenceReport {
        publication: publication.status(),
        items,
        records: items,
        issues,
        error_issues,
        codec_value_errors: 0,
    })
}

/// The eagerly decoded RECORD table the record-backed single-run drives serve
/// from: every record's value, the line its physical terminator falls on, and
/// the framer's own issue tally.
///
/// This is [`EagerSequence`]'s record sibling, and shares its consumers: `-n`
/// attaches the values as a cursor, `-s` collects them into one array. What
/// differs is only WHO says where a value ends — the framer here, the payload
/// codec there.
pub(crate) struct EagerRecords {
    sequence: EagerSequence,
    records: u64,
    issues: u64,
    error_issues: u64,
}

/// The table one eager record decode builds, and the tallies that travel with
/// it.
///
/// Holding these together is what lets the batch absorber be a function of one
/// accumulator rather than of six running locals.
pub(crate) struct EagerRecordTally<'a> {
    values: Vec<Option<Value>>,
    value_lines: Vec<u64>,
    value_filenames: Vec<(usize, String)>,
    lines: InputLines<'a>,
    records: u64,
    issues: u64,
    error_issues: u64,
}

impl<'a> EagerRecordTally<'a> {
    fn new(files: Option<&'a [jqf_source::SourceFileRange<'a>]>) -> Self {
        Self {
            values: Vec::new(),
            value_lines: Vec::new(),
            value_filenames: Vec::new(),
            lines: InputLines::with_files_or_new(files),
            records: 0,
            issues: 0,
            error_issues: 0,
        }
    }

    /// Counts an issue the caller has already handed to the sink.
    fn count_issue(&mut self, severity: RecordIssueSeverity) {
        self.issues = self.issues.saturating_add(1);
        if severity == RecordIssueSeverity::Error {
            self.error_issues = self.error_issues.saturating_add(1);
        }
    }

    fn finish(self) -> EagerRecords {
        EagerRecords {
            sequence: EagerSequence {
                values: self.values,
                lines: self.value_lines,
                filenames: self.value_filenames,
            },
            records: self.records,
            issues: self.issues,
            error_issues: self.error_issues,
        }
    }
}

/// Whether `record_slot`'s route RECOVERS from a malformed payload rather than
/// stopping the drive.
pub(crate) fn record_route_recovers<E>(
    records: &mut jqf_codec_core::ErasedRecordStreamProvider<'_>,
    record_slot: RouteSlot,
    publication: &Publication,
) -> Result<bool, PipelineError<E>> {
    let validation = records
        .record_route_descriptions()
        .iter()
        .find(|description| description.slot() == record_slot)
        .ok_or_else(|| {
            publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::ProviderRouteMismatch,
            )))
        })?
        .bundle()
        .validation();
    Ok(validation == jqf_codec_core::ValidationMode::Recover)
}

/// Turns one decoded record's engine outcome into the owned value the eager
/// table stores.
pub(crate) fn eager_record_value<E>(
    engine: jqf_engine::CodecInputResult<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Value, PipelineError<E>> {
    let (codec_outcome, _access_report) = engine.into_parts();
    match codec_outcome {
        CodecInputOutcome::Result(EngineResult::Owned(value)) => Ok(value),
        CodecInputOutcome::Result(EngineResult::Located(located)) => located
            .product()
            .document()
            .materialize_node(located.node(), resources)
            .map_err(|_| {
                publication.fail(PipelineFailure::Codec(CodecError::new(
                    jqf_codec_core::CodecFailureKind::InternalContractViolation {
                        contract: "record-sequence value materialization",
                    },
                )))
            }),
        _other => Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record-sequence decode produced a non-result outcome",
            },
        )))),
    }
}

/// Decodes one polled batch's entries into `tally`.
///
/// The issue disposition is [`execute_record_sequence`]'s, unchanged: a
/// recovering profile turns a malformed payload into an ordered error-severity
/// issue and continues; a strict one stops the drive.
#[allow(
    clippy::too_many_arguments,
    reason = "the same boundary inventory the record drive threads; splitting would duplicate it"
)]
pub(crate) fn absorb_record_batch<'source, Sink: ItemSink>(
    batch: &RecordBatch<'source>,
    tally: &mut EagerRecordTally,
    recovering: bool,
    provider: &mut ErasedProvider<'source>,
    reuse: &mut ReusableAccessSession<'source>,
    handle: &jqf_codec_core::AccessHandle<'_>,
    source: ResolvedSource<'source>,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &Publication,
) -> Result<(), PipelineError<Sink::Error>> {
    let mut index = 0usize;
    while index < batch.len() {
        let entry = batch
            .entries()
            .get(index)
            .ok_or_else(|| overflow::<Sink::Error>(publication))?;
        index += 1;
        let (payload_start, payload_end, ordinal, _physical_end) = match entry {
            RecordEntry::Issue(issue) => {
                tally.count_issue(issue.severity());
                sink.report_record_issue(RecordIssueReport {
                    ordinal: issue.ordinal().get(),
                    offset: issue.offset(),
                    severity: issue.severity(),
                    code: issue.code(),
                    cause: None,
                })
                .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
                continue;
            }
            RecordEntry::Record(record) => (
                record.lease().payload_start(),
                record.lease().payload_end(),
                record.ordinal().get(),
                record.physical_end(),
            ),
        };
        let engine = match decode_record_item(
            provider,
            reuse,
            handle,
            payload_start,
            payload_end,
            credits,
            resources,
            publication,
        ) {
            Ok(engine) => engine,
            Err(error) if recovering => {
                let PipelineFailure::Codec(cause) = error.failure() else {
                    return Err(error);
                };
                tally.count_issue(RecordIssueSeverity::Error);
                sink.report_record_issue(RecordIssueReport {
                    ordinal,
                    offset: payload_start,
                    severity: RecordIssueSeverity::Error,
                    code: RecordIssueCode::MalformedPayload,
                    cause: Some(cause),
                })
                .map_err(|sink_error| publication.fail(PipelineFailure::Sink(sink_error)))?;
                continue;
            }
            Err(error) => return Err(error),
        };
        tally.records = tally.records.saturating_add(1);
        let value = eager_record_value(engine, resources, publication)?;
        let end = usize::try_from(payload_end.saturating_sub(source.base_offset()))
            .map_err(|_| overflow::<Sink::Error>(publication))?;
        let line = tally.lines.at_value_end(source.bytes(), end);
        tally
            .value_lines
            .try_reserve(1)
            .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
        tally.value_lines.push(line);
        if let Some(label) = tally.lines.current_file_label() {
            super::cursor::push_filename_run::<Sink::Error>(
                &mut tally.value_filenames,
                tally.values.len(),
                label,
                publication,
            )?;
        }
        tally
            .values
            .try_reserve(1)
            .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
        tally.values.push(Some(value));
    }
    Ok(())
}

/// Decodes every framed record of `records` into an owned value, stopping on
/// the first payload failure the framing profile does not recover from.
///
/// What this does NOT do is publish anything — the single-run drives publish
/// once, after the whole table exists.
#[allow(
    clippy::too_many_arguments,
    reason = "the same boundary inventory the record drive threads; splitting would duplicate it"
)]
pub(crate) fn decode_eager_record_sequence<'source, Sink: ItemSink>(
    records: &mut jqf_codec_core::ErasedRecordStreamProvider<'source>,
    record_slot: RouteSlot,
    provider: &mut ErasedProvider<'source>,
    reuse: &mut ReusableAccessSession<'source>,
    handle: &jqf_codec_core::AccessHandle<'_>,
    source: ResolvedSource<'source>,
    files: Option<&[jqf_source::SourceFileRange<'_>]>,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &Publication,
) -> Result<EagerRecords, PipelineError<Sink::Error>> {
    let recovering = record_route_recovers::<Sink::Error>(records, record_slot, publication)?;
    let mut stream = records
        .open_record_route(record_slot, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let limit = RecordBatchLimit::new(RECORD_BATCH_ENTRIES, RECORD_BATCH_TARGET_BYTES).ok_or_else(|| {
        publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record batch limit",
            },
        )))
    })?;
    let mut batch = RecordBatch::new();
    let mut tally = EagerRecordTally::new(files);
    loop {
        batch.clear();
        let poll = {
            let mut run = CodecRunContext::new(resources);
            run.set_cooperative_credits(credits);
            stream
                .poll(limit, &mut batch, &mut run)
                .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        };
        match poll {
            RecordPoll::Pending => {
                resume(resources, credits, publication)?;
                continue;
            }
            RecordPoll::End(_) => break,
            RecordPoll::Filled => {}
        }
        absorb_record_batch(
            &batch,
            &mut tally,
            recovering,
            provider,
            reuse,
            handle,
            source,
            credits,
            resources,
            sink,
            publication,
        )?;
    }
    batch.release();
    Ok(tally.finish())
}

/// The construction the record SLURP drive uses: the payload provider bound
/// to a WHOLE requirement, the encoder factory, and the eager record table.
///
/// The whole requirement is a LAW, not a default: every record's value is
/// materialized into an owned value here, so a located/projected lowering
/// would bind a scoped route whose outcome this drive cannot interpret. The
/// caller passes the requirement it lowered, and this refuses anything else
/// rather than decoding half a record. The `-n` record drive does not use this table.
pub(crate) struct RecordSingleRun {
    factory: ErasedEncoderFactory,
    table: EagerRecords,
}

#[allow(
    clippy::too_many_arguments,
    reason = "the record drive's own boundary inventory, threaded once for the slurp drive"
)]
pub(crate) fn open_record_single_run<'source, Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    mut records: jqf_codec_core::ErasedRecordStreamProvider<'source>,
    record_slot: RouteSlot,
    source: ResolvedSource<'source>,
    files: Option<&[jqf_source::SourceFileRange<'_>]>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    requirement: &AccessRequirement,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &Publication,
) -> Result<RecordSingleRun, PipelineError<Sink::Error>> {
    if policy.decode.allow_adjacent_values {
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record payload decode must not allow adjacent values",
            },
        ))));
    }
    if !requirement.footprint().is_whole() {
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record single-run decode requires a whole-document requirement",
            },
        ))));
    }
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
    let mut reuse = ReusableAccessSession::new();
    let table = decode_eager_record_sequence(
        &mut records,
        record_slot,
        &mut provider,
        &mut reuse,
        &handle,
        source,
        files,
        policy.cooperative_credits,
        resources,
        sink,
        publication,
    )?;
    Ok(RecordSingleRun { factory, table })
}

/// Runs one compiled program ONCE over the array of every framed RECORD — the
/// CLI's `-s`/`--slurp` drive over a record stream.
///
/// [`execute_slurped_sequence`]'s record sibling: same collection law, same
/// post-slurp input state (no cursor is attached, so `input`/`inputs` raise
/// `break` exactly as the reference's do after a slurp), and the same one run over one
/// array. Only the framing authority differs, which is the whole point — a CSV
/// or NDJSON stream can now be summed, counted, or grouped in one process.
#[allow(
    clippy::too_many_arguments,
    reason = "the record drive's boundary inventory; the slurp drive is one run read as a single \
              obligation"
)]
pub(crate) fn execute_slurped_record_sequence<'source, Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    records: jqf_codec_core::ErasedRecordStreamProvider<'source>,
    record_slot: RouteSlot,
    source: ResolvedSource<'source>,
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
) -> Result<RecordSequenceReport, PipelineError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    let encoding_policy = policy.encoding();
    let run = open_record_single_run(
        catalog,
        records,
        record_slot,
        source,
        files,
        input_format,
        input_dialect,
        requirement,
        output_format,
        output_dialect,
        policy,
        resources,
        sink,
        &publication,
    )?;
    let mut array = Array::try_new().map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
    for value in run.table.sequence.values {
        let value = value.ok_or_else(|| {
            publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "record slurp decode produced an empty cursor slot",
                },
            )))
        })?;
        array
            .try_push(value)
            .map_err(|_| publication.fail(PipelineFailure::Codec(allocation_failure())))?;
    }
    // The whole record stream was consumed, so the error location is the LAST
    // record's line — the adjacent-value slurp's law over the record table
    // (0 when the stream was empty).
    let input_line = run.table.sequence.lines.last().copied().unwrap_or(0);
    let mut reused_encoder = ReusableEncoderSession::new();
    let (outcome, items) = run_one_owned_value(
        program,
        CodecInputOutcome::Result(EngineResult::owned(Value::Array(array))),
        &run.factory,
        &mut reused_encoder,
        0,
        policy.max_iterations,
        encoding_policy,
        framing,
        resources,
        sink,
        &mut publication,
    )?;
    report_single_run(outcome, items, input_line, resources, &mut publication, sink).map(|report| {
        RecordSequenceReport {
            publication: report.publication,
            items: report.items,
            records: run.table.records,
            issues: run.table.issues,
            error_issues: run.table.error_issues,
            codec_value_errors: report.codec_value_errors,
        }
    })
}

/// Runs one compiled program ONCE over `null` with the RECORD stream attached
/// as the reference's shared input cursor — the CLI's `-n`/`--null-input` drive over a
/// record stream.
///
/// [`execute_null_first_sequence`]'s record sibling. Each `input`/`inputs`
/// pull frames and decodes the next record through the payload codec's
/// ordinary access ladder (`open_range_reusing`) and drops the previous
/// record's document; records the program never pulls are never decoded. A
/// malformed record raises at the pull site (catch-eligible), not as a
/// pre-run setup failure.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the record drive's boundary inventory; the null-first drive is one run read as a \
              single obligation"
)]
pub(crate) fn execute_null_first_record_sequence<'source, Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    mut records: jqf_codec_core::ErasedRecordStreamProvider<'source>,
    record_slot: RouteSlot,
    source: ResolvedSource<'source>,
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
) -> Result<RecordSequenceReport, PipelineError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    note_single_document_output(catalog, output_format, output_dialect, &mut publication, true)?;

    if policy.decode.allow_adjacent_values {
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record payload decode must not allow adjacent values",
            },
        ))));
    }
    if !requirement.footprint().is_whole() {
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record single-run decode requires a whole-document requirement",
            },
        ))));
    }
    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
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
    let limit = RecordBatchLimit::new(RECORD_BATCH_ENTRIES, RECORD_BATCH_TARGET_BYTES).ok_or_else(|| {
        publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record batch limit",
            },
        )))
    })?;
    let stream = records
        .open_record_route(record_slot, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let cursor = RecordInputCursor {
        stream,
        batch: RecordBatch::new(),
        batch_index: 0,
        eof: false,
        provider,
        reuse: ReusableAccessSession::new(),
        requirement,
        source,
        lines: InputLines::with_files_or_new(files),
        filename: std::string::ToString::to_string(&source.label()),
        credits: policy.cooperative_credits,
        limit,
        pulled_line: Cell::new(0),
        marked_line: Cell::new(None),
        pulls: Cell::new(0),
        records: 0,
        issues: 0,
        error_issues: 0,
    };
    resources.set_host_extension(Box::new(jqf_engine::InputSourceHandle::new(Box::new(
        into_static_record_cursor(cursor),
    ))));
    let mut reused_encoder = ReusableEncoderSession::new();
    let run = run_one_owned_value(
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
    );
    let input_line = null_first_input_line(resources);
    let (records, issues, error_issues) = resources
        .host_extension()
        .and_then(|extension| extension.downcast_ref::<jqf_engine::InputSourceHandle>())
        .map_or((0, 0, 0), jqf_engine::InputSource::record_progress);
    let _ = resources.take_host_extension();
    let (outcome, items) = run?;
    report_single_run(outcome, items, input_line, resources, &mut publication, sink).map(|report| {
        RecordSequenceReport {
            publication: report.publication,
            items: report.items,
            records,
            issues,
            error_issues,
            codec_value_errors: report.codec_value_errors,
        }
    })
}

/// The `-n` record cursor: one framed payload is decoded per `input`/`inputs`
/// pull through the payload codec's ordinary access ladder, and the previous
/// record's document is dropped when the next pull reopens the recycled
/// session. The host-extension box is `'static`, so the drive transmutes the
/// source/requirement borrows and takes the extension back before those
/// borrows end.
struct RecordInputCursor<'source, 'req> {
    stream: jqf_codec_core::ErasedRecordStreamSession<'source>,
    batch: RecordBatch<'source>,
    batch_index: usize,
    eof: bool,
    provider: ErasedProvider<'source>,
    reuse: ReusableAccessSession<'source>,
    requirement: &'req AccessRequirement,
    source: ResolvedSource<'source>,
    lines: InputLines<'req>,
    filename: String,
    credits: u32,
    limit: RecordBatchLimit,
    pulled_line: Cell<u64>,
    marked_line: Cell<Option<u64>>,
    pulls: Cell<u64>,
    records: u64,
    issues: u64,
    error_issues: u64,
}

fn into_static_record_cursor<'source, 'req>(
    cursor: RecordInputCursor<'source, 'req>,
) -> RecordInputCursor<'static, 'static> {
    // SAFETY: the host-extension box is `'static`, but this cursor only
    // borrows the retained source, the record stream, and the requirement
    // for the `-n` drive. `execute_null_first_record_sequence` takes
    // the extension back before those borrows end, so the `'static` stamps
    // never outlive the data.
    unsafe { core::mem::transmute::<RecordInputCursor<'source, 'req>, RecordInputCursor<'static, 'static>>(cursor) }
}

fn codec_to_pull(codec: &CodecError) -> jqf_engine::InputSourceError {
    match codec.kind() {
        jqf_codec_core::CodecFailureKind::AllocationFailure | jqf_codec_core::CodecFailureKind::Overflow => {
            jqf_engine::InputSourceError::Allocation
        }
        _ => jqf_engine::InputSourceError::Refused(std::format!("{codec}")),
    }
}

fn pipeline_to_pull<E>(error: &PipelineError<E>) -> jqf_engine::InputSourceError {
    match error.failure() {
        PipelineFailure::Codec(codec) => codec_to_pull(codec),
        _ => jqf_engine::InputSourceError::Allocation,
    }
}

fn resume_pull(resources: &mut ResourceContext<'_>, credits: u32) -> Result<(), jqf_engine::InputSourceError> {
    match resources.try_begin_next_cooperative_entry(credits) {
        Ok(true) => Ok(()),
        Ok(false) | Err(_) => Err(jqf_engine::InputSourceError::Allocation),
    }
}

fn issue_pull_message(code: RecordIssueCode) -> String {
    String::from(match code {
        RecordIssueCode::BlankRecord => "blank record",
        RecordIssueCode::InitialByteOrderMark => "initial byte-order mark",
        RecordIssueCode::BareCarriageReturn => "bare carriage return",
        RecordIssueCode::MalformedPayload => "malformed payload",
        RecordIssueCode::OversizeRecord => "oversize record",
        RecordIssueCode::MissingFinalTerminator => "missing final terminator",
        RecordIssueCode::TruncatedTopLevelScalar => "truncated top-level scalar",
        RecordIssueCode::UnframedInput => "unframed input",
    })
}

impl RecordInputCursor<'_, '_> {
    fn fill_batch(&mut self, resources: &mut ResourceContext<'_>) -> Result<bool, jqf_engine::InputSourceError> {
        if self.batch_index < self.batch.len() {
            return Ok(true);
        }
        if self.eof {
            return Ok(false);
        }
        self.batch.clear();
        loop {
            let poll = {
                let mut run = CodecRunContext::new(resources);
                run.set_cooperative_credits(self.credits);
                self.stream
                    .poll(self.limit, &mut self.batch, &mut run)
                    .map_err(|error| codec_to_pull(&error))?
            };
            match poll {
                RecordPoll::Pending => resume_pull(resources, self.credits)?,
                RecordPoll::End(_) => {
                    self.eof = true;
                    self.batch_index = 0;
                    return Ok(!self.batch.is_empty());
                }
                RecordPoll::Filled => {
                    self.batch_index = 0;
                    return Ok(true);
                }
            }
        }
    }

    fn pull_next(
        &mut self,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<Value>, jqf_engine::InputSourceError> {
        if !self.fill_batch(resources)? {
            return Ok(None);
        }
        let index = self.batch_index;
        self.batch_index = self.batch_index.saturating_add(1);
        let (payload_start, payload_end) = {
            let entry = self
                .batch
                .entries()
                .get(index)
                .ok_or(jqf_engine::InputSourceError::Allocation)?;
            match entry {
                RecordEntry::Issue(issue) => {
                    let code = issue.code();
                    let severity = issue.severity();
                    self.issues = self.issues.saturating_add(1);
                    if severity == RecordIssueSeverity::Error {
                        self.error_issues = self.error_issues.saturating_add(1);
                    }
                    return Err(jqf_engine::InputSourceError::Refused(issue_pull_message(code)));
                }
                RecordEntry::Record(record) => (record.lease().payload_start(), record.lease().payload_end()),
            }
        };
        let handle = self
            .provider
            .bind(self.requirement)
            .map_err(|_| jqf_engine::InputSourceError::Allocation)?;
        let publication = Publication::new();
        let engine = decode_record_item::<()>(
            &mut self.provider,
            &mut self.reuse,
            &handle,
            payload_start,
            payload_end,
            self.credits,
            resources,
            &publication,
        )
        .map_err(|error| pipeline_to_pull(&error))?;
        let value =
            eager_record_value::<()>(engine, resources, &publication).map_err(|error| pipeline_to_pull(&error))?;
        let end = usize::try_from(payload_end.saturating_sub(self.source.base_offset()))
            .map_err(|_| jqf_engine::InputSourceError::Allocation)?;
        self.pulled_line.set(self.lines.at_value_end(self.source.bytes(), end));
        self.records = self.records.saturating_add(1);
        Ok(Some(value))
    }
}

impl jqf_engine::InputSource for RecordInputCursor<'static, 'static> {
    fn next(&mut self, resources: &mut ResourceContext<'_>) -> Result<Option<Value>, jqf_engine::InputSourceError> {
        self.pulls.set(self.pulls.get().saturating_add(1));
        self.pull_next(resources)
    }

    fn current_filename(&self) -> Option<&str> {
        self.marked_line.get()?;
        Some(self.lines.current_file_label().unwrap_or(self.filename.as_str()))
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

    fn record_progress(&self) -> (u64, u64, u64) {
        (self.records, self.issues, self.error_issues)
    }
}

/// One record whose source bytes may be published instead of rendered.
struct RecordEcho<'bytes> {
    payload: &'bytes [u8],
    suffix: &'static [u8],
    truthy: Option<bool>,
    empty_array: Option<bool>,
}

/// The S4 canonicality gate on the record identity path: a canonical
/// payload's source bytes are the compact render, so they may be echoed.
/// Non-canonical records (interior whitespace, duplicate keys, exponents,
/// non-minimal escapes) return `None` and the caller renders as today.
#[allow(
    clippy::too_many_arguments,
    reason = "one gate over the whole record identity decision: program, outcome, source window, format, framing, policy, and publication are each an independent input the S4 law reads"
)]
fn record_identity_echo<'source, E>(
    program: &CompiledProgram,
    outcome: &CodecInputOutcome<'source>,
    source: ResolvedSource<'source>,
    payload_start: u64,
    payload_end: u64,
    output_format: &FormatId,
    framing: FacadeFraming<'_>,
    policy: &PipelinePolicy<'_>,
    publication: &Publication,
) -> Result<Option<RecordEcho<'source>>, PipelineError<E>> {
    if program.host_io() != jqf_engine::HostIo::Echo || policy.split.is_some() {
        return Ok(None);
    }
    let Some(suffix) = record_echo_suffix(output_format, framing, policy) else {
        return Ok(None);
    };
    let CodecInputOutcome::Result(result) = outcome else {
        return Ok(None);
    };
    let EngineResult::Located(located) = result else {
        return Ok(None);
    };
    if !located.product().document().source_canonical() {
        return Ok(None);
    }
    let base = source.base_offset();
    let start = usize::try_from(payload_start.saturating_sub(base)).map_err(|_| overflow::<E>(publication))?;
    let end = usize::try_from(payload_end.saturating_sub(base)).map_err(|_| overflow::<E>(publication))?;
    let Some(payload) = source.bytes().get(start..end) else {
        return Ok(None);
    };
    // The same one-walk fact read encode_one publishes with: a second
    // spelling here would let the echo's exit-status verdict drift from the
    // encoded path's. A failed document view degrades to no verdict, exactly
    // as the encode path's `.ok()` does.
    let facts = jqf_engine::publication_facts(result).ok();
    Ok(Some(RecordEcho {
        payload,
        suffix,
        truthy: facts.as_ref().map(|facts| facts.truthy),
        empty_array: facts.as_ref().map(|facts| facts.empty_array),
    }))
}

/// Framing appended after an echoed payload: the facade suffix for JSON
/// output, the NDJSON encoder terminator for NDJSON output. Any other
/// target, or a formatting flag that rewrites bytes, declines the echo.
fn record_echo_suffix(
    output_format: &FormatId,
    framing: FacadeFraming<'_>,
    policy: &PipelinePolicy<'_>,
) -> Option<&'static [u8]> {
    if let Some(options) = policy
        .encode_options
        .and_then(|payload| payload.downcast_ref::<jqf_codec_json::JsonEncodeOptions>())
        && (options.indent != jqf_codec_json::JsonIndent::Compact
            || options.sort_keys
            || options.ascii_output
            || options.raw_strings
            || options.raw_output_nul)
    {
        return None;
    }
    match output_format.as_str() {
        jqf_codec_core::JSON_FORMAT_ID => static_item_suffix(framing.item_suffix),
        jqf_codec_core::NDJSON_FORMAT_ID => {
            let terminator = policy
                .encode_options
                .and_then(|payload| payload.downcast_ref::<jqf_codec_json::ndjson::NdjsonEncodeOptions>())
                .map_or(jqf_codec_json::ndjson::NdjsonTerminator::Lf, |options| {
                    options.canonical_terminator()
                });
            Some(terminator.bytes())
        }
        _ => None,
    }
}

fn static_item_suffix(bytes: &[u8]) -> Option<&'static [u8]> {
    match bytes {
        [] => Some(b""),
        b"\n" => Some(b"\n"),
        b"\r\n" => Some(b"\r\n"),
        [0] => Some(b"\0"),
        _ => None,
    }
}

fn echo_record_item<Sink: ItemSink>(
    echo: &RecordEcho<'_>,
    index: u64,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<(), PipelineError<Sink::Error>> {
    sink.begin_item(index)
        .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    publication.item_open = true;
    let item_start = publication.published_bytes;
    publish_all(echo.payload, resources, credits, sink, publication)?;
    let codec_bytes = checked_delta::<Sink::Error>(publication.published_bytes, item_start, publication)?;
    let framing_start = publication.published_bytes;
    publish_all(echo.suffix, resources, credits, sink, publication)?;
    let framing_bytes = checked_delta::<Sink::Error>(publication.published_bytes, framing_start, publication)?;
    sink.finish_item(
        index,
        EncodedItemReport {
            physical_encoder: jqf_codec_core::PhysicalRouteId::UNSPECIFIED,
            preservation: None,
            codec_bytes,
            framing_bytes,
            value_truthy: echo.truthy,
            value_empty_array: echo.empty_array,
            raw_text_root: false,
        },
    )
    .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    publication.item_open = false;
    publication.completed_items = publication
        .completed_items
        .checked_add(1)
        .ok_or_else(|| overflow::<Sink::Error>(publication))?;
    Ok(())
}

/// Opens and decodes exactly one record payload over its EXACT byte range,
/// without re-charging input.
///
/// The range is the framing codec's proof, so the payload is a COMPLETE
/// document over those bytes: trailing content inside one record fails here
/// exactly as it would for a one-document request, at its true absolute offset.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors decode_sequence_item's explicit ownership boundaries"
)]
pub(crate) fn decode_record_item<'source, E>(
    provider: &mut ErasedProvider<'source>,
    reuse: &mut ReusableAccessSession<'source>,
    handle: &jqf_codec_core::AccessHandle<'_>,
    payload_start: u64,
    payload_end: u64,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<jqf_engine::CodecInputResult<'source>, PipelineError<E>> {
    let access = provider
        .open_range_reusing(handle, payload_start, payload_end, reuse, resources)
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
