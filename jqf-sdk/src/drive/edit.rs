//! The edit drive family: source-preserving edit runs over one or many
//! input documents.

use core::ops::ControlFlow;

use super::{
    AccessRequirement, BytePatch, CodecCatalog, CodecError, CodecInputOutcome, CodecRunContext, CompiledProgram,
    DataError, DialectId, Document, DocumentProduct, EncodeItem, EncodeRequest, EncodedItemReport, EngineResult,
    EngineRun, ErasedEncoderFactory, ErasedProvider, FacadeFraming, FactDelta, FactPayloadView, FormatId, InputLines,
    ItemSink, LocalOwnerRef, NodeId, PatchError, PatchSet, PhysicalRouteId, PipelineError, PipelineFailure,
    PipelinePolicy, PreservationRequest, Publication, ResolvedSource, ResourceContext, ReusableAccessSession,
    ReusableEncoderSession, RunError, RunPoll, RuntimeError, SequenceReport, String, ToOwned, Value, ValueKind,
    ValueView, Vec, access_input, admit_visible_boundary, checked_delta, decode_sequence_item, format, overflow,
    publish_all, pushdown_error, report_and_fail_raised, report_and_fail_runtime, require_forward_progress, resume,
    skip_value_separator, split_run_error, validate_credits, values_semantically_equal,
};

/// The outcome of a source-preserving edit attempt.
#[derive(Debug)]
pub enum EditRun {
    /// Every input document was edited and published.
    Completed(SequenceReport),
    /// The lane declined: nothing was published, and the caller reruns its
    /// ordinary route.
    Declined,
}

/// One document's diff classification against its retained source.
pub(crate) enum EditDiff {
    /// Every authored byte already renders the new value: no patch is needed.
    Unchanged,
    /// Disjoint leaf-span replacements that turn the source into the new value.
    Patches(Vec<BytePatch>),
    /// The edit is structural (container shape, key order, a leaf without a
    /// source span): no minimal patch is provable, so the floor re-encodes.
    Structural,
}

/// Runs the program per input document and publishes the whole EDITED document
/// per document, reusing retained source bytes for everything the edit did not
/// touch.
///
/// This is the `inplace_edit` vertical's publication lane. The engine already
/// owns the reference's complete assignment semantics (the `Modify` node family): the lane
/// runs the program, requires EXACTLY ONE output per document (zero or multiple
/// outputs error the document — the multi-version `--edit-multi` surface is a
/// future lane, not a flag on this one), and then publishes the document the
/// program produced:
///
/// - a scalar leaf whose authored source bytes already render the new value is
///   left verbatim, and a changed scalar leaf whose source span the decoder
///   retained is replaced by an exact byte patch;
/// - any structural change — an insertion, deletion, reordered or renamed key,
///   a changed container shape, or a leaf without a retained source span — is
///   beyond the minimal-patch proof, so the lane falls back to the floor: the
///   whole document re-encoded with the caller's output options;
/// - every patch is VERIFIED before publication: the patched document bytes
///   are re-decoded and their rendered form must equal the rendered form of
///   the program's output value, or the lane falls back to the floor. A
///   patch that would publish wrong bytes is a bug, and the verification is
///   the machine-enforced "any doubt → floor" law.
///
/// The lane publishes patched document bytes plus the facade's item suffix
/// (the host owns separators), and reports the physical encoder as
/// [`PhysicalRouteId::UNSPECIFIED`] because the untouched bytes come from the
/// source, not from an encoder.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one lane keeps its provider, encoder, policy, framing, resources, and sink explicit, \
              and the per-document run/diff/publish orchestration is one linear obligation"
)]
pub(crate) fn execute_source_edit<Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'_>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<EditRun, PipelineError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    // Seam 5 : the edit lane never changes format — the CLI already
    // refuses a cross-format edit — so format equality is the only dialect
    // requirement. The input and output dialect NAMESPACES differ by design
    // (`toml-1.0` in, `toml.jqf-1.0@1` out), so the old dialect-equality
    // check could never pass for a TOML/YAML edit even once the parsers bind
    // their source.
    if input_format.as_str() != output_format.as_str() {
        return Ok(EditRun::Declined);
    }
    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let requirement = program
        .try_whole_document_requirement(resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        .with_fact_intent(jqf_codec_core::FactIntent::Preserve);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?;
    // One recycled access session for the whole lane; the ENCODER FACTORY is
    // per document (created inside `edit_document_cycle`). A shared factory
    // would carry the block profile's between-documents `---` fact across
    // documents whose published bytes never touched the encoder (a patched
    // or verbatim document skips it), so a later floored document could lose
    // or double its unit separator. In this lane each unit's framing lives
    // in its own source segment, which the floor republishes verbatim.
    let mut reused_encoder = ReusableEncoderSession::new();
    let mut reuse = ReusableAccessSession::new();
    let mut offset = 0usize;
    let mut lines = InputLines::new();
    if policy.decode.allow_adjacent_values {
        // The adjacent-value drive: the source is a stream of documents whose
        // spans attach to the segment holding exactly their own text. Forward
        // progress is REQUIRED — an adjacent-value route that does not report
        // where its value ended would loop forever over the same bytes.
        loop {
            let start = skip_value_separator(source.bytes(), offset, policy.decode.value_separator);
            if start >= source.bytes().len() {
                break;
            }
            let start_offset = u64::try_from(start).map_err(|_| overflow::<Sink::Error>(&publication))?;
            let item = decode_sequence_item(
                &mut provider,
                &mut reuse,
                &handle,
                start_offset,
                policy.cooperative_credits,
                resources,
                &publication,
            )?;
            let consumed = require_forward_progress::<Sink::Error>(item.report().consumed_offset(), &publication)?;
            let consumed_usize = usize::try_from(consumed).map_err(|_| overflow::<Sink::Error>(&publication))?;
            let end = start
                .checked_add(consumed_usize)
                .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
            let input_line = lines.at_value_end(source.bytes(), end);
            if edit_document_cycle::<Sink>(
                item,
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
                return Err(edit_contract::<Sink::Error>(&publication));
            }
            offset = end;
        }
    } else {
        // The one-shot single-document drive : a
        // single-document format (TOML/YAML) is exactly ONE text per source, so
        // one decode, one diff, one patch set. The decline condition keys on
        // "can this route replay the source" — the retained binding — not on
        // adjacency, so there is no consumed-offset forward-progress
        // requirement (the provider reports none).
        let end = source.bytes().len();
        let input_line = lines.at_value_end(source.bytes(), end);
        let item = decode_sequence_item(
            &mut provider,
            &mut reuse,
            &handle,
            0,
            policy.cooperative_credits,
            resources,
            &publication,
        )?;
        if edit_document_cycle::<Sink>(
            item,
            0,
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
            // Nothing was published: the caller reruns its ordinary route.
            return Ok(EditRun::Declined);
        }
    }
    Ok(EditRun::Completed(SequenceReport {
        publication: publication.status(),
        items: publication.completed_items,
        codec_value_errors: 0,
    }))
}

/// One document's run/diff/publish cycle, shared by the adjacent-value and
/// one-shot single-document edit drives. The drives differ only in HOW a
/// document is decoded and where the next one begins; the run/diff/publish
/// law is the same.
///
/// Returns `Some(next_offset)` when the document was published, or `None`
/// when the lane must DECLINE with nothing published: a modifying program
/// over a document with no retained source segment cannot diff against the
/// source. Only the one-shot drive can hit this before publishing anything;
/// the adjacent-value drive treats it as the contract violation it would be
/// (JSON always binds).
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one lane keeps its provider, encoder, policy, framing, resources, and sink explicit, \
              and the per-document run/diff/publish orchestration is one linear obligation"
)]
pub(crate) fn edit_document_cycle<'source, Sink: ItemSink>(
    item: jqf_engine::CodecInputResult<'source>,
    start: usize,
    end: usize,
    input_line: u64,
    program: &CompiledProgram,
    source: ResolvedSource<'source>,
    catalog: CodecCatalog<'_, '_>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    output_format: &FormatId,
    output_dialect: &DialectId,
    requirement: &AccessRequirement,
    reused_encoder: &mut ReusableEncoderSession,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<Option<usize>, PipelineError<Sink::Error>> {
    let value_index = publication.completed_items;
    // THIS document's own encoder factory (see `execute_source_edit`: a
    // per-document factory keeps the block profile's between-documents
    // separator out of a lane where each unit's framing lives in its own
    // source segment).
    let encoding_policy = policy.encoding();
    let preservation = encoding_policy.preservation;
    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let factory = encoder
        .create_factory(
            EncodeRequest {
                format: output_format,
                dialect: output_dialect,
                diagnostics: encoding_policy.diagnostics,
                preservation,
                options: encoding_policy.options,
            },
            resources,
        )
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    // The run's output is OWNED: the Modify executor materializes its
    // input, so the located document survives the run untouched and is
    // available for the source diff afterwards.
    let (engine, _) = item.into_parts();
    let original = edit_product(&engine)
        .ok_or_else(|| {
            publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "edit lane document authority (outcome)",
                },
            )))
        })?
        .try_clone()
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    // The edit lane's requirement is the EAGER WHOLE DOCUMENT
    // (`try_whole_document_requirement`), so the codec never pushed down a
    // prefix — the whole program must run from the root. `try_run` would
    // skip `prefix_len` steps (the pushdown prefix the program's own
    // requirement would have resolved at the codec), and a skipped read
    // step emits its input unchanged — `--edit '.b'` over an array would
    // silently publish the array instead of raising the index mismatch.
    let run = program
        .try_run_whole_value(engine, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        .with_iteration_cap(policy.max_iterations);
    let (output, fact_deltas) = match run {
        EngineRun::Stream { mut stream, input } => {
            // A missing path is an ordinary jq read (it emits `null`), not
            // an edit failure: the exactly-one-output law below decides.
            let _ = input;
            let mut collected: Option<Value> = None;
            loop {
                match stream.poll(resources) {
                    Ok(RunPoll::Pending) => {
                        resume(resources, policy.cooperative_credits, publication)?;
                    }
                    Ok(RunPoll::Item(result)) => {
                        if collected.is_some() {
                            return Err(publication.fail(PipelineFailure::EditOutputCount { observed: 2 }));
                        }
                        let value = edit_materialize_result::<Sink>(result, resources, publication)?;
                        collected = Some(value);
                    }
                    Ok(RunPoll::Complete) => break,
                    Err(error) => {
                        return Err(match split_run_error(error) {
                            RunError::Machine(error) => publication.fail(PipelineFailure::Codec(error)),
                            RunError::Raised(value) => {
                                report_and_fail_raised(sink, publication, value, input_line, None)
                            }
                            RunError::Halt { status, message } => {
                                publication.fail(PipelineFailure::Halt { status, message })
                            }
                            RunError::Runtime(error) => {
                                report_and_fail_runtime(sink, publication, error, input_line, None)
                            }
                        });
                    }
                }
            }
            let Some(value) = collected else {
                return Err(publication.fail(PipelineFailure::EditOutputCount { observed: 0 }));
            };
            // A fact-write run records its deltas on the stream (the edit
            // lane's span-op channel); drained here, after the
            // exactly-one-output law has been satisfied.
            (value, stream.take_fact_deltas())
        }
        EngineRun::Suppressed => {
            return Err(publication.fail(PipelineFailure::EditOutputCount { observed: 0 }));
        }
        EngineRun::Pushdown(error) => {
            // A pushed-down mismatch is a typed failure with a rendered
            // message, exactly like a mid-residual runtime failure. It is
            // reported to the sink first — the facade classifies the
            // terminal failure as `Reported` ("already streamed to
            // stderr"), so the abort must not swallow the stderr line
            // (the same shape the single-document drive's arms keep).
            let RuntimeError { mismatch, message } = pushdown_error(error);
            let report = RuntimeError { mismatch, message }.into_sequence_error(value_index, input_line, None);
            sink.report_value_error(report)
                .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
            return Err(publication.fail(mismatch.into_failure()));
        }
    };
    let published: std::borrow::Cow<'_, [u8]> = if program.fact_writes() && program.modifies() {
        // Both families produce BytePatches against the ORIGINAL segment.
        // Apply them as one PatchSet: sequencing one apply then the other
        // shifts offsets. A structural value change has no leaf patches, and
        // a whole re-encode would emit the old facts, so mixed+structural
        // refuses rather than flooring.
        let document = original.document();
        let Some(segment) = document.source_segment() else {
            return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "edit lane fact write without a retained source segment",
                },
            ))));
        };
        let root = document.root_handle();
        let root_view = document
            .value_view(root)
            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
        let value_patches = match diff_edit_value::<Sink>(
            document,
            root_view,
            &output,
            &mut Vec::new(),
            false,
            segment,
            &factory,
            reused_encoder,
            preservation,
            policy.cooperative_credits,
            resources,
            publication,
        )? {
            EditDiff::Unchanged => Vec::new(),
            EditDiff::Patches(patches) => patches,
            EditDiff::Structural => {
                return Err(publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(
                    "a fact assignment cannot be mixed with a structural value edit",
                )))));
            }
        };
        let patched = apply_fact_deltas::<Sink>(
            document,
            segment,
            &fact_deltas,
            input_format,
            &factory,
            resources,
            publication,
            value_patches,
        )?;
        let verifies = edit_document_facts_verify::<Sink>(
            catalog,
            source,
            start,
            &patched,
            input_format,
            input_dialect,
            requirement,
            &fact_deltas,
            &output,
            policy,
            resources,
            publication,
        )?;
        if !verifies {
            return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "edit lane mixed fact and value write verification",
                },
            ))));
        }
        std::borrow::Cow::Owned(patched)
    } else if program.fact_writes() {
        // Fact deltas only. There is NO floor fallback: a whole re-encode
        // would re-emit the document's OLD facts, so a fact write is exact or
        // it is an error.
        let document = original.document();
        let Some(segment) = document.source_segment() else {
            return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "edit lane fact write without a retained source segment",
                },
            ))));
        };
        let patched = apply_fact_deltas::<Sink>(
            document,
            segment,
            &fact_deltas,
            input_format,
            &factory,
            resources,
            publication,
            Vec::new(),
        )?;
        // Verify by re-decode: the VALUE must be identical (the fact write
        // touches comment bytes only) AND the written node's comment fact must
        // equal the delta's payload. The value comparison is on MATERIALIZED
        // values, never encoded bytes — a whole re-encode re-emits the comment
        // lines, so a byte comparison would trip on the very comment the write
        // changed. A failure is a hard contract violation, never a silent
        // floor.
        let verifies = edit_document_facts_verify::<Sink>(
            catalog,
            source,
            start,
            &patched,
            input_format,
            input_dialect,
            requirement,
            &fact_deltas,
            &output,
            policy,
            resources,
            publication,
        )?;
        if !verifies {
            return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                    contract: "edit lane fact write verification",
                },
            ))));
        }
        std::borrow::Cow::Owned(patched)
    } else if program.modifies() {
        let document = original.document();
        // The document's own retained segment is the span authority:
        // adjacent values attach to the segment holding exactly their
        // text, so every node span is segment-relative by construction.
        // A document WITHOUT a retained segment (a span-less TOML/YAML
        // build, or a transformed YAML source) cannot be diffed against
        // the source; with nothing published yet the lane declines so the
        // caller's ordinary ladder re-encodes instead of erroring.
        let Some(segment) = document.source_segment() else {
            if publication.completed_items > 0 {
                return Err(edit_contract::<Sink::Error>(publication));
            }
            return Ok(None);
        };
        let root = document.root_handle();
        let root_view = document
            .value_view(root)
            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
        // The unit's framing: the bytes this segment carries BEFORE the root
        // node's own span — the opening `---` and any comment block attached
        // to the unit start, the FIRST unit's included (its leading comments
        // and an authored leading `---` live in its own segment, before the
        // value). A whole-segment re-encode (the floor) renders only the
        // VALUE, so without this prefix the unit's stream framing would be
        // lost and a multi-document file would splice into one merged
        // document. Patch-based publishes never drop it (they edit spans
        // inside the segment); the floor must match that law.
        //
        // A unit whose document carries no committed spans (the bare empty
        // document: an explicit `---` or a comment-only unit) has no framable
        // prefix — there is no span to anchor content_start on, so the floor
        // re-encodes from the value alone. That is §4.8's own treatment: the
        // empty unit is not a value-bearing document for framing to lead.
        let content_start = document
            .node_source_span(root_view.node())
            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
            .map_or(0, |span| usize::try_from(span.start()).unwrap_or(0));
        let unit_prefix = if content_start > 0 && pre_value_bytes_are_trivia(input_format.as_str()) {
            &segment[..content_start.min(segment.len())]
        } else {
            &[]
        };
        let diff = diff_edit_value::<Sink>(
            document,
            root_view,
            &output,
            &mut Vec::new(),
            false,
            segment,
            &factory,
            reused_encoder,
            preservation,
            policy.cooperative_credits,
            resources,
            publication,
        )?;
        match diff {
            EditDiff::Unchanged => std::borrow::Cow::Borrowed(segment),
            EditDiff::Patches(mut patches) => {
                // The patch set is position-ordered. The recursive walk emits
                // in document order for one container, but a RESIZE combines
                // its own splice with its children's patches, and a child's
                // splice can land after its parent's — so the lane sorts once
                // here instead of asking every producer to interleave (the
                // fact-write lane sorts at its own set the same way).
                patches.sort_by_key(|patch| patch.span().start());
                // A patch set that does not validate is DOUBT, not a fault:
                // removing a container's first member and the member after it
                // names the same comma twice (the first member's cut runs
                // through the FOLLOWING comma, the next member's from the
                // PRECEDING one), and the splice's declared law is that any
                // doubt falls to the whole-document floor. Failing the request
                // instead left `del(.a,.b)` a hard error with the file
                // untouched.
                let patched_bytes = PatchSet::try_new(None, segment.len(), patches)
                    .and_then(|set| set.apply(None, segment))
                    .ok();
                let verified = match &patched_bytes {
                    Some(bytes) => edit_document_verifies::<Sink>(
                        catalog,
                        source,
                        start,
                        bytes,
                        input_format,
                        input_dialect,
                        output_format,
                        output_dialect,
                        requirement,
                        &output,
                        preservation,
                        policy,
                        resources,
                        publication,
                    )?,
                    None => false,
                };
                if let (Some(patched_bytes), true) = (patched_bytes, verified) {
                    std::borrow::Cow::Owned(patched_bytes)
                } else {
                    // A patch that cannot be proven to render the program's
                    // value is the floor's job: re-encode the whole document.
                    std::borrow::Cow::Owned(encode_edit_whole::<Sink>(
                        segment,
                        unit_prefix,
                        &factory,
                        reused_encoder,
                        &output,
                        preservation,
                        policy.cooperative_credits,
                        resources,
                        publication,
                    )?)
                }
            }
            EditDiff::Structural => std::borrow::Cow::Owned(encode_edit_whole::<Sink>(
                segment,
                unit_prefix,
                &factory,
                reused_encoder,
                &output,
                preservation,
                policy.cooperative_credits,
                resources,
                publication,
            )?),
        }
    } else {
        // The document-subject law: a program with no assignment leaves the
        // document unchanged, so edit mode publishes the retained bytes.
        // The exactly-one-output law above already refused zero/many runs,
        // and the retained span of this value is authoritative without a
        // document walk.
        std::borrow::Cow::Borrowed(
            source
                .bytes()
                .get(start..end)
                .ok_or_else(|| edit_contract::<Sink::Error>(publication))?,
        )
    };
    publish_edit_item(
        sink,
        publication.completed_items,
        &published,
        framing.item_suffix,
        resources,
        policy.cooperative_credits,
        publication,
    )?;
    Ok(Some(end))
}

/// The whole-document product behind one codec input outcome, when the lane can
/// diff it against the source.
pub(crate) fn edit_product<'a, 'source>(
    outcome: &'a CodecInputOutcome<'source>,
) -> Option<&'a DocumentProduct<'source>> {
    match outcome {
        CodecInputOutcome::Result(EngineResult::Located(located)) => Some(located.product()),
        CodecInputOutcome::Missing { authority, .. } | CodecInputOutcome::TypeMismatch { authority, .. } => {
            Some(authority.product())
        }
        CodecInputOutcome::Result(EngineResult::Owned(_)) => None,
    }
}

/// Materializes one engine result into the owned value the edit lane publishes.
pub(crate) fn edit_materialize_result<Sink: ItemSink>(
    result: EngineResult<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Value, PipelineError<Sink::Error>> {
    match result {
        EngineResult::Owned(value) => Ok(value),
        EngineResult::Located(located) => located
            .product()
            .document()
            .materialize_node(located.node(), resources)
            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error)))),
    }
}

/// Recursively diffs one original document node against the program's output
/// value, producing disjoint leaf-span replacements or a structural decline.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one recursive diff keeps its document, view, value, source, encoder, session, and \
              accounting explicit; the array/object/scalar arms are one coherent walk"
)]
pub(crate) fn diff_edit_value<'document, 'source, Sink: ItemSink>(
    document: &'document Document<'source>,
    view: ValueView<'document, 'source>,
    new: &Value,
    key_path: &mut Vec<String>,
    within_shared: bool,
    source: &[u8],
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    // An edit that touches the CONTENT of an alias-referenced node refuses:
    // the codec shares ONE document node
    // across an anchor and its aliases, so a leaf patch or container growth
    // would rewrite the anchor's authored span and silently change every
    // other alias site. The refusal is a terminal prose error, never a
    // patch and never a silent floor. The flag carries DOWN the recursion:
    // a change INSIDE an aliased container's subtree is just as ambiguous
    // as a change at the container itself, because the patch lands inside
    // the anchor's bytes.
    let within_shared = within_shared
        || edit_refusal_message(document, view.node())
            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
            .is_some();
    let original_kind = view
        .kind()
        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
    let new_kind = new.kind();
    if original_kind != new_kind {
        // A cross-kind change at a SCALAR leaf whose authored span is
        // retained can still patch: the scalar arm's span replacement is
        // kind-agnostic — the new value renders through the codec's leaf
        // grammar — so a changed null, boolean, or number need not floor.
        // The deferral stands for the CONTAINER cases (a `{…}` → scalar
        // change needs a container-span splice) and for span-less scalars,
        // whose scalar arm keeps flooring them.
        let scalar_leaf_patch = !matches!(original_kind, ValueKind::Array | ValueKind::Object)
            && !matches!(new_kind, ValueKind::Array | ValueKind::Object)
            && document
                .node_source_span(view.node())
                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
                .is_some();
        if !scalar_leaf_patch {
            if within_shared {
                refuse_shared_edit(publication, document, view.node(), within_shared, resources)?;
            }
            return Ok(EditDiff::Structural);
        }
    }
    match original_kind {
        ValueKind::Array => {
            let array = view
                .array()
                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
                .ok_or_else(|| {
                    publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "edit lane diff array view",
                        },
                    )))
                })?;
            let Value::Array(new_array) = new.untagged() else {
                return Ok(EditDiff::Structural);
            };
            let original_items: Vec<_> = array.iter().collect();
            if original_items.len() != new_array.len() {
                if within_shared {
                    refuse_shared_edit(publication, document, view.node(), within_shared, resources)?;
                }
                let resize = if new_array.len() > original_items.len() {
                    edit_growth_array::<Sink>
                } else {
                    edit_shrink_array::<Sink>
                };
                return resize(
                    document,
                    view,
                    key_path,
                    new_array,
                    &original_items,
                    within_shared,
                    source,
                    factory,
                    reused,
                    preservation,
                    credits,
                    resources,
                    publication,
                );
            }
            edit_combine_children::<Sink, _>(
                original_items.iter().enumerate().map(|(index, item)| {
                    (
                        *item,
                        new_array.get(index).ok_or_else(|| {
                            publication.fail(PipelineFailure::Codec(CodecError::new(
                                jqf_codec_core::CodecFailureKind::InternalContractViolation {
                                    contract: "edit lane diff array item",
                                },
                            )))
                        }),
                    )
                }),
                document,
                key_path,
                within_shared,
                source,
                factory,
                reused,
                preservation,
                credits,
                resources,
                publication,
            )
        }
        ValueKind::Object => {
            let object = view
                .object()
                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
                .ok_or_else(|| {
                    publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "edit lane diff object view",
                        },
                    )))
                })?;
            let Value::Object(new_object) = new.untagged() else {
                return Ok(EditDiff::Structural);
            };
            let mut original_entries = Vec::new();
            for entry in object.iter() {
                let entry = entry.map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
                original_entries.push((entry.key(), entry.value()));
            }
            if original_entries.len() != new_object.len() {
                if within_shared {
                    refuse_shared_edit(publication, document, view.node(), within_shared, resources)?;
                }
                let resize = if new_object.len() > original_entries.len() {
                    edit_growth_object::<Sink>
                } else {
                    edit_shrink_object::<Sink>
                };
                return resize(
                    document,
                    view,
                    key_path,
                    new_object,
                    &original_entries,
                    within_shared,
                    source,
                    factory,
                    reused,
                    preservation,
                    credits,
                    resources,
                    publication,
                );
            }
            let mut children = Vec::new();
            // A key change at an index is a RENAME: the new key's bytes
            // splice the old key's authored token in place — at the SAME
            // byte length the splice is an overwrite that moves nothing,
            // and at a DIFFERENT length it shifts the following bytes.
            // Either way the new key
            // goes where the old key was (the rename keeps the member's
            // position in the program's ordered-object value) and the
            // entry's comments follow the key: they belong to the entry,
            // never to the line, so the splice touches only the key
            // token's bytes. The index pairing is the diff's own guess — a
            // rename that also reordered the members is caught by the
            // re-decode verification, which sends any doubt to the floor.
            let mut renames: Vec<(&str, &str)> = Vec::new();
            for (index, (key, value)) in original_entries.iter().enumerate() {
                let entry = new_object.get_index(index).ok_or_else(|| {
                    publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "edit lane diff object entry",
                        },
                    )))
                })?;
                if *key != entry.key() {
                    if within_shared {
                        refuse_shared_edit(publication, document, view.node(), within_shared, resources)?;
                    }
                    renames.push((*key, entry.key()));
                }
                children.push((*key, *value, Ok(entry.value())));
            }
            // Object members contribute one path component each: the
            // structural-append seam needs the container's value path to
            // render a new section's `[a.b]` header.
            let mut patches = Vec::new();
            for (key, child, new_child) in children {
                let new_child = new_child?;
                // A MERGE-INHERITED member of THIS container (142 W1): any
                // change to it — a leaf edit or a full replacement — splices
                // the WHOLE new member into the container at its own
                // indentation instead of patching the anchor's authored
                // span. The `<<:` line, the anchor, and every other merge
                // site stay byte-identical. A write that reaches the same
                // node THROUGH the anchor sees a different container here
                // (the payload names the host, not the anchor) and falls
                // through to the ordinary refusal below. Gated on
                // `within_shared`: a host that is ITSELF alias-shared has no
                // unambiguous authored span to append into.
                if !within_shared
                    && merge_override_into(document, child.node(), view.node())
                        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
                {
                    key_path.push((*key).to_owned());
                    let diff = diff_merge_override_member::<Sink>(
                        document,
                        view,
                        key_path,
                        source,
                        factory,
                        resources,
                        publication,
                        key,
                        child,
                        new_child,
                    )?;
                    key_path.pop();
                    match diff {
                        EditDiff::Unchanged => {}
                        EditDiff::Patches(mut more) => patches.append(&mut more),
                        EditDiff::Structural => return Ok(EditDiff::Structural),
                    }
                    continue;
                }
                key_path.push((*key).to_owned());
                let diff = diff_edit_value::<Sink>(
                    document,
                    child,
                    new_child,
                    key_path,
                    within_shared,
                    source,
                    factory,
                    reused,
                    preservation,
                    credits,
                    resources,
                    publication,
                )?;
                key_path.pop();
                match diff {
                    EditDiff::Unchanged => {}
                    EditDiff::Patches(mut more) => patches.append(&mut more),
                    EditDiff::Structural => return Ok(EditDiff::Structural),
                }
            }
            if patches.is_empty() {
                if renames.is_empty() {
                    Ok(EditDiff::Unchanged)
                } else {
                    splice_edit_rename::<Sink>(
                        document,
                        view,
                        key_path,
                        source,
                        factory,
                        resources,
                        publication,
                        jqf_codec_core::EditRenameMembers(&renames),
                    )
                }
            } else if renames.is_empty() {
                Ok(EditDiff::Patches(patches))
            } else {
                // The rename overwrites the key tokens and the child diffs
                // patch the values below them: the two patch families land
                // on disjoint regions, so they combine into one set. A
                // codec that cannot name the key tokens in place declines
                // the whole container to the floor, exactly as a partial
                // removal would.
                let EditDiff::Patches(mut rename_patches) = splice_edit_rename::<Sink>(
                    document,
                    view,
                    key_path,
                    source,
                    factory,
                    resources,
                    publication,
                    jqf_codec_core::EditRenameMembers(&renames),
                )?
                else {
                    return Ok(EditDiff::Structural);
                };
                patches.append(&mut rename_patches);
                Ok(EditDiff::Patches(patches))
            }
        }
        _ => {
            // Scalar leaf: the source span decides. A leaf with no retained
            // span (a decimal float, a boolean, a null, a materialized value)
            // cannot be PATCHED minimally, but it can still be judged
            // UNCHANGED: the decoded original compares semantically against
            // the program's output value under the engine's observable
            // equality. An unchanged span-less leaf keeps its authored bytes
            // verbatim — which is what keeps a float- or bool-bearing
            // document on the patch lane instead of falling to a
            // whole-document re-encode that destroys comments, spelling, and
            // layout. Only a CHANGED span-less leaf is unpatchable and routes
            // the document to the floor.
            let Some(span) = document
                .node_source_span(view.node())
                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
            else {
                let original = document
                    .materialize_node(
                        document
                            .node_handle(view.node())
                            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?,
                        resources,
                    )
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
                let unchanged = values_semantically_equal(&original, new.untagged());
                return Ok(if unchanged {
                    EditDiff::Unchanged
                } else {
                    if within_shared {
                        refuse_shared_edit(publication, document, view.node(), within_shared, resources)?;
                    }
                    EditDiff::Structural
                });
            };
            let start = span.start() as usize;
            let end = span.end() as usize;
            if original_kind == ValueKind::String {
                // The span convention is codec-specific. The strict JSON and
                // TOML codecs record a string value's span as its INNER
                // content — between the quotes — and the YAML build does the
                // same for a quoted scalar; a YAML PLAIN scalar's span is the
                // complete value token (it has no quotes). The unchanged test
                // is SEMANTIC (the decoded text against the new value), so an
                // untouched source-backed string survives verbatim in every
                // convention; a change replaces the quoted span in the inner
                // convention, or the bare span in the value-complete one.
                let Value::String(new_string) = new.untagged() else {
                    if within_shared {
                        refuse_shared_edit(publication, document, view.node(), within_shared, resources)?;
                    }
                    return Ok(EditDiff::Structural);
                };
                let unchanged = match view
                    .scalar()
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
                {
                    Some(jqf_data::ScalarView::String(original)) => original == new_string.as_str(),
                    _ => false,
                };
                if unchanged {
                    return Ok(EditDiff::Unchanged);
                }
                if within_shared {
                    refuse_shared_edit(publication, document, view.node(), within_shared, resources)?;
                }
                // Detect the convention from the source bytes at the span
                // edges: a matching quote pair on both sides means the span
                // is inner content and the patch must cover the quotes;
                // otherwise the span is the whole value token. Both JSON's
                // `"` and TOML's/YAML's `'` are recognized, so a TOML
                // literal string and a YAML single-quoted scalar patch like a
                // basic one.
                let quote = source.get(start.wrapping_sub(1)).copied();
                let (full_start, full_end) =
                    if quote.is_some_and(|q| q == b'"' || q == b'\'') && source.get(end) == quote.as_ref() {
                        (start - 1, end + 1)
                    } else {
                        (start, end)
                    };
                let Some(encoded) = encode_edit_leaf::<Sink>(
                    factory,
                    reused,
                    document,
                    view.node(),
                    key_path,
                    source,
                    new,
                    Some(&source[full_start..full_end]),
                    preservation,
                    credits,
                    resources,
                    publication,
                )?
                else {
                    // Neither the codec's leaf grammar nor its whole-document
                    // encoder can render this value at a leaf site; the floor
                    // re-encodes the whole document.
                    return Ok(EditDiff::Structural);
                };
                let patch = BytePatch::try_from_usize(full_start, full_end, encoded)
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?;
                Ok(EditDiff::Patches(std::vec![patch]))
            } else {
                let Some(encoded) = encode_edit_leaf::<Sink>(
                    factory,
                    reused,
                    document,
                    view.node(),
                    key_path,
                    source,
                    new,
                    Some(&source[start..end]),
                    preservation,
                    credits,
                    resources,
                    publication,
                )?
                else {
                    return Ok(EditDiff::Structural);
                };
                // The unchanged test is SEMANTIC, never a byte comparison:
                // a float's authored spelling (`1.50`) is not its canonical
                // render (`1.5`), so source-vs-encoded would patch an
                // untouched leaf. Equal leaves keep their authored bytes
                // verbatim; only a genuinely changed leaf patches its span.
                let original = document
                    .materialize_node(
                        document
                            .node_handle(view.node())
                            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?,
                        resources,
                    )
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
                if values_semantically_equal(&original, new.untagged()) {
                    Ok(EditDiff::Unchanged)
                } else {
                    if within_shared {
                        refuse_shared_edit(publication, document, view.node(), within_shared, resources)?;
                    }
                    let patch = BytePatch::try_from_usize(start, end, encoded)
                        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?;
                    Ok(EditDiff::Patches(std::vec![patch]))
                }
            }
        }
    }
}

/// One object container grew: every original key survives, the new
/// object only gained members. The added members are the new object's
/// keys absent from the original, in insertion order.
///
/// The members that SURVIVED are diffed too. A program is one edit, not
/// one per statement: `.db.host = "y" | .extra = 1` grows the root AND
/// changes a leaf below it, and a splice that renders only the growth
/// fails its own re-decode verification, dropping the whole document to
/// the floor. Carrying the surviving members' leaf patches alongside the
/// splice keeps that program on the patch lane.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edit_growth_object<'document, 'source, Sink: ItemSink>(
    document: &'document Document<'source>,
    view: ValueView<'document, 'source>,
    key_path: &mut Vec<String>,
    new_object: &jqf_data::Object,
    original_entries: &[(&'document str, ValueView<'document, 'source>)],
    within_shared: bool,
    source: &[u8],
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    if new_object.len() <= original_entries.len() {
        return Ok(EditDiff::Structural);
    }
    for (key, _) in original_entries {
        if new_object.get(key).is_none() {
            // A key both removed and added in one program is a reshape, not
            // a resize the policy names; the floor re-encodes.
            return Ok(EditDiff::Structural);
        }
    }
    let mut added: Vec<(&str, &Value)> = Vec::new();
    // One membership set over the original keys: classifying the added
    // members is O(n+m), never a linear original scan per new entry.
    let original_keys: std::collections::HashSet<&str> = original_entries.iter().map(|(key, _)| *key).collect();
    for entry in new_object {
        if !original_keys.contains(entry.key()) {
            added.push((entry.key(), entry.value()));
        }
    }
    if added.is_empty() {
        return Ok(EditDiff::Structural);
    }
    let EditDiff::Patches(mut patches) = edit_surviving_object_patches::<Sink>(
        document,
        key_path,
        new_object,
        original_entries,
        within_shared,
        source,
        factory,
        reused,
        preservation,
        credits,
        resources,
        publication,
    )?
    else {
        return Ok(EditDiff::Structural);
    };
    let EditDiff::Patches(mut splice) = splice_edit_growth::<Sink>(
        document,
        view,
        key_path,
        source,
        factory,
        resources,
        publication,
        jqf_codec_core::EditAppendMembers::Table(&added),
    )?
    else {
        return Ok(EditDiff::Structural);
    };
    patches.append(&mut splice);
    Ok(EditDiff::Patches(patches))
}

/// One object container shrank: every key the new object still holds was in
/// the original, and the program only removed members. The removed members
/// are the original keys absent from the new object.
///
/// Deleting a member removes authored bytes — the key, its value, its
/// punctuation, its line, and the comment lines the format attaches to it —
/// that no leaf patch addresses, so the codec's removal seam names the cuts.
/// Without it every `del(…)` fell to the whole-document floor, which drops
/// every comment in the file and respells every authored number.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edit_shrink_object<'document, 'source, Sink: ItemSink>(
    document: &'document Document<'source>,
    view: ValueView<'document, 'source>,
    key_path: &mut Vec<String>,
    new_object: &jqf_data::Object,
    original_entries: &[(&'document str, ValueView<'document, 'source>)],
    within_shared: bool,
    source: &[u8],
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    if new_object.len() >= original_entries.len() {
        return Ok(EditDiff::Structural);
    }
    // One membership set over the original keys: classifying the surviving
    // members is O(n+m), never a linear original scan per new entry.
    let original_keys: std::collections::HashSet<&str> = original_entries.iter().map(|(key, _)| *key).collect();
    for entry in new_object {
        if !original_keys.contains(entry.key()) {
            // A key both removed and added in one program is a reshape, not
            // a resize the policy names; the floor re-encodes.
            return Ok(EditDiff::Structural);
        }
    }
    let mut removed: Vec<(&str, NodeId)> = Vec::new();
    for (key, child) in original_entries {
        if new_object.get(key).is_none() {
            removed.push((*key, child.node()));
        }
    }
    if removed.is_empty() {
        return Ok(EditDiff::Structural);
    }
    let EditDiff::Patches(mut patches) = edit_surviving_object_patches::<Sink>(
        document,
        key_path,
        new_object,
        original_entries,
        within_shared,
        source,
        factory,
        reused,
        preservation,
        credits,
        resources,
        publication,
    )?
    else {
        return Ok(EditDiff::Structural);
    };
    let EditDiff::Patches(mut cuts) = cut_edit_shrink::<Sink>(
        document,
        view,
        key_path,
        source,
        factory,
        resources,
        publication,
        jqf_codec_core::EditRemoveMembers::Table(&removed),
    )?
    else {
        return Ok(EditDiff::Structural);
    };
    patches.append(&mut cuts);
    Ok(EditDiff::Patches(patches))
}

/// One array container shrank: the new array is the original with items
/// removed, matched positionally from the front. The surviving items are
/// diffed, so a program that deletes one item and edits another stays on the
/// patch lane.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edit_shrink_array<'document, 'source, Sink: ItemSink>(
    document: &'document Document<'source>,
    view: ValueView<'document, 'source>,
    key_path: &mut Vec<String>,
    new_array: &jqf_data::Array,
    original_items: &[ValueView<'document, 'source>],
    within_shared: bool,
    source: &[u8],
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    if new_array.len() >= original_items.len() {
        return Ok(EditDiff::Structural);
    }
    // Which original items the program dropped is not recorded anywhere: the
    // engine hands back a shorter array, not a delete list. The survivors are
    // matched greedily in order — the first original item semantically equal
    // to the next survivor keeps it — which is exactly what `del(.[i])` and a
    // `map(select(…))` filter produce. An order the walk cannot align leaves
    // survivors unmatched and declines to the floor.
    let mut removed: Vec<(usize, NodeId)> = Vec::new();
    let mut kept: Vec<(usize, &Value)> = Vec::new();
    let mut next = 0usize;
    for (index, original) in original_items.iter().enumerate() {
        let remaining = new_array.len() - next;
        if remaining == original_items.len() - index {
            kept.push((index, new_array.get(next).ok_or_else(|| edit_contract(publication))?));
            next += 1;
            continue;
        }
        let materialized = document
            .materialize_node(
                document
                    .node_handle(original.node())
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?,
                resources,
            )
            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
        match new_array.get(next) {
            Some(candidate) if values_semantically_equal(&materialized, candidate) => {
                kept.push((index, candidate));
                next += 1;
            }
            _ => removed.push((index, original.node())),
        }
    }
    if next != new_array.len() || removed.is_empty() {
        return Ok(EditDiff::Structural);
    }
    let mut patches = Vec::new();
    for (index, new_item) in kept {
        let diff = diff_edit_value::<Sink>(
            document,
            original_items[index],
            new_item,
            key_path,
            within_shared,
            source,
            factory,
            reused,
            preservation,
            credits,
            resources,
            publication,
        )?;
        match diff {
            EditDiff::Unchanged => {}
            EditDiff::Patches(mut more) => patches.append(&mut more),
            EditDiff::Structural => return Ok(EditDiff::Structural),
        }
    }
    let EditDiff::Patches(mut cuts) = cut_edit_shrink::<Sink>(
        document,
        view,
        key_path,
        source,
        factory,
        resources,
        publication,
        jqf_codec_core::EditRemoveMembers::Array(&removed),
    )?
    else {
        return Ok(EditDiff::Structural);
    };
    patches.append(&mut cuts);
    Ok(EditDiff::Patches(patches))
}

/// Asks the codec for the shrink cuts and turns each byte range into an
/// empty-replacement patch; an empty cut set declines to the whole-document
/// floor, exactly as the growth splice does.
#[allow(clippy::too_many_arguments)]
pub(crate) fn cut_edit_shrink<Sink: ItemSink>(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    key_path: &[String],
    source: &[u8],
    factory: &ErasedEncoderFactory,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
    members: jqf_codec_core::EditRemoveMembers<'_>,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    let removals = factory
        .render_edit_remove(document, view.node(), key_path, source, members, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    if removals.is_empty() {
        return Ok(EditDiff::Structural);
    }
    let mut patches = Vec::new();
    for removal in removals {
        // A removal with a non-empty replacement rewrites the span instead
        // of cutting it (the binary length-header pattern).
        patches.push(
            BytePatch::try_from_usize(removal.start, removal.end, removal.replacement)
                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
        );
    }
    Ok(EditDiff::Patches(patches))
}

/// Diffs the members a resized container KEPT, so the resize splice carries
/// the leaf changes that landed on its siblings in the same program.
/// A surviving member the walk cannot patch declines the whole container to
/// the floor, exactly as the equal-length walk does.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edit_surviving_object_patches<'document, 'source, Sink: ItemSink>(
    document: &'document Document<'source>,
    key_path: &mut Vec<String>,
    new_object: &jqf_data::Object,
    original_entries: &[(&'document str, ValueView<'document, 'source>)],
    within_shared: bool,
    source: &[u8],
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    let mut patches = Vec::new();
    for (key, child) in original_entries {
        let Some(new_child) = new_object.get(key) else {
            continue;
        };
        key_path.push((*key).to_owned());
        let diff = diff_edit_value::<Sink>(
            document,
            *child,
            new_child,
            key_path,
            within_shared,
            source,
            factory,
            reused,
            preservation,
            credits,
            resources,
            publication,
        )?;
        key_path.pop();
        match diff {
            EditDiff::Unchanged => {}
            EditDiff::Patches(mut more) => patches.append(&mut more),
            EditDiff::Structural => return Ok(EditDiff::Structural),
        }
    }
    Ok(EditDiff::Patches(patches))
}

/// One array container grew: the original items survive as the new
/// array's PREFIX, and the array only gained a suffix. A prepend or an
/// insert is not a splice the policy names; the floor re-encodes.
///
/// The surviving prefix is DIFFED, not required to be unchanged: a program
/// that appends an item and edits an existing one is one edit, and the
/// splice needs its siblings' patches to survive verification.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edit_growth_array<'document, 'source, Sink: ItemSink>(
    document: &'document Document<'source>,
    view: ValueView<'document, 'source>,
    key_path: &mut Vec<String>,
    new_array: &jqf_data::Array,
    original_items: &[ValueView<'document, 'source>],
    within_shared: bool,
    source: &[u8],
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    if new_array.len() <= original_items.len() {
        return Ok(EditDiff::Structural);
    }
    let mut patches = Vec::new();
    for (index, original) in original_items.iter().enumerate() {
        let Some(new_item) = new_array.get(index) else {
            return Ok(EditDiff::Structural);
        };
        let diff = diff_edit_value::<Sink>(
            document,
            *original,
            new_item,
            key_path,
            within_shared,
            source,
            factory,
            reused,
            preservation,
            credits,
            resources,
            publication,
        )?;
        match diff {
            EditDiff::Unchanged => {}
            EditDiff::Patches(mut more) => patches.append(&mut more),
            EditDiff::Structural => return Ok(EditDiff::Structural),
        }
    }
    let mut added: Vec<&Value> = Vec::new();
    for index in original_items.len()..new_array.len() {
        let Some(item) = new_array.get(index) else {
            return Ok(EditDiff::Structural);
        };
        added.push(item);
    }
    if added.is_empty() {
        return Ok(EditDiff::Structural);
    }
    let EditDiff::Patches(mut splice) = splice_edit_growth::<Sink>(
        document,
        view,
        key_path,
        source,
        factory,
        resources,
        publication,
        jqf_codec_core::EditAppendMembers::Array(&added),
    )?
    else {
        return Ok(EditDiff::Structural);
    };
    patches.append(&mut splice);
    Ok(EditDiff::Patches(patches))
}

/// Asks the codec for the growth splice and turns the ordered insertions
/// into zero-length byte patches; an empty insertion set declines to the
/// whole-document floor.
#[allow(clippy::too_many_arguments)]
pub(crate) fn splice_edit_growth<Sink: ItemSink>(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    key_path: &[String],
    source: &[u8],
    factory: &ErasedEncoderFactory,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
    members: jqf_codec_core::EditAppendMembers<'_>,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    let insertions = factory
        .render_edit_append(document, view.node(), key_path, source, members, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    if insertions.is_empty() {
        return Ok(EditDiff::Structural);
    }
    // Merge same-position insertions (a section whose direct statements
    // and new subsections land on the same anchor line): the patch set
    // rejects same-start zero-length patches as ambiguous.
    let mut merged: Vec<jqf_codec_core::EditInsertion> = Vec::new();
    for insertion in insertions {
        if let Some(last) = merged.last_mut()
            && last.at == insertion.at
        {
            last.bytes.extend_from_slice(&insertion.bytes);
        } else {
            merged.push(insertion);
        }
    }
    let mut patches = Vec::new();
    for insertion in merged {
        // A replacement splice (`replace: Some((start, end))`) rewrites the
        // named span — the binary length-header pattern — instead of
        // inserting at a zero-length span.
        let (start, end) = match insertion.replace {
            Some((start, end)) => (start, end),
            None => (insertion.at, insertion.at),
        };
        patches.push(
            BytePatch::try_from_usize(start, end, insertion.bytes)
                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
        );
    }
    // The splice positions are named in source order (a section's direct
    // statements land before its new subsections), so the patches are
    // ordered by construction.
    Ok(EditDiff::Patches(patches))
}

/// Whether one document node is a MERGE-INHERITED member whose host is
/// exactly `container` — a YAML member `<<:`-spliced in from an anchored
/// mapping, with the format-neutral [`jqf_codec_core::MERGE_OVERRIDE_ROLE`]
/// fact naming the HOST mapping's document node id as its integer payload.
/// The anchor's own entry is the same shared value node but
/// its fact payload names the ANCHOR, never the host, so a write descending
/// through the anchor finds no matching host here and stays refused.
pub(crate) fn merge_override_into(document: &Document<'_>, node: NodeId, container: NodeId) -> Result<bool, DataError> {
    if !document.fact_owner_indexed() {
        return Ok(false);
    }
    for fact_id in document.owner_fact_ids(node) {
        let fact = document.fact(*fact_id)?;
        if fact.role().as_str() == jqf_codec_core::MERGE_OVERRIDE_ROLE
            && match fact.payload() {
                FactPayloadView::Integer(text) => {
                    text.parse::<u64>()
                        .ok()
                        .and_then(|index| usize::try_from(index).ok())
                        .and_then(jqf_data::NodeId::try_from_index)
                        == Some(container)
                }
                _ => false,
            }
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the splice boundary threads the same document inventory as splice_edit_rename/growth"
)]
pub(crate) fn diff_merge_override_member<'document, 'source, Sink: ItemSink>(
    document: &'document Document<'source>,
    container: ValueView<'document, 'source>,
    key_path: &[String],
    source: &[u8],
    factory: &ErasedEncoderFactory,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
    key: &str,
    original: ValueView<'document, 'source>,
    new_value: &Value,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    let original_value = document
        .materialize_node(
            document
                .node_handle(original.node())
                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?,
            resources,
        )
        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
    if values_semantically_equal(&original_value, new_value.untagged()) {
        return Ok(EditDiff::Unchanged);
    }
    splice_edit_growth::<Sink>(
        document,
        container,
        key_path,
        source,
        factory,
        resources,
        publication,
        jqf_codec_core::EditAppendMembers::Table(core::slice::from_ref(&(key, new_value))),
    )
}

/// Renders and applies a container's key renames: the codec names each old key token's authored span and
/// constructs the new key's replacement bytes over exactly that region, so
/// a SAME-length rename overwrites the key in place and moves nothing,
/// while a DIFFERENT-length rename splices the region at the new length and
/// shifts the following bytes — the new key goes where the old key was, and
/// the entry's comments stay attached (the comment follows the key). A
/// decline (an empty replacement set — the codec cannot name a key token,
/// or a dotted/flow shape it does not splice) is a structural decline to the
/// whole-document floor, exactly as a partial removal would be.
#[allow(clippy::too_many_arguments)]
pub(crate) fn splice_edit_rename<Sink: ItemSink>(
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    key_path: &[String],
    source: &[u8],
    factory: &ErasedEncoderFactory,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
    members: jqf_codec_core::EditRenameMembers<'_>,
) -> Result<EditDiff, PipelineError<Sink::Error>> {
    let replacements = factory
        .render_edit_rename(document, view.node(), key_path, source, members, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    if replacements.is_empty() {
        return Ok(EditDiff::Structural);
    }
    let mut patches = Vec::new();
    for replacement in replacements {
        let end = replacement
            .at
            .checked_add(replacement.region_len)
            .ok_or_else(|| overflow::<Sink::Error>(publication))?;
        patches.push(
            BytePatch::try_from_usize(replacement.at, end, replacement.bytes)
                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
        );
    }
    Ok(EditDiff::Patches(patches))
}

/// Combines one member's diffs, propagating a structural decline and collecting
/// the disjoint leaf patches.
#[allow(
    clippy::too_many_arguments,
    reason = "the recursive combine threads the same boundary inventory the diff walk owns"
)]
pub(crate) fn edit_combine_children<'document, 'source, Sink, I>(
    children: I,
    document: &'document Document<'source>,
    key_path: &mut Vec<String>,
    within_shared: bool,
    source: &[u8],
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<EditDiff, PipelineError<Sink::Error>>
where
    Sink: ItemSink,
    I: IntoIterator<
        Item = (
            ValueView<'document, 'source>,
            Result<&'document Value, PipelineError<Sink::Error>>,
        ),
    >,
{
    let mut patches = Vec::new();
    for (child, new_child) in children {
        let new_child = new_child?;
        match diff_edit_value::<Sink>(
            document,
            child,
            new_child,
            key_path,
            within_shared,
            source,
            factory,
            reused,
            preservation,
            credits,
            resources,
            publication,
        )? {
            EditDiff::Unchanged => {}
            EditDiff::Patches(mut more) => patches.append(&mut more),
            EditDiff::Structural => return Ok(EditDiff::Structural),
        }
    }
    if patches.is_empty() {
        Ok(EditDiff::Unchanged)
    } else {
        Ok(EditDiff::Patches(patches))
    }
}

/// Re-decodes one patched document and proves its rendered form equals the
/// program's output value's rendered form.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the verification borrows the same boundary inventory as the lane"
)]
pub(crate) fn edit_document_verifies<Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'_>,
    start: usize,
    patched: &[u8],
    input_format: &FormatId,
    input_dialect: &DialectId,
    output_format: &FormatId,
    output_dialect: &DialectId,
    requirement: &AccessRequirement,
    expected: &Value,
    preservation: PreservationRequest,
    policy: PipelinePolicy<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<bool, PipelineError<Sink::Error>> {
    // A headered record's patched payload is one DATA row. Re-decoding it as
    // a whole headered source would treat that row as the header. Rebuild
    // the prefix so the payload provider reads the authored names, then
    // decode only the patched range — the same two-step the record drive
    // uses on the way in.
    if jqf_codec_delimited::is_headered_delimited_dialect(input_dialect.as_str()) {
        return edit_headered_record_verifies::<Sink>(
            catalog,
            source,
            start,
            patched,
            input_format,
            input_dialect,
            requirement,
            expected,
            policy,
            resources,
            publication,
        );
    }
    let base = u64::try_from(start).unwrap_or(u64::MAX);
    let synthetic = ResolvedSource::new(
        source.source(),
        source.label(),
        patched,
        source.base_offset().saturating_add(base),
    );
    let engine = match access_input(
        catalog,
        synthetic,
        input_format,
        input_dialect,
        requirement,
        policy,
        resources,
        publication,
    ) {
        Ok(engine) => engine,
        // A patched byte string that fails to RE-DECODE is precisely the
        // "any doubt" the module doc's verification law sends to the floor
        // (`:54-60`) — the lane cannot prove the patch renders the program's
        // value, so it falls back to the whole-document re-encode instead of
        // raising a terminal decode diagnostic over the user's own patched
        // input. Only a codec failure over the patched bytes floors; registry,
        // bind, and internal-contract failures still propagate.
        Err(error) if matches!(error.failure(), PipelineFailure::Codec(_)) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let (outcome, _) = engine.into_parts();
    let Some(product) = edit_product(&outcome) else {
        return Ok(false);
    };
    let decoded_root = product
        .document()
        .materialize_root(resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
    // The comparison is SEMANTIC (jq's own value equality), never a byte
    // comparison of the two RENDERS, which this function used to make
    // through a fresh factory. The render is the lane's floor authority,
    // but a re-decode can differ from the program's output in PRESENTATION
    // while carrying the same value: the merge-key override (142 W1)
    // splices a host member over a `<<:`-inherited one, and the re-decode's
    // expansion necessarily orders the surviving merged entries BEFORE the
    // host's own — a key order the program's output (the input's expansion
    // order) cannot reproduce, so a byte comparison would floor a correct
    // patch. The fact-write lane already verifies on materialized values
    // for exactly this reason (`edit_document_facts_verify`: a re-encode
    // re-emits the changed comment lines and trips a byte comparison); the
    // value patch lane now follows the same law — the patch's re-decode
    // must equal the program's output as a VALUE, and key order is
    // presentation, not content.
    //
    // The RAW semantic comparison is the primary law, and it already
    // decides the record formats (csv/tsv): a record's `expected` and its
    // patched re-decode both carry the record's own 1-D value shape, so
    // they match directly. The FALLBACK is the codec round-trip: when the
    // raw values differ, the program's output value is encoded through the
    // OUTPUT codec and re-decoded through the INPUT codec before comparing
    // again, so both sides carry the codec's own type model. A strings-only
    // flat config (properties/ini/dotenv) decodes `a = 1` to the STRING
    // `"1"`, but `.z = 99` assigns a NUMBER — the program's output carries
    // `99`, the patched bytes' re-decode carries `"99"`, and the raw
    // comparison floors every flat number append (regression found at the
    // C2 merge, 145). The round-trip normalizes the expected side through
    // the same decode, so `99` becomes `"99"` and the comparison is apples
    // to apples. The fallback is only ever a strict improvement: a format
    // whose round-trip decode fails (the record formats' re-encode carries
    // framing the input codec cannot re-decode as one document) falls back
    // to the raw answer, which is the correct one for those formats.
    if values_semantically_equal(expected, &decoded_root) {
        return Ok(true);
    }
    let encoding_policy = policy.encoding();
    let encoder = catalog
        .encoder(output_format, output_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let expected_factory = encoder
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
    let expected_bytes = encode_edit_value::<Sink>(
        &expected_factory,
        &mut ReusableEncoderSession::new(),
        expected,
        preservation,
        policy.cooperative_credits,
        resources,
        publication,
    )?;
    // The round-trip's encode runs through a FRESH session, never the
    // lane's shared `reused`: a stateful output profile (the record formats'
    // CRLF law, a YAML factory's document-emitted cell) would leave the
    // shared session primed or framed, corrupting the lane's OWN later
    // encode — the exact hazard C2's doc names ("never the lane's shared
    // factory"). The fresh session is discarded after the comparison.
    let expected_source = ResolvedSource::new(
        source.source(),
        source.label(),
        &expected_bytes,
        source.base_offset().saturating_add(base),
    );
    let expected_engine = match access_input(
        catalog,
        expected_source,
        input_format,
        input_dialect,
        requirement,
        policy,
        resources,
        publication,
    ) {
        Ok(engine) => engine,
        // A round-trip the codec cannot re-decode (the record formats' case)
        // is not a verification failure — it is the fallback declining, and
        // the raw comparison above already gave the answer.
        Err(error) if matches!(error.failure(), PipelineFailure::Codec(_)) => {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    let (expected_outcome, _) = expected_engine.into_parts();
    let Some(expected_product) = edit_product(&expected_outcome) else {
        return Ok(false);
    };
    let expected_root = expected_product
        .document()
        .materialize_root(resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
    Ok(values_semantically_equal(&expected_root, &decoded_root))
}

/// Re-decodes one patched headered DATA record against the authored header.
///
/// The payload provider reads header names from the start of its source; a
/// synthetic source of just the patched row would treat that row as the
/// header. Prefixing the original bytes before this record and decoding the
/// patched range is the record drive's own two-step.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors edit_document_verifies' boundary inventory"
)]
fn edit_headered_record_verifies<Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'_>,
    start: usize,
    patched: &[u8],
    input_format: &FormatId,
    input_dialect: &DialectId,
    requirement: &AccessRequirement,
    expected: &Value,
    policy: PipelinePolicy<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<bool, PipelineError<Sink::Error>> {
    let Some(before) = source.bytes().get(..start) else {
        return Ok(false);
    };
    let mut rebuilt = Vec::with_capacity(before.len().saturating_add(patched.len()));
    rebuilt.extend_from_slice(before);
    rebuilt.extend_from_slice(patched);
    let rebuilt_source = ResolvedSource::new(source.source(), source.label(), &rebuilt, source.base_offset());
    let decoder = match catalog.decoder(input_format, input_dialect) {
        Ok(decoder) => decoder,
        Err(error) => return Err(publication.fail(PipelineFailure::Registry(error))),
    };
    let Ok(created) = decoder.create_provider(rebuilt_source, policy.decode, resources) else {
        return Ok(false);
    };
    let mut provider: ErasedProvider<'_> = created;
    let Ok(handle) = provider.bind(requirement) else {
        return Ok(false);
    };
    let payload_start = source
        .base_offset()
        .saturating_add(u64::try_from(start).unwrap_or(u64::MAX));
    let payload_end = payload_start.saturating_add(u64::try_from(patched.len()).unwrap_or(u64::MAX));
    let mut reuse = ReusableAccessSession::new();
    let Ok(access) = provider.open_range_reusing(&handle, payload_start, payload_end, &mut reuse, resources) else {
        return Ok(false);
    };
    let outcome = {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(policy.cooperative_credits);
        let Ok(outcome) = access.decode(&mut run) else {
            return Ok(false);
        };
        outcome
    };
    let Ok(engine) = jqf_engine::CodecInputResult::try_from_access(outcome) else {
        return Ok(false);
    };
    let (outcome, _) = engine.into_parts();
    let Some(product) = edit_product(&outcome) else {
        return Ok(false);
    };
    let Ok(decoded_root) = product.document().materialize_root(resources) else {
        return Ok(false);
    };
    Ok(values_semantically_equal(expected, &decoded_root))
}

/// Applies one fact-write delta set as span operations against the retained
/// source (Seam 2), keyed on each delta's ROLE — the shared position
/// vocabulary of [`jqf_codec_core::comment`] — never on a hardcoded format
/// name. The roles the seam serves are the `#`-comment positions, whose byte
/// syntax the shared line-based renderer produces:
///
/// - [`HEAD`](jqf_codec_core::comment::HEAD) (`comment`): the leading comment
///   block above the node's line — the maximal run of comment and blank lines
///   between the node's line and the previous content line, the decoder's
///   comment-ownership law read in reverse — replaced by the payload's
///   comment lines (`# text` per line, at the node's indent). An empty
///   replacement (a `null` payload or `= []`) removes the block. The block
///   is the whole of the write: an inline comment on the node's own line is
///   the INLINE position's to own, never cleared by a leading write.
/// - [`INLINE`](jqf_codec_core::comment::INLINE) (`comment_inline`): the
///   node's own-line trailing comment, replaced by ` # text` (or removed by a
///   `null`/empty payload). A multi-line payload has no inline spelling and
///   is refused.
/// - [`FOOT`](jqf_codec_core::comment::FOOT) (`comment_foot`): the comment
///   run below the node's block — the decoder's foot-attribution law read in
///   reverse ([`foot_comment_run`]) — replaced by the payload's comment lines
///   at the run's own indent. An empty replacement removes the run; a node
///   with no run and a non-empty payload inserts one below its block. The
///   root's span covers the whole document, so the root's foot is the
///   document trailer, addressed by anchoring the walk at the last content
///   line (the recorded root-trailer narrowing, resolved here).
///
/// A delta whose role the seam cannot render is a CLEAN codec refusal naming
/// the role and the format, never a name-list check: the markup attribute
/// role's grammar is a format's own splice policy, declared where that
/// format's write path lands. A format that declares a role the seam serves
/// is served by declaration; JSON, which declares no comment role at all,
/// refuses exactly as before.
///
/// Fact patches are disjoint by construction (a comment block contains no value
/// spans and two nodes' blocks are separated by content lines). `extra` is the
/// value-diff family when both writes share one program; those merge into the
/// same set, and an overlap is a refusal rather than a contract break.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one span-op application keeps its document, segment, deltas, and the per-role \
              render arms explicit"
)]
pub(crate) fn apply_fact_deltas<Sink: ItemSink>(
    document: &Document<'_>,
    segment: &[u8],
    deltas: &[FactDelta],
    input_format: &FormatId,
    factory: &ErasedEncoderFactory,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
    extra: Vec<BytePatch>,
) -> Result<Vec<u8>, PipelineError<Sink::Error>> {
    // The seam's shared renderer produces `#`-line comments; only a format
    // whose comment syntax IS the `#`-line class can be served by it (TOML
    // and YAML today). JSON declares no comment role and refuses; a markup
    // format that declares a comment role but writes `<!-- -->` comments
    // (XML, HTML) serves through its own splice policy. The two class
    // tables below — `is_hash_comment_format` for the line-based comment
    // positions, `is_markup_attribute_format` for the attribute role — are
    // the seam's whole declaration surface: a format with the `#`-line
    // syntax is served by declaration and never refused on its name.
    let mut patches: Vec<BytePatch> = Vec::new();
    for delta in deltas {
        // A fact write through an alias refuses exactly as a value write
        // does: the alias-shared node owns ONE comment fact,
        // so writing `.b.@comment` on an alias site would rewrite the
        // anchor's comment block. The escape hatch (`--edit-expand-alias`)
        // accepts exactly that anchor-rewrite semantics.
        if edit_refusal_message(document, delta.node)
            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
            .is_some()
        {
            refuse_shared_edit(publication, document, delta.node, true, resources)?;
        }
        let span = document
            .node_source_span(delta.node)
            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
            // A node without a retained span cannot be addressed by a span
            // operation; a fact write over it is a codec refusal, never the
            // engine's internal-contract class (unreachable from user input
            // per the contract law). Today only the source-retained TOML and
            // YAML documents reach this lane, where every node keeps a span,
            // so the arm is defensive.
            .ok_or_else(|| {
                publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(
                    "this value has no retained source span, so its comment fact cannot be \
                     rewritten in place",
                ))))
            })?;
        let start = span.start() as usize;
        let value_end = span.end() as usize;
        match delta.role.as_str() {
            // The HEAD position: the leading comment block above the node's
            // line, replaced by the payload's `# text` lines at the node's
            // indent. The payload-shape contract is self-describing: a lone
            // string is coerced to the list it denotes at the
            // fact delta's record site, so a refusal here is a NON-string
            // shape and the message says so instead of the bare class name.
            // The class gate is per-role: a format whose comment syntax IS
            // the shared `#`-line form is served by the line-based renderer;
            // a MARKUP-COMMENT format (XML) declares its own comment-bytes
            // renderer and is served through it; every other format refuses
            // cleanly.
            jqf_codec_core::comment::HEAD => {
                if is_hash_comment_format(input_format.as_str()) {
                    let comment_marker_bytes =
                        comment_marker(input_format).expect("a hash-comment format has a marker");
                    let (block_start, node_line_start, indent) =
                        comment_block(segment, start, comment_markers(input_format.as_str()));
                    let replacement =
                        fact_comment_text(indent, &delta.payload, comment_marker_bytes).map_err(|message| {
                            let base = CodecError::new(jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
                            let Some(diagnostic) = jqf_source::Diagnostic::try_new(
                                jqf_source::Namespace::new("pipeline").code("comment-fact"),
                                jqf_source::Severity::Error,
                                &message,
                            ) else {
                                return publication.fail(PipelineFailure::Codec(base));
                            };
                            publication.fail(PipelineFailure::Codec(base.with_diagnostic(diagnostic)))
                        })?;
                    if block_start != node_line_start || !replacement.is_empty() {
                        patches.push(
                            BytePatch::try_from_usize(block_start, node_line_start, replacement)
                                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                        );
                    }
                } else if is_codec_comment_format(input_format.as_str()) {
                    // A format whose comment syntax is NOT the shared
                    // `#`-line class owns its own byte render: XML writes
                    // `<!-- … -->` children; JSONC/JSON5 write `//` lines
                    // immediately before the member. The codec names the
                    // spans to replace. A decline (`None`) means the
                    // format's codec does not serve the comment role and
                    // is refused here.
                    let Some(patches_out) = factory
                        .render_fact_delta(
                            document,
                            delta.node,
                            segment,
                            jqf_codec_core::comment::HEAD,
                            &delta.kind,
                            &delta.payload,
                            resources,
                        )
                        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
                    else {
                        return Err(
                            publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                                "the {} format cannot carry the attached comment fact (its \
                             comment syntax is not the shared #-line form)",
                                input_format.as_str()
                            ))))),
                        );
                    };
                    for removal in patches_out {
                        patches.push(
                            BytePatch::try_from_usize(removal.start, removal.end, removal.replacement)
                                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                        );
                    }
                } else {
                    return Err(
                        publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                            "the {} format cannot carry the attached comment fact (its comment \
                         syntax is not the shared #-line form)",
                            input_format.as_str()
                        ))))),
                    );
                }
            }
            // The INLINE position: the node's own-line trailing comment. With
            // an existing trailing comment its span is replaced; without one
            // the comment is INSERTED at the value's line end.
            jqf_codec_core::comment::INLINE => {
                if !is_hash_comment_format(input_format.as_str()) {
                    return Err(
                        publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                            "the {} format cannot carry the attached comment fact (its comment \
                         syntax is not the shared #-line form)",
                            input_format.as_str()
                        ))))),
                    );
                }
                let comment_marker_bytes = comment_marker(input_format).expect("a hash-comment format has a marker");
                let text = fact_inline_text(&delta.payload).map_err(|message| {
                    let base = CodecError::new(jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
                    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
                        jqf_source::Namespace::new("pipeline").code("comment-fact"),
                        jqf_source::Severity::Error,
                        &message,
                    ) else {
                        return publication.fail(PipelineFailure::Codec(base));
                    };
                    publication.fail(PipelineFailure::Codec(base.with_diagnostic(diagnostic)))
                })?;
                match trailing_comment_span(segment, value_end, comment_marker_bytes) {
                    Some((trail_start, trail_end)) => {
                        let replacement = if text.is_empty() { Vec::new() } else { text };
                        patches.push(
                            BytePatch::try_from_usize(trail_start, trail_end, replacement)
                                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                        );
                    }
                    None if !text.is_empty() => {
                        // The value's line end, where the trailing comment
                        // would sit after the value's own bytes.
                        let line_end = segment[value_end..]
                            .iter()
                            .position(|byte| *byte == b'\n')
                            .map_or(segment.len(), |position| value_end + position);
                        patches.push(
                            BytePatch::try_from_usize(line_end, line_end, text)
                                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                        );
                    }
                    None => {}
                }
            }
            // The markup attribute role: `.&name = "value"` over a format
            // with an attribute grammar. The delta names the ELEMENT node
            // and the attribute kind. An EXISTING attribute carries its
            // authored QUOTED value span on the attribute fact; the leaf
            // seam rewrites those bytes. A MISSING attribute has no span —
            // the codec's fact-delta hook inserts ` name="escaped"` before
            // the start-tag close. The payload contract is a single text
            // value — the engine's `.&` write passes a string through
            // untouched; any other shape is refused with prose, and a
            // `null` payload (attribute DELETION) is a clean refusal too,
            // because the splice names the value, not the ` name="value"`
            // token. A missed insert refuses; there is no whole-document
            // floor (a re-encode would re-emit the old facts).
            jqf_codec_core::markup::ATTRIBUTE_FACT => {
                if !is_markup_attribute_format(input_format.as_str()) {
                    return Err(
                        publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                            "the {} format has no attribute grammar, so the attribute fact \
                         cannot be written",
                            input_format.as_str()
                        ))))),
                    );
                }
                let Value::String(_) = delta.payload.untagged() else {
                    return Err(publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(
                        "an attribute write must be a single text value (use null only to \
                             delete, which this splice policy does not name)",
                    )))));
                };
                if let Some(span) = attribute_value_span(document, delta.node, &delta.kind, resources)
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
                {
                    let authored = &segment[span.start() as usize..span.end() as usize];
                    let rendered = factory
                        .render_leaf(
                            document,
                            delta.node,
                            &[],
                            segment,
                            &delta.payload,
                            Some(authored),
                            resources,
                        )
                        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
                    patches.push(
                        BytePatch::try_from_usize(span.start() as usize, span.end() as usize, rendered)
                            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                    );
                } else {
                    let Some(patches_out) = factory
                        .render_fact_delta(
                            document,
                            delta.node,
                            segment,
                            jqf_codec_core::markup::ATTRIBUTE_FACT,
                            &delta.kind,
                            &delta.payload,
                            resources,
                        )
                        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
                    else {
                        return Err(
                            publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                                "the element has no attribute named {} to rewrite",
                                delta.kind
                            ))))),
                        );
                    };
                    for patch in patches_out {
                        patches.push(
                            BytePatch::try_from_usize(patch.start, patch.end, patch.replacement)
                                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                        );
                    }
                }
            }
            // The FOOT position: the comment run below the node's block,
            // replaced by the payload's `# text` lines at the run's own
            // indent (`foot_comment_run` finds the run, the decoder's
            // foot-attribution law read in reverse — including the root's
            // document trailer, whose span the walk anchors at the last
            // content line). A null/empty payload removes the run; a
            // non-empty payload over a node with no run inserts the lines
            // below its block. The position is served only for the
            // `#`-line comment class: a markup-comment format (XML) has no
            // below-block position distinct from its comment children, so a
            // foot write on one refuses cleanly instead of rendering `#`
            // lines into the document.
            jqf_codec_core::comment::FOOT => {
                if !is_hash_comment_format(input_format.as_str()) {
                    return Err(
                        publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                            "the {} format cannot carry the attached comment fact (its comment \
                         syntax is not the shared #-line form)",
                            input_format.as_str()
                        ))))),
                    );
                }
                let comment_marker_bytes = comment_marker(input_format).expect("a hash-comment format has a marker");
                let indent = node_line_indent(segment, start);
                if let Some((run_start, run_end, run_indent)) =
                    foot_comment_run(segment, value_end, comment_markers(input_format.as_str()))
                {
                    let replacement =
                        fact_comment_text(run_indent, &delta.payload, comment_marker_bytes).map_err(|message| {
                            let base = CodecError::new(jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
                            let Some(diagnostic) = jqf_source::Diagnostic::try_new(
                                jqf_source::Namespace::new("pipeline").code("comment-fact"),
                                jqf_source::Severity::Error,
                                &message,
                            ) else {
                                return publication.fail(PipelineFailure::Codec(base));
                            };
                            publication.fail(PipelineFailure::Codec(base.with_diagnostic(diagnostic)))
                        })?;
                    patches.push(
                        BytePatch::try_from_usize(run_start, run_end, replacement)
                            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                    );
                } else {
                    // No run below the block: insert the foot at the
                    // node's block end, indented one content step past
                    // the node (the column the codecs render block
                    // children at, which is also the column that
                    // re-attributes the inserted lines to the node on
                    // the verification re-decode).
                    let mut insert_indent = Vec::with_capacity(indent.len() + 2);
                    insert_indent.extend_from_slice(indent);
                    insert_indent.extend_from_slice(b"  ");
                    let replacement =
                        fact_comment_text(&insert_indent, &delta.payload, comment_marker_bytes).map_err(|message| {
                            let base = CodecError::new(jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
                            let Some(diagnostic) = jqf_source::Diagnostic::try_new(
                                jqf_source::Namespace::new("pipeline").code("comment-fact"),
                                jqf_source::Severity::Error,
                                &message,
                            ) else {
                                return publication.fail(PipelineFailure::Codec(base));
                            };
                            publication.fail(PipelineFailure::Codec(base.with_diagnostic(diagnostic)))
                        })?;
                    if !replacement.is_empty() {
                        let insert_at = segment[value_end..]
                            .iter()
                            .position(|byte| *byte == b'\n')
                            .map_or(segment.len(), |position| value_end + position + 1);
                        patches.push(
                            BytePatch::try_from_usize(insert_at, insert_at, replacement)
                                .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                        );
                    }
                }
            }

            // The METADATA roles (145 C5 / 142 W3): `.@style`, `.@tag`,
            // `.@anchor`, `.@alias`. The CODEC owns both the byte render and
            // the validity check — it reads the node's span and materialized
            // value from the document, validates the payload against its own
            // grammar, and returns the exact patches; a write it cannot honor
            // is a refusal error carrying its prose message (the
            // encode-or-report-a-loss law), and `None` means it does not
            // carry the role at all, refused here with the role named.
            "style" | "tag" | "anchor" | "alias" => {
                let Some(patches_out) = factory
                    .render_fact_delta(
                        document,
                        delta.node,
                        segment,
                        &delta.role,
                        &delta.kind,
                        &delta.payload,
                        resources,
                    )
                    .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
                else {
                    return Err(
                        publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                            "the {} format cannot carry the {} fact (the codec does not serve \
                             this metadata role)",
                            input_format.as_str(),
                            delta.role
                        ))))),
                    );
                };
                for patch in patches_out {
                    patches.push(
                        BytePatch::try_from_usize(patch.start, patch.end, patch.replacement)
                            .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))?,
                    );
                }
            }

            other => {
                return Err(
                    publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                        "the {} format cannot carry the {} fact (the seam renders only the \
                         comment positions it declares)",
                        input_format.as_str(),
                        other
                    ))))),
                );
            }
        }
    }
    let mixed = !extra.is_empty();
    patches.extend(extra);
    patches.sort_by_key(|patch| patch.span().start());
    let set = match PatchSet::try_new(None, segment.len(), patches) {
        Ok(set) => set,
        Err(_) if mixed => {
            return Err(publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(
                "a fact assignment and a value assignment overlap in the source; refused",
            )))));
        }
        // Two colliding FACT writes are ordinary input (e.g. two roles whose
        // renderers claim the same line): the same clean refusal the mixed
        // twin gets, never a machine fault.
        Err(error @ (PatchError::Overlap | PatchError::AmbiguousInsertion)) => {
            return Err(
                publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(&format!(
                    "the fact assignments could not be placed in the source: {error}"
                ))))),
            );
        }
        Err(error) => {
            return Err(publication.fail(PipelineFailure::Codec(edit_patch_error(error))));
        }
    };
    set.apply(None, segment)
        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_patch_error(error))))
}

/// Whether a format's comment syntax is the MARKUP-COMMENT class — the
/// element's comment CHILDREN rendered as `<!-- … -->` — served through
/// the one fact-write seam's HEAD comment role, where the codec renders
/// the bytes itself. XML is the one member; every other
/// format's comment class is the shared `#`-line form or none.
fn is_markup_comment_format(format: &str) -> bool {
    matches!(format, "xml")
}

/// Formats whose HEAD comment write is codec-owned (`render_fact_delta`),
/// not the shared `#`-line renderer. XML writes markup comments; JSONC
/// and JSON5 write `//` lines immediately before the member.
fn is_codec_comment_format(format: &str) -> bool {
    is_markup_comment_format(format) || matches!(format, "jsonc" | "json5")
}

/// Whether a format's comment syntax is the shared `#`-line class the seam's
/// line-based renderer produces. This is the seam's ONE declaration surface:
/// a format that declares a comment role AND uses `#`-lines is served here
/// by declaration; a format with a different syntax class (markup comments,
/// or none at all) refuses cleanly until its own splice policy lands.
fn is_hash_comment_format(format: &str) -> bool {
    // The flat-config grammars all take `#` as a line-comment marker
    // (properties also `!`, ini also `;`), so the shared `#`-line renderer
    // serves them; a flat-config comment block decodes back to the same
    // fact under every dialect.
    matches!(format, "toml" | "yaml" | "properties" | "ini" | "dotenv")
}

/// The line-comment markers of one format's grammar, as a byte set: `#` for
/// the whole class, `!` additionally under properties, `;` additionally
/// under ini. The seam's comment-block and foot-run walks recognize a
/// format's OWN markers so a `;`- or `!`-style comment block is addressable
/// exactly like a `#`-style one — the emitted `#` lines re-decode under
/// every dialect, so a write-back is byte-identical for the FACT whatever
/// the source marker was.
fn comment_markers(format: &str) -> &'static [u8] {
    match format {
        "properties" => b"#!",
        "ini" => b"#;",
        _ => b"#",
    }
}

/// Whether a format has the markup ATTRIBUTE grammar the seam's attribute
/// arm serves (the `.&name` write). XML is the one markup format with an
/// edit tier; HTML's edit is ruled out at the CLI, so it never reaches this
/// seam — the list names the formats whose attributes this lane can rewrite,
/// the same declaration surface as [`is_hash_comment_format`].
fn is_markup_attribute_format(format: &str) -> bool {
    matches!(format, "xml")
}

/// Whether a format's pre-value bytes are stream/file TRIVIA — comments, a
/// document marker, a prolog — that no canonical encoder emits, so the edit
/// floor may republish them before a re-encoded value. The binary container
/// formats are outside the class: jqfb's bytes before the root node's table
/// entry are the image header and pool chunks a fresh encode reproduces, so
/// prepending them would corrupt the image (CBOR and `MessagePack` bind no
/// pre-value bytes at all). A new edit-capable text format joins by
/// declaration, the same surface as [`is_hash_comment_format`]; an undeclared
/// format keeps today's drop-on-floor behavior, never a wrong byte.
fn pre_value_bytes_are_trivia(format: &str) -> bool {
    matches!(format, "json" | "jsonc" | "json5" | "toml" | "yaml" | "xml" | "ini")
}

/// The authored quoted-value span of one attribute on `element`, read from
/// the attribute fact. Missing when the element has no such attribute (the
/// insert hook then runs) or the codec did not bind a span.
fn attribute_value_span(
    document: &Document<'_>,
    element: NodeId,
    kind: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<jqf_source::Span>, DataError> {
    if document.fact_owner_indexed() {
        for fact_id in document.owner_fact_ids(element) {
            let fact = document.fact(*fact_id)?;
            if fact.role().as_str() == jqf_codec_core::markup::ATTRIBUTE_FACT && fact.kind().as_str() == kind {
                return Ok(fact.source_span());
            }
        }
        return Ok(None);
    }
    let mut reader = document.fact_reader(resources)?;
    let owner = LocalOwnerRef::Node(element);
    match reader.drain(resources, |fact| {
        if fact.owner() == owner
            && fact.role().as_str() == jqf_codec_core::markup::ATTRIBUTE_FACT
            && fact.kind().as_str() == kind
        {
            ControlFlow::Break(fact.source_span())
        } else {
            ControlFlow::Continue(())
        }
    })? {
        ControlFlow::Break(span) => Ok(span),
        ControlFlow::Continue(()) => Ok(None),
    }
}

/// The INLINE position's byte text: ` # text` at the value's line end, or an
/// empty byte string for a deletion (`null`/empty payload). An inline comment
/// is single-line by grammar, so a payload naming more than one line is
/// refused.
pub(crate) fn fact_inline_text(payload: &Value) -> Result<Vec<u8>, String> {
    match payload.untagged() {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => {
            let mut lines = items.iter();
            let Some(Value::String(text)) = lines.next().map(Value::untagged) else {
                return Err(String::from(
                    "an inline comment payload must be a list of text lines (or null to delete)",
                ));
            };
            if lines.next().is_some() {
                return Err(String::from(
                    "an inline comment is one line; a multi-line payload has no inline spelling",
                ));
            }
            let mut out: Vec<u8> = Vec::new();
            out.extend_from_slice(b" # ");
            // The engine's payload normalization splits embedded line breaks
            // before the delta lands here; refusing (rather than cutting)
            // keeps that drift a loud refusal instead of a truncated write.
            if text.as_str().contains(['\n', '\r']) {
                return Err(String::from("an inline comment line carries an embedded line break"));
            }
            out.extend_from_slice(text.as_str().as_bytes());
            Ok(out)
        }
        _ => Err(String::from(
            "an inline comment payload must be a list of text lines (or null to delete)",
        )),
    }
}

/// The node's own-line trailing comment region: the bytes from the value span's
/// end to the line end, when they are exactly trailing material (whitespace,
/// then a `#` comment running to the line end). `None` when the region is not
/// trailing material (a quoted string's closing quote, a `#` inside a value
/// region, another statement on the same line).
pub(crate) fn trailing_comment_span(segment: &[u8], value_end: usize, marker: &[u8]) -> Option<(usize, usize)> {
    let line_end = segment[value_end..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(segment.len(), |position| value_end + position);
    let tail = &segment[value_end..line_end];
    let trimmed = tail.trim_ascii_start();
    if trimmed.starts_with(marker) {
        Some((value_end, line_end))
    } else {
        None
    }
}

/// The comment marker one format writes: TOML and YAML write `#`, JSONC
/// writes `//`. `None` for a format with no comment fact at
/// all — the fact-write refusal.
pub(crate) fn comment_marker(format: &FormatId) -> Option<&'static [u8]> {
    match format.as_str() {
        "toml" | "yaml" | "properties" | "ini" | "dotenv" => Some(b"#"),
        "jsonc" | "json5" => Some(b"//"),
        _ => None,
    }
}

/// The comment block directly above a node's line: the maximal run of comment
/// and blank lines between the node's line and the previous content line.
///
/// This is the codecs' comment-ownership law read in REVERSE: a comment
/// belongs to the first node whose span starts at or after its end, so the
/// lines directly above a node (including the blanks between them) are its
/// leading comments — a trailing-after-value comment is the NEXT node's
/// leading comment, exactly as the decoders attach it. The walk stops at a
/// comment indented strictly DEEPER than the node's own line: under the
/// column rule such a comment is the closing block's FOOT (the parent's),
/// never this node's leading block, so a leading write never replaces a
/// comment it does not own. Returns
/// `(block_start, node_line_start, indent_bytes)`.
///
/// The root trailer comment (a trailing comment with no following node pins to
/// the root) is a recorded narrowing: the root's span covers the whole
/// document, so its trailer lines are inside the span and outside this
/// line-based walk. A root-comment write therefore fails verification loudly
/// rather than patching wrong bytes.
pub(crate) fn comment_block<'a>(source: &'a [u8], span_start: usize, markers: &[u8]) -> (usize, usize, &'a [u8]) {
    let node_line = line_start(source, span_start);
    let node_indent = node_line_indent(source, span_start).len();
    let mut block = node_line;
    while block > 0 {
        let prev = line_start(source, block - 1);
        let line = &source[prev..block];
        if line.trim_ascii().is_empty() {
            block = prev;
        } else if is_comment_line(line, markers) {
            // A comment indented strictly DEEPER than the node's own line is
            // not this node's leading block — under the column rule it is
            // the closing block's foot (the parent's foot). Stop the walk so
            // a leading write never replaces a comment it does not own. (The
            // flat-config grammars have no closing blocks; their decoders
            // attach every comment above an entry regardless of column, and
            // the walk's stop here leaves such a comment as an untouched
            // prefix the verification's `ends_with` law tolerates.)
            let column = line.len() - line.trim_ascii_start().len();
            if column > node_indent {
                break;
            }
            block = prev;
        } else {
            break;
        }
    }
    let indent = {
        let line = &source[node_line..];
        let ws = line.iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
        &line[..ws]
    };
    (block, node_line, indent)
}

/// The leading whitespace of the line containing `at`: the column the
/// node's own line starts at.
pub(crate) fn node_line_indent(source: &[u8], at: usize) -> &[u8] {
    let line = &source[line_start(source, at)..];
    let ws = line.iter().take_while(|b| matches!(b, b' ' | b'\t')).count();
    &line[..ws]
}

/// The foot-comment run below a node's block: the comment lines after the
/// node's last content line that belong to the node and not to a following
/// sibling. Returns `(run_start, run_end, indent_bytes)` — the run's own
/// leading whitespace, so a write-back reproduces the authored bytes.
///
/// The walk mirrors the codecs' foot-attribution law in reverse:
///
/// - From the node's span end, walk UP over comment and blank lines to the
///   node's last content line. The ROOT's span covers the whole document, so
///   its value end sits past its own trailer; the walk-up anchors it at the
///   last content line, which is what lets the document trailer — the root's
///   foot — be addressed as a span (the recorded root-trailer narrowing,
///   resolved here: a root foot write patches the trailer instead of failing
///   loudly).
/// - Walk DOWN over comment lines while they sit strictly DEEPER than the
///   node's own indent (the column rule read in reverse: a flush comment
///   below a flush node is the NEXT node's leading comment, never this
///   node's foot). A blank line or a content line ends the run.
/// - A node whose span already includes trailing comments (the root) takes
///   the whole remaining comment tail whatever its column — there is no next
///   node to protect, so the flush trailer is the root's foot.
///
/// `None` when no comment line sits below the node's block.
pub(crate) fn foot_comment_run<'a>(
    source: &'a [u8],
    value_end: usize,
    markers: &[u8],
) -> Option<(usize, usize, &'a [u8])> {
    // Anchor: the node's last content line end — walk up over comment and
    // blank lines (the root's span includes its trailer, so its value end
    // sits past them).
    let mut anchor = value_end;
    let mut span_includes_tail = false;
    loop {
        let start = line_start(source, anchor.saturating_sub(1));
        if start == anchor {
            break;
        }
        let line = &source[start..anchor];
        if line.trim_ascii().is_empty() || is_comment_line(line, markers) {
            anchor = start;
            span_includes_tail = true;
        } else {
            break;
        }
    }
    // The comment run below the anchor, down to the first non-comment line.
    let mut cursor = anchor;
    if !span_includes_tail {
        // The anchor is the node's last content BYTE, which may stop mid-line
        // (a value followed by a same-line terminator): the foot run begins
        // on the line BELOW it.
        let line_end = source[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(source.len(), |position| cursor + position);
        cursor = line_end + usize::from(line_end < source.len());
    }
    // Collect the candidate comment lines and the next content line's column
    // in one pass: the column bound needs the following content to judge the
    // candidates, and judging as it scans would need a lookahead.
    let mut lines: Vec<(usize, usize, usize)> = Vec::new(); // (start, end, column)
    let mut after = cursor;
    while after < source.len() {
        let end = source[after..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(source.len(), |position| after + position);
        let line = &source[after..end];
        let trimmed = line.trim_ascii_start();
        if is_comment_line(line, markers) {
            lines.push((after, end + usize::from(end < source.len()), line.len() - trimmed.len()));
            after = end + usize::from(end < source.len());
        } else if line.trim_ascii().is_empty() {
            // A blank line ends the run (it is not foot material and is
            // preserved as the separator to the next node).
            break;
        } else {
            break;
        }
    }
    if lines.is_empty() {
        return None;
    }
    // The next content line's column (the first non-comment, non-blank line
    // after the collected run), for the column bound.
    let next_column = {
        let content_line = source[after..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(source.len(), |position| after + position);
        let line = &source[after..content_line];
        line.len() - line.trim_ascii_start().len()
    };
    // The run: the contiguous comment lines strictly deeper than the next
    // content line's column (a flush comment below a flush node is the next
    // node's leading), or the whole tail when the node's span includes it
    // (the root's trailer — no next node to protect) or when nothing but
    // whitespace follows the run (a document-tail run is the ROOT's foot by
    // the trailer law, whatever its column).
    let at_tail = span_includes_tail || source[after..].trim_ascii().is_empty();
    let keep: Vec<&(usize, usize, usize)> = if at_tail {
        lines.iter().collect()
    } else {
        lines
            .iter()
            .take_while(|(_, _, column)| *column > next_column)
            .collect()
    };
    let first = keep.first()?;
    let (run_start, _, first_column) = **first;
    let run_end = keep.last().map_or(run_start, |(_, end, _)| *end);
    Some((run_start, run_end, &source[run_start..run_start + first_column]))
}

/// Whether one source line is a comment line under `markers` (its first
/// non-blank byte is one of the format's line-comment markers).
fn is_comment_line(line: &[u8], markers: &[u8]) -> bool {
    line.trim_ascii_start()
        .first()
        .is_some_and(|byte| markers.contains(byte))
}

/// The byte offset where the line containing `at` starts.
pub(crate) fn line_start(source: &[u8], at: usize) -> usize {
    source[..at]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1)
}

/// Renders one fact payload as comment lines: `indent + "# " + text + "\n"`
/// per line, mirroring the codecs' `write_leading_comments` renderer (the
/// encoder writes `# ` plus the text's FIRST line). A `null` payload or an
/// empty list deletes (an empty replacement removes the block); a payload that
/// is not a list of text lines is a refusal naming the shape.
pub(crate) fn fact_comment_text(indent: &[u8], payload: &Value, marker: &[u8]) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    match payload.untagged() {
        Value::Null => Ok(out),
        Value::Array(items) => {
            for item in items {
                let Value::String(text) = item.untagged() else {
                    return Err(String::from("a comment fact payload must be a list of text lines"));
                };
                out.extend_from_slice(indent);
                out.extend_from_slice(marker);
                out.extend_from_slice(b" ");
                // The engine's payload normalization splits embedded line
                // breaks before the delta lands here; refusing (rather than
                // cutting) keeps that drift a loud refusal instead of a
                // truncated write.
                if text.as_str().contains(['\n', '\r']) {
                    return Err(String::from("a comment fact line carries an embedded line break"));
                }
                out.extend_from_slice(text.as_str().as_bytes());
                out.push(b'\n');
            }
            Ok(out)
        }
        _ => Err(String::from(
            "a comment fact payload must be a list of text lines (or null to delete)",
        )),
    }
}

/// Re-decodes the patched source and asserts every fact delta: the materialized
/// value equals the program's output, and the node at the delta's value path
/// carries exactly the payload the run recorded (a fact write's whole effect is
/// invisible to a value-only check).
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one fact verification keeps its catalog, source, requirement, deltas, and value explicit"
)]
pub(crate) fn edit_document_facts_verify<Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'_>,
    start: usize,
    patched: &[u8],
    input_format: &FormatId,
    input_dialect: &DialectId,
    requirement: &AccessRequirement,
    deltas: &[FactDelta],
    expected_value: &Value,
    policy: PipelinePolicy<'_>,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<bool, PipelineError<Sink::Error>> {
    let base = u64::try_from(start).unwrap_or(u64::MAX);
    let synthetic = ResolvedSource::new(
        source.source(),
        source.label(),
        patched,
        source.base_offset().saturating_add(base),
    );
    let engine = access_input(
        catalog,
        synthetic,
        input_format,
        input_dialect,
        requirement,
        policy,
        resources,
        publication,
    )?;
    let (outcome, _) = engine.into_parts();
    let Some(product) = edit_product(&outcome) else {
        return Ok(false);
    };
    let document = product.document();
    if deltas.is_empty() {
        return Ok(true);
    }
    // The patched source must decode to the SAME value the run produced.
    // A fact-only program's output is the unchanged document; a mixed
    // program's output includes the value writes. A NON-CORE tag write
    // wraps the written node's value in a `Tagged` wrapper — the tag is the
    // write's whole point (a re-type) — so the comparison strips the
    // tag at every tag-delta path first: the PAYLOAD must be identical, the
    // tag may appear. For a metadata delta a value change is a LOUD refusal
    // naming the write; for a comment write it stays the internal contract.
    let mut decoded_root = document
        .materialize_root(resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
    let mut stripped = true;
    for delta in deltas {
        if delta.role.as_str() == "tag" && !strip_tag_at_path(&mut decoded_root, &delta.path, resources) {
            stripped = false;
        }
    }
    if !stripped || !jqf_engine::values_semantically_equal(&decoded_root, expected_value) {
        if let Some(role) = deltas
            .iter()
            .find(|delta| is_metadata_role(&delta.role))
            .map(|delta| delta.role.as_str())
        {
            let message = match role {
                "tag" => {
                    "the .@tag write would change the value (the tag's resolution is not \
                           consistent with the node's authored text); refused"
                }
                "alias" => {
                    "the .@alias write would change the value (the anchor's value differs \
                             from the node's original value); refused"
                }
                "style" => {
                    "the .@style write would change the value (the re-rendered scalar does \
                             not re-decode to the same text); refused"
                }
                _ => "the metadata fact write would change the value; refused",
            };
            return Err(publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(message)))));
        }
        return Ok(false);
    }
    for delta in deltas {
        let Some(node) = resolve_value_path(document, &delta.path) else {
            return Ok(false);
        };
        // The verification is role-keyed like the seam itself: each delta is
        // checked against the re-decoded fact at the SAME position. The
        // COMMENT roles carry a LIST payload (fact_payload_texts); the
        // ATTRIBUTE role carries a single text value, so its expectation is
        // extracted per-role below instead of through the list-only helper.
        let expected = match delta.role.as_str() {
            // The metadata roles carry a single TEXT payload resolved in
            // their own verification arms (145 C5); the list-only helper
            // would refuse a string as an unsupported shape.
            jqf_codec_core::markup::ATTRIBUTE_FACT | "style" | "tag" | "anchor" | "alias" => Vec::new(),
            _ => fact_payload_texts::<Sink>(&delta.payload, publication)?,
        };
        match delta.role.as_str() {
            // `.@comment` addresses the HEAD list only (`.@comment` ≡
            // `.@comment_head`). The write replaces
            // the comment block directly above the node and never touches an
            // inline comment absorbed from the previous line, so a re-decoded
            // node's list is the pre-existing absorbed prefix followed by the
            // written head — the absorbed inline sits on an earlier source line
            // than the written block, and comments attach in source order.
            // Verify the HEAD portion: the payload must be the trailing run of
            // the re-decoded list. An exact whole-list comparison would raise
            // an internal contract violation on every write over an absorbed
            // inline; the decoder-side attribution that would move the
            // absorbed inline out of the head list is deferred, untouched
            // here.
            jqf_codec_core::comment::HEAD => {
                let actual = read_comment_fact_texts::<Sink>(document, node, resources, publication)?;
                if !actual.ends_with(&expected) {
                    return Ok(false);
                }
            }
            // `.@comment_inline` addresses the INLINE list (the node's
            // own-line trailing comment), a single run with no absorbed
            // prefix — an EXACT comparison.
            jqf_codec_core::comment::INLINE => {
                let actual = read_comment_fact_texts_semantic::<Sink>(
                    document,
                    node,
                    jqf_codec_core::comment::INLINE,
                    resources,
                    publication,
                )?;
                if actual != expected {
                    return Ok(false);
                }
            }
            // The markup attribute role: the write is a single text value,
            // so the re-decoded attribute fact at the SAME element and kind
            // must equal the payload exactly. The delta's value path names
            // the ELEMENT (the walk stops before the accessor), and the
            // re-decoded document's `.&kind` fact is the written value.
            jqf_codec_core::markup::ATTRIBUTE_FACT => {
                let Value::String(written) = delta.payload.untagged() else {
                    return Ok(false);
                };
                let actual = read_attribute_fact(document, node, &delta.kind, resources);
                if actual.as_deref() != Some(written.as_str()) {
                    return Ok(false);
                }
            }
            // `.@comment_foot` addresses the FOOT list (the comment run below
            // the node's block, the document trailer for the root) — an
            // EXACT comparison like INLINE, because the run is the node's own
            // and nothing precedes it in the list.
            jqf_codec_core::comment::FOOT => {
                let actual = read_comment_fact_texts_semantic::<Sink>(
                    document,
                    node,
                    jqf_codec_core::comment::FOOT,
                    resources,
                    publication,
                )?;
                if actual != expected {
                    return Ok(false);
                }
            }
            // The METADATA roles (145 C5 / 142 W3): the re-decoded fact must
            // equal the written payload, except `.@alias` whose value
            // identity the global check above already enforced.
            "style" => {
                let actual = read_metadata_fact_text::<Sink>(document, node, "style", resources, publication)?;
                let Value::String(written) = delta.payload.untagged() else {
                    return Ok(false);
                };
                if actual.as_deref() != Some(written.as_str()) {
                    return Err(publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(
                        "the .@style write did not re-decode to the written style; refused",
                    )))));
                }
            }
            "tag" => {
                let Value::String(written) = delta.payload.untagged() else {
                    return Ok(false);
                };
                let expected_tag = resolve_written_tag(written.as_str());
                let handle = document
                    .node_handle(node)
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
                let actual_tag = document
                    .value_view(handle)
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
                    .tag()
                    .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?;
                if actual_tag.map(jqf_data::TagId::as_str) != Some(expected_tag.as_str()) {
                    return Err(publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(
                        "the .@tag write did not re-decode to the written tag; refused",
                    )))));
                }
            }
            "anchor" => {
                let actual = read_metadata_fact_text::<Sink>(document, node, "anchor", resources, publication)?;
                let Value::String(written) = delta.payload.untagged() else {
                    return Ok(false);
                };
                if actual.as_deref() != Some(written.as_str()) {
                    return Err(publication.fail(PipelineFailure::Codec(edit_refusal_failure(Some(
                        "the .@anchor write did not re-decode to the written anchor; refused",
                    )))));
                }
            }
            "alias" => {}
            // A role the seam refused cannot reach verification; a fresh
            // decoder here proves nothing about a position the seam never
            // patched.
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Resolves one value path (the delta's `["port"]`-shaped array of keys and
/// indices) to a node in a freshly decoded document; `None` when the path does
/// not exist.
pub(crate) fn resolve_value_path(document: &Document<'_>, path: &Value) -> Option<NodeId> {
    let Value::Array(components) = path.untagged() else {
        return None;
    };
    let mut node = document.root_handle();
    for component in components {
        let view = document.value_view(node).ok()?;
        match component.untagged() {
            Value::String(key) => {
                let object = view.object().ok()??;
                let child = object.get(key.as_str())?;
                node = document.node_handle(child.node()).ok()?;
            }
            Value::Number(number) => {
                let array = view.array().ok()??;
                let index = number
                    .to_i64()
                    .and_then(|index| jqf_data::resolve_index(array.len(), index))?;
                let child = array.get(index)?;
                node = document.node_handle(child.node()).ok()?;
            }
            _ => return None,
        }
    }
    document.resolve_node_handle(node).ok()
}

/// Walk `path` through `root` and unwrap a tag there, if the node is tagged.
pub(crate) fn strip_tag_at_path(root: &mut Value, path: &Value, _resources: &mut ResourceContext<'_>) -> bool {
    let Value::Array(components) = path.untagged() else {
        return false;
    };
    let mut current = root;
    for component in components {
        let Some(next) = (match (current, component.untagged()) {
            (Value::Object(object), Value::String(key)) => object.try_get_mut(key.as_str()).ok().flatten(),
            (Value::Array(array), Value::Number(number)) => {
                let Some(index) = number
                    .to_i64()
                    .and_then(|index| jqf_data::resolve_index(array.len(), index))
                else {
                    return false;
                };
                array.try_get_mut(index).ok().flatten()
            }
            _ => return false,
        }) else {
            return false;
        };
        current = next;
    }
    if let Value::Tagged { payload, .. } = current {
        *current = (**payload).clone();
    }
    true
}

/// Whether a fact role is one of the metadata vocabulary's semantic segments
/// (`style`/`tag`/`anchor`/`alias` — the YAML-side roles the write
/// allow-list admits beside the comment positions).
pub(crate) fn is_metadata_role(role: &str) -> bool {
    matches!(role, "style" | "tag" | "anchor" | "alias")
}

pub(crate) fn read_metadata_fact_text<Sink: ItemSink>(
    document: &Document<'_>,
    node: NodeId,
    semantic: &str,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Option<String>, PipelineError<Sink::Error>> {
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => return Ok(None),
        Err(error) => {
            return Err(publication.fail(PipelineFailure::Codec(edit_data_error(error))));
        }
    };
    let owner = LocalOwnerRef::Node(node);
    match reader
        .drain(resources, |fact| {
            if fact.owner() != owner {
                return ControlFlow::Continue(());
            }
            if !fact_role_serves(fact.role().as_str(), semantic) {
                return ControlFlow::Continue(());
            }
            let FactPayloadView::Text(text) = fact.payload() else {
                return ControlFlow::Continue(());
            };
            ControlFlow::Break(String::from(text))
        })
        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
    {
        ControlFlow::Break(text) => Ok(Some(text)),
        ControlFlow::Continue(()) => Ok(None),
    }
}

pub(crate) fn resolve_written_tag(payload: &str) -> String {
    if let Some(uri) = payload.strip_prefix("!<").and_then(|t| t.strip_suffix('>')) {
        return uri.to_owned();
    }
    if let Some(suffix) = payload.strip_prefix("!!") {
        return format!("tag:yaml.org,2002:{suffix}");
    }
    payload.to_owned()
}

/// Reads one markup ATTRIBUTE's text: the payload of the per-attribute fact
/// whose role is the markup attribute role and whose kind is the attribute
/// selector, or `None` for an element without that attribute. The engine's
/// `.&name` read and the fact-write verification share this one law.
fn read_attribute_fact(
    document: &Document<'_>,
    node: NodeId,
    kind: &str,
    resources: &mut ResourceContext<'_>,
) -> Option<String> {
    let mut reader = document.fact_reader(resources).ok()?;
    let owner = LocalOwnerRef::Node(node);
    match reader
        .drain(resources, |fact| {
            if fact.owner() == owner
                && fact.role().as_str() == jqf_codec_core::markup::ATTRIBUTE_FACT
                && fact.kind().as_str() == kind
                && let FactPayloadView::Text(text) = fact.payload()
            {
                return ControlFlow::Break(String::from(text));
            }
            ControlFlow::Continue(())
        })
        .ok()?
    {
        ControlFlow::Break(text) => Some(text),
        ControlFlow::Continue(()) => None,
    }
}

/// Reads the comment-fact texts attached to one node of a decoded document
/// (the `yaml.comment@1`/`toml.comment@1` list payload), or an empty list for a
/// node with none.
pub(crate) fn read_comment_fact_texts<Sink: ItemSink>(
    document: &Document<'_>,
    node: NodeId,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Vec<String>, PipelineError<Sink::Error>> {
    read_comment_fact_texts_semantic::<Sink>(document, node, jqf_codec_core::comment::HEAD, resources, publication)
}

/// Reads one comment POSITION's fact texts: the list payload of the fact
/// whose semantic segment is `semantic` (`"comment"`, `"comment_inline"`, or
/// `"comment_foot"`), or an empty list for a node with none. The semantic
/// segment is the engine selector the write allow-list keys on, so the
/// verification reads back exactly the position a delta wrote.
pub(crate) fn read_comment_fact_texts_semantic<Sink: ItemSink>(
    document: &Document<'_>,
    node: NodeId,
    semantic: &str,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Vec<String>, PipelineError<Sink::Error>> {
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => return Ok(Vec::new()),
        Err(error) => {
            return Err(publication.fail(PipelineFailure::Codec(edit_data_error(error))));
        }
    };
    let owner = LocalOwnerRef::Node(node);
    match reader
        .drain(resources, |fact| {
            if fact.owner() != owner {
                return ControlFlow::Continue(());
            }
            if !fact_role_serves(fact.role().as_str(), semantic) {
                return ControlFlow::Continue(());
            }
            let FactPayloadView::List(texts) = fact.payload() else {
                return ControlFlow::Continue(());
            };
            let mut out = Vec::new();
            for entry in texts.iter() {
                if let FactPayloadView::Text(text) = entry {
                    out.push(String::from(text));
                }
            }
            ControlFlow::Break(out)
        })
        .map_err(|error| publication.fail(PipelineFailure::Codec(edit_data_error(error))))?
    {
        ControlFlow::Break(out) => Ok(out),
        ControlFlow::Continue(()) => Ok(Vec::new()),
    }
}

/// Whether a fact role serves one semantic segment of the comment vocabulary:
/// exactly the segment, or the codec's namespaced form
/// (`<format>.<semantic>@<revision>` — the engine's semantic-segment
/// fact-role law, re-derived here for the edit lane).
pub(crate) fn fact_role_serves(role: &str, semantic: &str) -> bool {
    if role == semantic {
        return true;
    }
    let core = role.rsplit_once('.').map_or(role, |(_, rest)| rest);
    let core_semantic = core.split_once('@').map_or(core, |(semantic, _)| semantic);
    core_semantic == semantic
}

/// The delta payload as a text list: `null` (or an empty list) is deletion,
/// a list of strings is the replacement lines.
pub(crate) fn fact_payload_texts<Sink: ItemSink>(
    payload: &Value,
    publication: &Publication,
) -> Result<Vec<String>, PipelineError<Sink::Error>> {
    match payload.untagged() {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let Value::String(text) = item.untagged() else {
                    return Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::UnsupportedRepresentation,
                    ))));
                };
                out.push(String::from(text.as_str()));
            }
            Ok(out)
        }
        _ => Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::UnsupportedRepresentation,
        )))),
    }
}

/// Renders one leaf value as it would appear in VALUE position, or declines
/// to `None` when neither the codec's leaf grammar  nor its
/// whole-document encoder can render the value. A declined leaf takes the
/// [`EditDiff::Structural`] floor, never a hard error: the floor re-encodes
/// the whole document, which still fails loudly if the VALUE itself is
/// unrepresentable in the output format.
///
/// `authored` is the retained source bytes of the patch site (the span the
/// patch replaces), passed through to the codec so it can preserve the
/// site's authored spelling — a quote style is a format fact, so the diff
/// walk passes BYTES, never a parsed style (142 C1).
#[allow(
    clippy::too_many_arguments,
    reason = "the leaf seam carries the node context and the authored span the position-aware codecs render from"
)]
pub(crate) fn encode_edit_leaf<Sink: ItemSink>(
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    document: &Document<'_>,
    node: NodeId,
    path: &[String],
    source: &[u8],
    value: &Value,
    authored: Option<&[u8]>,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Option<Vec<u8>>, PipelineError<Sink::Error>> {
    match factory.render_leaf(document, node, path, source, value, authored, resources) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == jqf_codec_core::CodecFailureKind::UnsupportedRepresentation => {
            // The codec has no standalone value grammar; the whole-document
            // encoder is the fallback (a JSON scalar IS a document). If that
            // cannot render the value either, the leaf cannot be patched
            // minimally and the diff declines to the floor.
            match encode_edit_value::<Sink>(factory, reused, value, preservation, credits, resources, publication) {
                Ok(bytes) => Ok(Some(bytes)),
                Err(_) => Ok(None),
            }
        }
        Err(error) => Err(publication.fail(PipelineFailure::Codec(error))),
    }
}

/// Re-encodes a whole edited document in place of its source segment, restoring
/// the source's own final newline.
///
/// The block renderers (YAML block, TOML) emit no trailing newline of their
/// own, and the edit facade's suffix is empty for those formats by design (the
/// segment path echoes the file's own trailing bytes). A re-encode that
/// replaces the segment therefore drops the file's final newline unless it is
/// restored here — a mangled POSIX text file. The law is the segment's, not the
/// encoder's: a source that ends with `\n` re-encodes ending with `\n`, and a
/// source that does not stays as the encoder wrote it.
#[allow(
    clippy::too_many_arguments,
    reason = "the re-encode borrows the same encoder inventory the leaf and value arms own"
)]
pub(crate) fn encode_edit_whole<Sink: ItemSink>(
    segment: &[u8],
    unit_prefix: &[u8],
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    value: &Value,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Vec<u8>, PipelineError<Sink::Error>> {
    let mut encoded = encode_edit_value::<Sink>(factory, reused, value, preservation, credits, resources, publication)?;
    if segment.last() == Some(&b'\n') && encoded.last() != Some(&b'\n') {
        encoded.push(b'\n');
    }
    // The unit's own framing leads the re-encoded content: the floor renders
    // only the value, so the segment's pre-content bytes (a YAML `---` unit
    // marker, a leading comment block) are republished from the source here.
    if !unit_prefix.is_empty() {
        let mut framed = Vec::with_capacity(unit_prefix.len() + encoded.len());
        framed.extend_from_slice(unit_prefix);
        framed.extend_from_slice(&encoded);
        return Ok(framed);
    }
    Ok(encoded)
}

pub(crate) fn encode_edit_value<Sink: ItemSink>(
    factory: &ErasedEncoderFactory,
    reused: &mut ReusableEncoderSession,
    value: &Value,
    preservation: PreservationRequest,
    credits: u32,
    resources: &mut ResourceContext<'_>,
    publication: &Publication,
) -> Result<Vec<u8>, PipelineError<Sink::Error>> {
    let owned = value.clone();
    let mut session = factory
        .start_reusing(EncodeItem::owned(&owned), preservation, resources, reused)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let mut bytes = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut bytes);
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(credits);
        session
            .encode(&mut sink, &mut run)
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    }
    session.recycle(reused);
    Ok(bytes)
}

/// Publishes one edited document's bytes and its facade suffix as one item.
pub(crate) fn publish_edit_item<Sink: ItemSink>(
    sink: &mut Sink,
    index: u64,
    bytes: &[u8],
    framing: &[u8],
    resources: &mut ResourceContext<'_>,
    credits: u32,
    publication: &mut Publication,
) -> Result<(), PipelineError<Sink::Error>> {
    admit_visible_boundary(resources, credits, publication, true)?;
    sink.begin_item(index)
        .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    publication.item_open = true;
    let item_start = publication.published_bytes;
    publish_all(bytes, resources, credits, sink, publication)?;
    let codec_end = publication.published_bytes;
    publish_all(framing, resources, credits, sink, publication)?;
    let item_end = publication.published_bytes;
    let report = EncodedItemReport {
        physical_encoder: PhysicalRouteId::UNSPECIFIED,
        preservation: None,
        codec_bytes: checked_delta::<Sink::Error>(codec_end, item_start, publication)?,
        framing_bytes: checked_delta::<Sink::Error>(item_end, codec_end, publication)?,
        // The edit lane publishes a whole document as bytes; there is no
        // single expression-output value to judge (`-e` is rejected with
        // `--edit` for exactly this reason), and the bytes are always a
        // valid JSON document — never a raw-printed root text.
        value_truthy: None,
        value_empty_array: None,
        raw_text_root: false,
    };
    admit_visible_boundary(resources, credits, publication, true)?;
    sink.finish_item(index, report)
        .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    publication.item_open = false;
    publication.completed_items = publication
        .completed_items
        .checked_add(1)
        .ok_or_else(|| overflow::<Sink::Error>(publication))?;
    Ok(())
}

/// Maps a document-read failure into the codec channel the lane reports on.
pub(crate) fn edit_data_error(error: DataError) -> CodecError {
    match error {
        DataError::Resource(error) => error.into(),
        DataError::Control(error) => error.into(),
        _other => CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
            contract: "edit lane document read",
        }),
    }
}

/// The codec's edit-refusal message for one document node, when it carries an
/// attached fact with the format-neutral [`jqf_codec_core::EDIT_REFUSAL_ROLE`]
/// (a YAML node an alias references). The payload is the prose
/// the codec owns; a document without the fact index has no refusals to
/// report.
pub(crate) fn edit_refusal_message(document: &Document<'_>, node: NodeId) -> Result<Option<String>, DataError> {
    if !document.fact_owner_indexed() {
        return Ok(None);
    }
    for fact_id in document.owner_fact_ids(node) {
        let fact = document.fact(*fact_id)?;
        if fact.role().as_str() == jqf_codec_core::EDIT_REFUSAL_ROLE {
            return Ok(match fact.payload() {
                FactPayloadView::Text(text) => Some(text.to_owned()),
                _ => Some(String::new()),
            });
        }
    }
    Ok(None)
}

/// The refusal's terminal, prose-rendered codec failure: the codec's message
/// rides a message-only diagnostic in the pipeline namespace, exactly the
/// shape [`single_document_error`] gives the multi-document refusal, so the
/// failure reads as words about the EDIT, never a bare class name.
pub(crate) fn edit_refusal_failure(message: Option<&str>) -> CodecError {
    let base = CodecError::new(jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) = jqf_source::Diagnostic::try_new(
        jqf_source::Namespace::new("pipeline").code("edit-refusal"),
        jqf_source::Severity::Error,
        message.unwrap_or("this value cannot be edited in place"),
    ) else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

/// Refuses an edit that touches an alias-referenced node's content: the
/// codec's prose message rides the message-only diagnostic, and the caller
/// fails the publication with it (the `Sink`-generic `fail` infers at the
/// call site, so the error itself is sink-free).
pub(crate) fn edit_refusal_error(document: &Document<'_>, node: NodeId) -> CodecError {
    let message = edit_refusal_message(document, node).ok().flatten();
    edit_refusal_failure(message.as_deref())
}

/// The alias-refusal gate: one decision point shared by every
/// refusal site in the diff walk and the fact-write lane: a change whose
/// patch would land inside an alias-shared node's authored span refuses with
/// the codec's prose — unless the request set `--edit-expand-alias`, which
/// accepts the anchor-rewrite semantics: the patch rewrites
/// the shared anchor, so EVERY alias site changes. The escape hatch is not a
/// correctness fix — the edit is the same anchor rewrite the refusal
/// describes — it is the user's explicit acceptance, recorded once per
/// request so the host can warn that it engaged.
pub(crate) fn refuse_shared_edit<E>(
    publication: &Publication,
    document: &Document<'_>,
    node: NodeId,
    within_shared: bool,
    resources: &ResourceContext<'_>,
) -> Result<(), PipelineError<E>> {
    if !within_shared {
        return Ok(());
    }
    if resources.edit_expand_alias() {
        resources.note_edit_alias_expansion();
        return Ok(());
    }
    Err(publication.fail(PipelineFailure::Codec(edit_refusal_error(document, node))))
}

/// A patch the lane built must always be valid; a failure is an internal
/// contract violation, never a caller mistake.
pub(crate) fn edit_patch_error(_: PatchError) -> CodecError {
    CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
        contract: "edit lane patch set",
    })
}

/// The edit lane's internal invariant failures.
pub(crate) fn edit_contract<E>(publication: &Publication) -> PipelineError<E> {
    publication.fail(PipelineFailure::Codec(CodecError::new(
        jqf_codec_core::CodecFailureKind::InternalContractViolation {
            contract: "edit lane document authority",
        },
    )))
}
