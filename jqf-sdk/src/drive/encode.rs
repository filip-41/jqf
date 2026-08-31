//! The encode drive family: publication of encoded items through item sinks.
//!
//! Also the Whole-rebind helpers ([`execute_or_rebind_whole`]) the value,
//! sequence, and record drives use when JSON Exact count misses: Exact
//! republished the selection as root, so the graph cannot run on that product.

use super::{
    AccessRequirement, CodecCatalog, CodecError, CodecInputOutcome, CodecInputResult, CodecRunContext, CompiledProgram,
    DialectId, EncodeItem, EncodedItemReport, EngineResult, EngineRun, EngineRunError, FacadeFraming, FormatId,
    ItemSink, OrderedEncodingPolicy, PipelineError, PipelineFailure, PipelinePolicy, Publication, ResolvedSource,
    ResourceContext, ResourceError, ReusableEncoderSession, RunError, RunPoll, RuntimeError, RuntimeMismatch, String,
    Value, ValueKind, WorkAdmission,
};
use jqf_codec_core::{AccessResultKind, CodecFailureKind};

/// The SDK's bounded [`jqf_codec_core::ByteSink`] implementation: publishes
/// encoder output through the request's own sink in bounded chunks (the same
/// 64 KiB `publish_once`'s `MAX_HOST_WRITE` caps each host write at), doing the
/// same resource accounting and control checks at the same boundaries. A
/// write accepts every byte — partial writes are resolved internally, chunk
/// by chunk — so the encoder never sees backpressure.
///
/// A failed write latches the [`PipelineError`] and returns a marker codec
/// error; the caller reads [`Self::take_error`] after the encoder call so the
/// failure keeps its exact [`PipelineFailure`] class (sink vs codec vs
/// resource).
pub(crate) struct PublishSink<'a, Sink: ItemSink> {
    sink: &'a mut Sink,
    credits: u32,
    publication: &'a mut Publication,
    error: Option<PipelineError<Sink::Error>>,
}

impl<'a, Sink: ItemSink> PublishSink<'a, Sink> {
    fn new(sink: &'a mut Sink, credits: u32, publication: &'a mut Publication) -> Self {
        Self {
            sink,
            credits,
            publication,
            error: None,
        }
    }

    /// The latched failure of the first failed write, if any.
    pub(crate) fn take_error(&mut self) -> Option<PipelineError<Sink::Error>> {
        self.error.take()
    }
}

impl<Sink: ItemSink> jqf_codec_core::ByteSink for PublishSink<'_, Sink> {
    fn write(&mut self, bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<usize, CodecError> {
        if self.error.is_some() {
            return Err(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "byte sink write after failure",
                },
            ));
        }
        if let Err(error) = publish_all(bytes, resources, self.credits, self.sink, self.publication) {
            self.error = Some(error);
            return Err(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "byte sink publication failed (latched)",
                },
            ));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> Result<(), CodecError> {
        Ok(())
    }
}

/// Splits one engine run error into the channel its host reports it on.
///
/// Shared by both residual drives (publishing and folding): they differ in what
/// they do with an ITEM, never in how they classify a failure.
pub(crate) fn split_run_error(error: EngineRunError) -> RunError {
    match error {
        EngineRunError::Codec(codec) => RunError::Machine(codec),
        EngineRunError::Raised(value) => RunError::Raised(value),
        EngineRunError::Halt { status, message } => RunError::Halt { status, message },
        // The strict-dial raise has no jq message of its own (jq
        // answers the value), so the SDK renders the cell name here, in the
        // same vocabulary the CLI prints for a per-value error.
        EngineRunError::MismatchRaised { cell } => RunError::Runtime(RuntimeError {
            mismatch: RuntimeMismatch::MismatchRaised { cell },
            message: mismatch_raised_message(cell),
        }),
        typed => {
            let Some(mismatch) = RuntimeMismatch::of(&typed) else {
                return RunError::Machine(CodecError::new(
                    jqf_codec_core::CodecFailureKind::InternalContractViolation {
                        contract: "engine run error outside the machine, raised, and typed channels",
                    },
                ));
            };
            match typed.typed_message() {
                Ok(Some(message)) => RunError::Runtime(RuntimeError { mismatch, message }),
                // A typed class always has a message; only reserving its buffer
                // can fail, and that is the machine channel.
                Ok(None) | Err(_) => RunError::Machine(allocation_failure()),
            }
        }
    }
}

/// One strict-dial cell raise's jqf message: the reference answers the value,
/// so there is no jq text to match — the message names the cell the request's
/// policy refused.
pub(crate) fn mismatch_raised_message(cell: u16) -> String {
    let name = jqf_resource::policy::MISMATCH_CELL_NAMES
        .get(usize::from(cell))
        .copied()
        .unwrap_or("<unknown-cell>");
    std::format!("mismatch under strict policy: {name}")
}

/// The typed error for one PUSHED-DOWN mismatch, which the engine renders in the
/// same vocabulary a mid-residual failure uses, so both reach the sink through
/// [`split_run_error`] rather than through two parallel conversions.
pub(crate) fn pushdown_error(error: EngineRunError) -> RuntimeError {
    match split_run_error(error) {
        RunError::Runtime(runtime) => runtime,
        // A pushdown mismatch is always a typed class; the machine and raised
        // channels are unreachable here, and a rendering that could not reserve
        // still reports the class with an empty message rather than losing it.
        RunError::Machine(_) | RunError::Raised(_) | RunError::Halt { .. } => RuntimeError {
            mismatch: RuntimeMismatch::Index {
                step_index: 0,
                actual_type: ValueKind::Null,
            },
            message: String::new(),
        },
    }
}

/// The allocation-failure machine error, for a rendering that could not reserve.
pub(crate) fn allocation_failure() -> CodecError {
    CodecError::new(jqf_codec_core::CodecFailureKind::AllocationFailure)
}

/// The single-document output refusal: a message-only diagnostic in the
/// pipeline namespace naming the shape (a second document for a format with
/// no multi-document framing), so the failure reads as words about the
/// REQUEST, not a bare class name.
pub(crate) fn single_document_error() -> CodecError {
    // the plain carrier builds fallibly; on refusal the bare
    // failure survives (a bad request never gets worse).
    let base = CodecError::new(jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
        jqf_source::Namespace::new("pipeline").code("representation"),
        jqf_source::Severity::Error,
        "this output format is one document per run; the run produced \
         more than one — encode one document per request, or choose a \
         multi-document output format (yaml, json-seq, ndjson, csv)",
    ) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

/// Stamps one request's publication with the output format's single-document
/// fact: an output format that can hold exactly one document sets the
/// refusal flag from it — its registration declares neither
/// [`RouteCapability::AdjacentValues`] nor [`RouteCapability::Record`].
/// Either capability means the drive can publish several items safely:
/// `AdjacentValues` because the encoder frames every item (yaml, json-seq,
/// ndjson, csv), `Record` only when `record_drive` is true — the record
/// drive knows each item's exact boundary because it framed the records
/// itself, while the byte-stream drives over the same output would emit a
/// concatenation no XML/HTML parser reads back. Called by every drive that
/// can publish MORE than one item (the single-item lanes — source
/// round-trip, edit, range-locate, slurped-record — publish at most one and
/// need no check).
pub(crate) fn note_single_document_output<E>(
    catalog: CodecCatalog<'_, '_>,
    output_format: &FormatId,
    output_dialect: &DialectId,
    publication: &mut Publication,
    record_drive: bool,
) -> Result<(), PipelineError<E>> {
    let capabilities = catalog
        .route_capabilities(output_format, output_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    publication.single_document_output = !(capabilities.contains(&jqf_codec_core::RouteCapability::AdjacentValues)
        || (record_drive && capabilities.contains(&jqf_codec_core::RouteCapability::Record)));
    Ok(())
}

/// The machine failure when the input-sequence cursor extension is missing.
pub(crate) fn input_cursor_failure<E>(publication: &Publication) -> PipelineError<E> {
    publication.fail(PipelineFailure::Codec(CodecError::new(
        jqf_codec_core::CodecFailureKind::InternalContractViolation {
            contract: "input-sequence cursor extension",
        },
    )))
}

#[allow(
    clippy::too_many_arguments,
    reason = "one item keeps its encoder, receipt, framing, resources, and sink explicit"
)]
pub(crate) fn encode_one<Sink: ItemSink>(
    factory: &jqf_codec_core::ErasedEncoderFactory,
    reused_encoder: &mut ReusableEncoderSession,
    result: &EngineResult<'_>,
    index: u64,
    policy: OrderedEncodingPolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<(), PipelineError<Sink::Error>> {
    // The single-document output refusal: an output format without
    // `RouteCapability::AdjacentValues` (TOML, XML, HTML, CBOR) cannot
    // express a SECOND document — no multi-document framing exists, and the
    // blank-line-separated bytes no parser reads back (over two TOML inputs
    // the emitted text is invalid — a duplicate key; disjoint keys silently
    // merge two documents into one). The first document is already published (the
    // standing prefix-keep law); the second is refused before a byte of it
    // is encoded. The flag is set by the multi-item drives from the output
    // format's own registration; the single-document edit lane (whose many
    // encoder sessions are all patches of ONE document) never sets it and is
    // untouched by construction.
    if publication.single_document_output && publication.completed_items >= 1 {
        return Err(publication.fail(PipelineFailure::Codec(single_document_error())));
    }
    // A located value out of a document the on-demand frontier deferred part of
    // cannot be encoded in place: a container span carries no occurrences for
    // the encoder's cursor to walk. It materializes ONCE here and the encoder
    // sees an ordinary owned value. An eager document has no spans, so the
    // default route pays one integer compare per published item.
    let materialized = match result {
        EngineResult::Located(located) if located.product().document().container_span_count() > 0 => Some(
            located
                .product()
                .document()
                .materialize_node(located.node(), resources)
                .map_err(|_| {
                    publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "publishing a container span failed to materialize",
                        },
                    )))
                })?,
        ),
        EngineResult::Located(_) | EngineResult::Owned(_) => None,
    };
    let item = match (materialized.as_ref(), result) {
        (Some(value), _) | (None, EngineResult::Owned(value)) => EncodeItem::owned(value),
        (None, EngineResult::Located(located)) => EncodeItem::try_located(located.product(), located.node())
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?,
    };
    let mut session = factory
        .start_reusing(item, policy.preservation, resources, reused_encoder)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let physical = session.physical_encoder();
    // The split destination: when the policy
    // carries a split program, the item's NAME is evaluated over the item's
    // value before the item boundary — one destination per published item —
    // and the sink receives it through the defaulted `begin_item_named`
    // (which delegates to `begin_item`, so every existing sink is untouched).
    // A refused name (a non-string split result) fails the item before a byte
    // is published, so no item ever opens a destination without its name.
    match policy.split {
        Some(split) => {
            let name = evaluate_split_name::<Sink>(split, result, index, policy, resources, publication)?;
            if let Some(&first) = publication.split_destinations.get(&name) {
                return Err(publication.fail(PipelineFailure::SplitCollision {
                    name,
                    first_index: first,
                    second_index: index,
                }));
            }
            publication.split_destinations.insert(name.clone(), index);
            sink.begin_item_named(index, &name)
                .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
        }
        None => sink
            .begin_item(index)
            .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?,
    }
    publication.item_open = true;
    let item_start = publication.published_bytes;
    drain_encoder(
        &mut session,
        resources,
        policy.cooperative_credits,
        sink,
        publication,
        framing.item_suffix,
    )?;
    let report = session.report().copied();
    session.recycle(reused_encoder);
    let item_end = publication.published_bytes;
    // Cancellation must stop publication BEFORE the item is counted: the
    // item's own final write may be the one that flips the host control (the
    // facade suffix folds into it), and reporting Complete after a cancelled
    // write breaks the ordered-publication law. Worker sinks keep the morsel
    // exemption — the coordinator owns their stop.
    admit_visible_boundary(
        resources,
        policy.cooperative_credits,
        publication,
        sink.observes_host_progress(),
    )?;
    let framing_len = u64::try_from(framing.item_suffix.len()).map_err(|_| overflow(publication))?;
    let facts = jqf_engine::publication_facts(result);
    let item_report = EncodedItemReport {
        physical_encoder: physical,
        preservation: report,
        codec_bytes: checked_delta(item_end, item_start, publication)?
            .checked_sub(framing_len)
            .ok_or_else(|| overflow(publication))?,
        framing_bytes: framing_len,
        // The exit-status fact is judged from the VALUE, not the bytes, so the
        // `-r` raw-string corner (`-e` on the string "false" exits 0, on the
        // boolean false exits 1 — the same five bytes) stays exact. A failed
        // document view is an internal contract violation the same shape the
        // materialization arm above treats as one; the item still publishes,
        // carrying no exit-status verdict.
        value_truthy: facts.as_ref().map(|facts| facts.truthy).ok(),
        // The `--diff` emptiness verdict rides the same report: the diff
        // program emits one array per run, and an empty array is the
        // equality law (emit nothing on equality) made observable.
        value_empty_array: facts.as_ref().map(|facts| facts.empty_array).ok(),
        // The raw-text fact is the value side of the `-r` raw arm law, judged
        // from the VALUE like the exit-status fact: a raw-printed root text's
        // bytes are the string itself (no quotes, no escapes), which the CLI's
        // colour rendering must leave untouched. A failed document view is the
        // same internal-contract class the truthiness arm treats as one; the
        // item still publishes, carrying no raw-text verdict (it renders as
        // ordinary JSON, which the encoder's own view would have refused).
        raw_text_root: facts.map(|facts| facts.raw_text).ok().unwrap_or(false),
    };
    sink.finish_item(index, item_report)
        .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    publication.item_open = false;
    // The live-tail lanes' cadence: publish-to-observable per item, so a
    // program that next blocks on input never strands already-published
    // bytes behind a host buffer.
    if policy.flush_each_item {
        sink.flush()
            .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    }
    publication.completed_items = publication
        .completed_items
        .checked_add(1)
        .ok_or_else(|| overflow(publication))?;
    Ok(())
}

pub(crate) fn checked_delta<E>(end: u64, start: u64, publication: &Publication) -> Result<u64, PipelineError<E>> {
    end.checked_sub(start).ok_or_else(|| overflow(publication))
}

pub(crate) fn overflow<E>(publication: &Publication) -> PipelineError<E> {
    publication.fail(PipelineFailure::Codec(CodecError::new(
        jqf_codec_core::CodecFailureKind::Overflow,
    )))
}

pub(crate) fn validate_credits<E>(credits: u32, publication: &Publication) -> Result<(), PipelineError<E>> {
    if credits == 0 || credits > 4_096 {
        Err(publication.fail(PipelineFailure::InvalidCooperativeCredits))
    } else {
        Ok(())
    }
}

/// Guards `execute_sequence`'s adjacent-value loop against a provider that
/// reports no consumed offset, or a consumed offset of zero: either would
/// leave `offset` unchanged and loop forever re-decoding the same start
/// position. Both are treated as the same internal-contract violation.
pub(crate) fn require_forward_progress<E>(
    consumed: Option<u64>,
    publication: &Publication,
) -> Result<u64, PipelineError<E>> {
    match consumed {
        None | Some(0) => Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "adjacent-value route opened without reporting a consumed offset",
            },
        )))),
        Some(consumed) => Ok(consumed),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "access keeps selection, exact requirement, and request authority explicit"
)]
pub(crate) fn access_input<'source, E>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'source>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    requirement: &AccessRequirement,
    policy: PipelinePolicy<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<CodecInputResult<'source>, PipelineError<E>> {
    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let handle = provider
        .bind(requirement)
        .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?;
    let mut access = provider
        .open(&handle, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let receipt = access.physical_route_receipt().ok_or_else(|| {
        publication.fail(PipelineFailure::Codec(CodecError::new(
            CodecFailureKind::InternalContractViolation {
                contract: "opened access session physical receipt",
            },
        )))
    })?;
    let outcome = {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(policy.cooperative_credits);
        access
            .decode(&mut run)
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
    };
    let engine =
        CodecInputResult::try_from_access(outcome).map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    if engine.report().route() != Some(receipt) {
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            CodecFailureKind::InternalContractViolation {
                contract: "access report route matches opened session receipt",
            },
        ))));
    }
    Ok(engine)
}

/// Programs whose execute may return [`EngineRun::ReboundWhole`]. Bind the
/// Whole handle once per drive. Keys off the packed Exact+count plan, not
/// a second count-demand walk. Skip when the codec bind is already Whole
/// (non-lenient lowerings rebind Whole at requirement time).
pub(crate) fn may_rebind_whole(program: &CompiledProgram, requirement: &AccessRequirement) -> bool {
    program.may_rebind_whole() && !requirement.footprint().is_whole()
}

/// [`CompiledProgram::execute`], then Whole decode + Whole graph on JSON Exact
/// count miss. `rebind` must decode the same value under Whole access.
pub(crate) fn execute_or_rebind_whole<'program, 'source, E>(
    program: &'program CompiledProgram,
    outcome: CodecInputOutcome<'source>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
    rebind: impl FnOnce(&mut ResourceContext<'_>) -> Result<CodecInputOutcome<'source>, PipelineError<E>>,
) -> Result<EngineRun<'program, 'source>, PipelineError<E>> {
    match program
        .execute(outcome, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
    {
        EngineRun::ReboundWhole => program
            .try_run_whole_value(rebind(resources)?, resources)
            .map_err(|error| publication.fail(PipelineFailure::Codec(error))),
        run => Ok(run),
    }
}

/// A Located requirement whose codec declined to pack a multi-match range as
/// one document. The caller rebinds the whole-document floor and runs the
/// whole program.
pub(crate) fn located_range_declined<E>(requirement: &AccessRequirement, error: &PipelineError<E>) -> bool {
    requirement.result() == AccessResultKind::Located
        && matches!(
            error.failure(),
            PipelineFailure::Codec(codec) if codec.kind() == CodecFailureKind::RequirementMismatch
        )
}

/// Records the decoded document's deferred container-span count onto the
/// request context, read while the document is alive. The count is a document
/// fact; only the materialized side accumulates on the context during the run.
pub(crate) fn set_lazy_deferred(resources: &ResourceContext<'_>, engine: &CodecInputOutcome<'_>) {
    let deferred = match engine {
        CodecInputOutcome::Result(EngineResult::Located(located)) => {
            located.product().document().container_span_count()
        }
        _ => 0,
    };
    resources.set_lazy_deferred_spans(deferred);
}

pub(crate) fn resume<E>(
    resources: &mut ResourceContext<'_>,
    credits: u32,
    publication: &Publication,
) -> Result<(), PipelineError<E>> {
    let resumed = resources
        .try_begin_next_cooperative_entry(credits)
        .map_err(|error| publication.fail(PipelineFailure::Codec(CodecError::from(error))))?;
    if !resumed {
        return Err(publication.fail(PipelineFailure::InvalidCooperativeCredits));
    }
    Ok(())
}

/// Encodes the whole item straight-line through a bounded publish sink:
/// the encoder writes as it produces, the sink publishes in the
/// same 64 KiB chunks `publish_once` bounds each host write to, and a failure keeps its
/// exact [`PipelineFailure`] class via the sink's latch.
pub(crate) fn drain_encoder<Sink: ItemSink>(
    session: &mut jqf_codec_core::ErasedEncoderSession<'_, '_>,
    resources: &mut ResourceContext<'_>,
    credits: u32,
    sink: &mut Sink,
    publication: &mut Publication,
    item_suffix: &[u8],
) -> Result<(), PipelineError<Sink::Error>> {
    let mut publish = PublishSink::new(sink, credits, publication);
    let fold_suffix = item_suffix.len() <= 8;
    let mut leftover_buf = [0_u8; 8];
    let mut leftover_len = 0_usize;
    let encoded = {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(credits);
        if fold_suffix {
            run.set_item_trailer(item_suffix);
        }
        let encoded = session.encode(&mut publish, &mut run);
        if fold_suffix {
            let leftover = run.item_trailer();
            leftover_len = leftover.len();
            leftover_buf[..leftover_len].copy_from_slice(leftover);
        }
        encoded
    };
    if let Some(error) = publish.take_error() {
        return Err(error);
    }
    encoded.map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let leftover: &[u8] = if fold_suffix {
        &leftover_buf[..leftover_len]
    } else {
        item_suffix
    };
    if leftover.is_empty() {
        Ok(())
    } else {
        publish_all(leftover, resources, credits, sink, publication)
    }
}

pub(crate) fn publish_all<Sink: ItemSink>(
    mut bytes: &[u8],
    resources: &mut ResourceContext<'_>,
    credits: u32,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<(), PipelineError<Sink::Error>> {
    while !bytes.is_empty() {
        let written = publish_once(bytes, resources, credits, sink, publication)?;
        bytes = &bytes[written..];
    }
    Ok(())
}

pub(crate) fn publish_once<Sink: ItemSink>(
    bytes: &[u8],
    resources: &mut ResourceContext<'_>,
    credits: u32,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<usize, PipelineError<Sink::Error>> {
    const MAX_HOST_WRITE: usize = 64 * 1024;
    let proposed_bytes = &bytes[..bytes.len().min(MAX_HOST_WRITE)];
    admit_visible_boundary(resources, credits, publication, sink.observes_host_progress())?;
    let proposed = u64::try_from(proposed_bytes.len()).map_err(|_| overflow(publication))?;
    let permit = resources
        .reserve_output(proposed)
        .map_err(|error| publication.fail(PipelineFailure::Codec(CodecError::from(error))))?;
    let written = sink
        .write(proposed_bytes)
        .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    if written == 0 || written > proposed_bytes.len() {
        return Err(publication.fail(PipelineFailure::SinkContract));
    }
    let written_u64 = u64::try_from(written).map_err(|_| overflow(publication))?;
    publication.published_bytes = publication
        .published_bytes
        .checked_add(written_u64)
        .ok_or_else(|| overflow(publication))?;
    permit
        .commit(written_u64)
        .map_err(|error: ResourceError| publication.fail(PipelineFailure::Codec(CodecError::from(error))))?;
    Ok(written)
}

pub(crate) fn admit_visible_boundary<E>(
    resources: &mut ResourceContext<'_>,
    credits: u32,
    publication: &Publication,
    check_progress: bool,
) -> Result<(), PipelineError<E>> {
    loop {
        match resources
            .admit_work_transition()
            .map_err(|error| publication.fail(PipelineFailure::Codec(CodecError::from(error))))?
        {
            WorkAdmission::Pending => resume(resources, credits, publication)?,
            WorkAdmission::Granted(_) => {
                if check_progress {
                    resources
                        .check_control()
                        .map_err(|error| publication.fail(PipelineFailure::Codec(CodecError::from(error))))?;
                }
                return Ok(());
            }
        }
    }
}

/// Evaluates the split-destination program over one item's value: runs the program with `$index` bound to the item counter
/// and returns its SINGLE string output — the item's destination name.
///
/// The result law is fixed: the expression must produce exactly one
/// output and it must be a string. A non-string output or an empty stream is
/// the [`PipelineFailure::SplitName`] refusal naming the item index and the
/// produced kind; a split-program runtime failure is the same refusal
/// carrying the rendered message. Either way the item fails BEFORE
/// `begin_item`, so no item publishes a byte toward a destination it never
/// opened.
#[allow(
    clippy::too_many_arguments,
    reason = "one name-per-item seam keeps the program, item, counter, policy, resources, and publication explicit"
)]
fn evaluate_split_name<Sink: ItemSink>(
    program: &jqf_engine::CompiledProgram,
    result: &EngineResult<'_>,
    index: u64,
    policy: OrderedEncodingPolicy<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &mut Publication,
) -> Result<String, PipelineError<Sink::Error>> {
    let outcome = CodecInputOutcome::Result(
        result
            .try_clone()
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?,
    );
    let run = program
        .try_run_split(outcome, index, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let EngineRun::Stream { mut stream, .. } = run else {
        // No codec resolved any pushed-down prefix for the split program, so a
        // suppressed or pushed-down run is an internal contract violation.
        return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "the split program produced a suppressed or pushed-down run",
            },
        ))));
    };
    let mut last: Option<Value> = None;
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Pending) => resume(resources, policy.cooperative_credits, publication)?,
            Ok(RunPoll::Item(item)) => {
                last = Some(materialize_split_item(item, resources, publication)?);
            }
            Ok(RunPoll::Complete) => break,
            Err(error) => {
                return Err(publication.fail(PipelineFailure::SplitName {
                    index,
                    detail: render_split_failure(error),
                }));
            }
        }
    }
    match last {
        Some(Value::String(text)) => Ok(text.to_string()),
        Some(other) => Err(publication.fail(PipelineFailure::SplitName {
            index,
            detail: format!("{} instead of a single string", jqf_engine::kind_name(other.kind())),
        })),
        None => Err(publication.fail(PipelineFailure::SplitName {
            index,
            detail: "no output; the split expression must return a single string".to_owned(),
        })),
    }
}

/// Materializes one split-program output for the name check: Owned passes
/// through, Located materializes the node. Any failure is the same internal
/// contract class the input-sequence materialization keeps.
fn materialize_split_item<E>(
    result: EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Value, PipelineError<E>> {
    match result {
        EngineResult::Owned(value) => Ok(value),
        EngineResult::Located(located) => located
            .product()
            .document()
            .materialize_node(located.node(), resources)
            .map_err(|_| {
                publication.fail(PipelineFailure::Codec(CodecError::new(
                    jqf_codec_core::CodecFailureKind::InternalContractViolation {
                        contract: "split expression value materialization",
                    },
                )))
            }),
    }
}

/// Renders one split-program run failure as the detail text of the
/// [`PipelineFailure::SplitName`] refusal: the engine's own message when the
/// error carries one, the variant name otherwise.
fn render_split_failure(error: EngineRunError) -> String {
    match crate::drive::split_run_error(error) {
        RunError::Runtime(runtime) => runtime.message.clone(),
        RunError::Machine(codec) => format!("the split expression failed ({})", codec.kind()),
        RunError::Raised(_) => "the split expression raised".to_owned(),
        RunError::Halt { status, .. } => format!("the split expression halted (status {status})"),
    }
}
