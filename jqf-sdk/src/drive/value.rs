//! The value drive family (122 W4-T2 split of the former pipeline.rs).

use super::{
    AccessReport, AccessRequirement, CodecCatalog, CodecError, CodecInputOutcome, CodecInputResult, CodecRunContext,
    CompiledProgram, DataError, Diagnostics, DialectId, EncodeRequest, EncodedItemReport, EngineResult, EngineRun,
    EngineRunStream, FacadeFraming, FactDelta, FormatId, InputLines, Integer, ItemSink, Number, ObjectBuilder,
    ObjectKey, OrderedEncodingPolicy, PhysicalRouteId, PipelineDisposition, PipelineError, PipelineFailure,
    PipelinePolicy, PipelineReport, Publication, PublicationStatus, RaisedError, ResolvedSource, ResourceContext,
    ReusableAccessSession, ReusableEncoderSession, RunError, RunInput, RunPoll, RuntimeError, SequenceValueError,
    ToOwned, Value, access_input, admit_visible_boundary, checked_delta, decode_sequence_item, encode_one,
    is_per_value_codec_kind, located_range_declined, overflow, publish_all, pushdown_error, require_forward_progress,
    resume, set_lazy_deferred, skip_value_separator, split_run_error, try_lower_root_requirement, validate_credits,
};
use jqf_engine::CodecRequirementPolicy;

/// The outcome of a source-preserving round-trip attempt.
#[derive(Debug)]
pub enum RoundtripRun {
    /// The retained source bytes were published verbatim as exactly one item.
    Published(PipelineReport),
    /// The source was not echoable; the already-decoded document was encoded
    /// as identity so the floor does not decode a second time.
    Encoded(PipelineReport),
    /// The route declined: nothing was published, and the caller reruns its
    /// ordinary route, which owns the authoritative bytes and failure class.
    Declined,
}

/// Publishes the retained source bytes VERBATIM when the program is provably
/// the identity filter and the whole input is exactly one document of the same
/// format/dialect pair the output asks for.
///
/// This is the first slice of the source-preserving round-trip vertical
/// (`presentation_roundtrip`): a document-aware edit that is byte-faithful on
/// the parts the program did not touch. Today the only program that touches
/// nothing is identity, so the lane validates and echoes; the edit lanes that
/// reuse untouched source bytes are later slices over the same seam.
///
/// The route is deliberately conservative. It DECLINES — it never errors on a
/// shape it cannot serve — when the program is not identity, when the
/// decode policy has not opted into adjacent values (the route needs each
/// value's consumed offset to prove single-documentness), when the input and
/// output format or dialect identities differ (the retained bytes are only
/// publishable through the format that owns them), when the encoder factory
/// says these options would not emit the dialect's canonical spelling
/// (indent, `-S`, `-a`, `-r`), when a split destination is set, or when the
/// input contains more than one adjacent text. A malformed SINGLE document fails here exactly
/// as `execute_sequence` fails it, because the route validates by decoding:
/// validate-everything-first, and a corrupt byte in a document the program
/// would have echoed must fail the echo exactly as it fails the floor. A
/// malformed second value is the caller's floor problem: the lane returns
/// [`RoundtripRun::Declined`] as soon as a second text is detected, so the
/// fallback owns that failure's authoritative bytes and class.
///
/// Publication is the retained bytes and NOTHING else: no encoder is
/// constructed and no facade framing is applied, or the echo would not be the
/// input. The report counts the echoed bytes as codec bytes and the physical
/// encoder as [`PhysicalRouteId::UNSPECIFIED`] (there is none).
#[allow(
    clippy::too_many_arguments,
    reason = "the pipeline keeps each ownership boundary explicit, matching its sibling lanes"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one coherent route: catalog resolve, provider open, echo, report — splitting it would hand fragments the retained-bytes invariant that only holds end to end"
)]
pub(crate) fn execute_source_roundtrip<Sink: ItemSink>(
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
    encode_on_noncanonical: bool,
) -> Result<RoundtripRun, PipelineError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    if !program.is_identity()
        || !policy.decode.allow_adjacent_values
        || input_format.as_str() != output_format.as_str()
        || input_dialect.as_str() != output_dialect.as_str()
        // The split destination names each item by running the split program;
        // the echo publishes retained bytes without running it.
        || policy.split.is_some()
    {
        return Ok(RoundtripRun::Declined);
    }
    // The canonical-form question: under these encode options, does this
    // dialect emit its identity spelling? JSON: compact, no sort, no
    // ascii-escape, no raw-string rewrite. Other formats: their
    // identity-encode dialect. The request carries encode_options as
    // `&dyn Any`; the factory downcasts its own type.
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
    if !factory.emits_canonical_form() {
        return Ok(RoundtripRun::Declined);
    }
    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    // The validation-only decode : the round-trip's whole purpose
    // is to validate the input and learn the canonicality verdict — it never
    // materializes the document it decodes. The S2 route decision (the
    // identity consumes the whole document) would build every node eagerly,
    // which is exactly the work the echo exists to skip, so the validation
    // pass FORCES the container-span frontier ON ITS OWN REQUIREMENT —
    // containers below the root are deferred to spans, the canonicality
    // hooks run over the deferred scan (the S1 law), and the document is
    // discarded after the verdict. The frontier travels with the requirement
    // (the Q4b law on this tree), so no global and no guard: the caller's
    // policy and every concurrent session are untouched by construction.
    let requirement = program
        .try_requirement(resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        .with_lazy_frontier(1)
        // The probe is the roundtrip's whole reason: its per-key duplicate
        // fingerprints decide the canonicality verdict the echo gates on.
        .with_canonicality_probe(true);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| publication.fail(PipelineFailure::AccessBind(error)))?;
    // One recycled access session for the whole validation pass: each value
    // resets the previous value's retained workspaces instead of constructing
    // and dropping a session per value.
    let mut reuse = ReusableAccessSession::new();
    let mut offset = 0usize;
    // Where the one echoed value's own bytes begin, after the separator scan
    // skipped whatever led up to it.
    let mut value_start = 0usize;
    let mut access_report: Option<AccessReport> = None;
    // Judged from the Located value this lane already holds, while the
    // document is still live. `None` here would fabricate an empty
    // exit-status report (`-e` exits 0 on a falsy file that the sequence
    // floor and stdin both exit 1 on).
    let mut value_truthy: Option<bool> = None;
    let mut value_empty_array: Option<bool> = None;
    let mut last_outcome: Option<CodecInputOutcome> = None;
    let mut last_canonical = false;
    loop {
        let start = skip_value_separator(source.bytes(), offset, policy.decode.value_separator);
        if start >= source.bytes().len() {
            break;
        }
        if access_report.is_some() {
            // A second adjacent text: the retained bytes are a stream, not one
            // document, and whole-buffer echo would fuse unrelated texts. The
            // caller's floor owns this shape, malformed tail and all.
            return Ok(RoundtripRun::Declined);
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
        let (outcome, report) = engine.into_parts();
        // The S4 canonicality gate: the source echo is byte-identical to the
        // compact render ONLY when the decoded document is canonical. A
        // non-canonical document declines here — nothing has been published —
        // and the caller's floor reruns the whole request and renders
        // authoritative bytes. The gate reads the decoded DOCUMENT's verdict
        // (the decode-side hooks clear it on any disqualifier); the outcome
        // variant is the located result the whole-document route publishes
        // for the identity, and any other shape declines defensively.
        let canonical = match &outcome {
            CodecInputOutcome::Result(EngineResult::Located(located)) => {
                located.product().document().source_canonical()
            }
            _ => false,
        };
        if !canonical && !encode_on_noncanonical {
            return Ok(RoundtripRun::Declined);
        }
        if let CodecInputOutcome::Result(result) = &outcome {
            value_truthy = jqf_engine::is_truthy(result).ok();
            value_empty_array = jqf_engine::is_empty_array(result).ok();
        }
        access_report = Some(report);
        last_outcome = Some(outcome);
        last_canonical = canonical;
        value_start = start;
        let consumed_usize = usize::try_from(consumed).map_err(|_| overflow::<Sink::Error>(&publication))?;
        offset = start
            .checked_add(consumed_usize)
            .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
    }
    // The empty input decodes zero documents: decline, and the floor publishes
    // its authoritative empty result.
    let Some(access) = access_report else {
        return Ok(RoundtripRun::Declined);
    };
    if !last_canonical {
        let Some(CodecInputOutcome::Result(result)) = last_outcome else {
            return Ok(RoundtripRun::Declined);
        };
        let mut reused_encoder = ReusableEncoderSession::new();
        encode_one(
            &factory,
            &mut reused_encoder,
            &result,
            0,
            encoding_policy,
            framing,
            resources,
            sink,
            &mut publication,
        )?;
        return Ok(RoundtripRun::Encoded(PipelineReport {
            publication: PublicationStatus::Complete {
                items: publication.completed_items,
                published_bytes: publication.published_bytes,
            },
            disposition: PipelineDisposition::Emitted,
            access,
        }));
    }
    admit_visible_boundary(resources, policy.cooperative_credits, &publication, true)?;
    sink.begin_item(0)
        .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    publication.item_open = true;
    let item_start = publication.published_bytes;
    // The echo publishes the VALUE's bytes — the source from the value's start
    // to the consumed offset — never the whitespace on either side of it,
    // which belongs to the facade's own framing exactly as the render's
    // trailing newline does. Without the trailing half a line feed would
    // disqualify every file-shaped input, forfeiting the canonical-identity
    // prize on exactly the inputs that have it; without the leading half the
    // echo would republish indentation the compact render never emits, which
    // is the same divergence read from the other end.
    publish_all(
        &source.bytes()[value_start..offset],
        resources,
        policy.cooperative_credits,
        sink,
        &mut publication,
    )?;
    let codec_bytes = checked_delta::<Sink::Error>(publication.published_bytes, item_start, &publication)?;
    // The facade's own item suffix, exactly as the encode lanes publish it:
    // the echo is the VALUE's bytes, and the suffix is the facade's framing.
    let framing_start = publication.published_bytes;
    publish_all(
        framing.item_suffix,
        resources,
        policy.cooperative_credits,
        sink,
        &mut publication,
    )?;
    let framing_bytes = checked_delta::<Sink::Error>(publication.published_bytes, framing_start, &publication)?;
    admit_visible_boundary(resources, policy.cooperative_credits, &publication, true)?;
    sink.finish_item(
        0,
        EncodedItemReport {
            physical_encoder: PhysicalRouteId::UNSPECIFIED,
            preservation: None,
            codec_bytes,
            framing_bytes,
            value_truthy,
            value_empty_array,
            raw_text_root: false,
        },
    )
    .map_err(|error| publication.fail(PipelineFailure::Sink(error)))?;
    publication.item_open = false;
    publication.completed_items = publication
        .completed_items
        .checked_add(1)
        .ok_or_else(|| overflow::<Sink::Error>(&publication))?;
    Ok(RoundtripRun::Published(PipelineReport {
        publication: PublicationStatus::Complete {
            items: publication.completed_items,
            published_bytes: publication.published_bytes,
        },
        disposition: PipelineDisposition::Emitted,
        access,
    }))
}

/// Executes one retained source through exact access, engine handoff, and ordered encoding.
#[allow(
    clippy::too_many_arguments,
    reason = "the pipeline keeps each ownership boundary explicit"
)]
#[allow(
    clippy::too_many_lines,
    reason = "the whole single-value pipeline is one sequential law; the diagnostics wrap adds               the terminal bookkeeping beside the run body so the two are read together"
)]
pub(crate) fn execute_value_document<'a, Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'_>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    requirement: &AccessRequirement,
    program: &CompiledProgram,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'a>,
    diagnostics: Option<&'a Diagnostics>,
    sink: &mut Sink,
) -> Result<PipelineReport, PipelineError<Sink::Error>> {
    // The diagnostic sink rides the request context exactly like the stderr
    // channel: installed here, emitted into by the engine's raise sites, and
    // read back by the caller after the run. `Off` is `None`, and the whole
    // path is a no-op.
    if let Some(diagnostics) = diagnostics {
        resources.set_diagnostics(diagnostics);
    }
    let outcome = (|| -> Result<PipelineReport, PipelineError<Sink::Error>> {
        let mut publication = Publication::new();
        validate_credits(policy.cooperative_credits, &publication)?;
        let (access, run_whole) = match access_input(
            catalog,
            source,
            input_format,
            input_dialect,
            requirement,
            policy,
            resources,
            &publication,
        ) {
            Ok(access) => (access, false),
            Err(error) if located_range_declined(requirement, &error) => {
                let whole = try_lower_root_requirement(
                    CodecRequirementPolicy::new(policy.decode.validation, policy.decode.diagnostics),
                    None,
                    resources,
                )
                .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
                (
                    access_input(
                        catalog,
                        source,
                        input_format,
                        input_dialect,
                        &whole,
                        policy,
                        resources,
                        &publication,
                    )?,
                    true,
                )
            }
            Err(error) => return Err(error),
        };
        if let Some(diagnostics) = diagnostics {
            diagnostics.record_route_named("single-document");
        }
        let (engine, access_report) = access.into_parts();
        // The W3-T1 lazy default defers container spans below the frontier;
        // the deferred count is a document fact, read while the document is
        // alive; the materialized count accumulates on the request context as
        // the run touches spans, and the CLI's explain route block reads both
        // after the run.
        set_lazy_deferred(resources, &engine);
        let encoding_policy = policy.encoding();
        let encoder = catalog
            .encoder(output_format, output_dialect)
            .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
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
        // Plan 133 R1's COUNT fast-path: a count-class program's value is
        // served by the document-core consumer from the lazy document's span
        // skeleton — no executor run, no leaf materialization. A decline
        // falls through to the ordinary residual run over the same outcome.
        if let Some(count) = count_answer(&engine, program, resources)
            .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        {
            encode_one(
                &factory,
                &mut reused_encoder,
                &EngineResult::owned(count),
                0,
                encoding_policy,
                framing,
                resources,
                sink,
                &mut publication,
            )?;
            return Ok(PipelineReport {
                publication: PublicationStatus::Complete {
                    items: 1,
                    published_bytes: publication.published_bytes,
                },
                disposition: PipelineDisposition::Emitted,
                access: access_report,
            });
        }
        // Plan 133 R6's ELEMENT-ITERATION fast-path: a fan-out/fold program's
        // values are served by the document-core consumer iterating the lazy
        // document's span skeleton — no executor run, no whole-tree
        // materialization. A decline falls through to the ordinary residual
        // run over the same outcome (which, for an element program, is the
        // whole program over the whole document).
        {
            match element_answer(
                &engine,
                program,
                &factory,
                &mut reused_encoder,
                encoding_policy,
                framing,
                resources,
                sink,
                &mut publication,
            )? {
                ElementAnswer::FanOut { items } => {
                    return Ok(PipelineReport {
                        publication: PublicationStatus::Complete {
                            items,
                            published_bytes: publication.published_bytes,
                        },
                        disposition: PipelineDisposition::Emitted,
                        access: access_report,
                    });
                }
                ElementAnswer::Fold(state) => {
                    encode_one(
                        &factory,
                        &mut reused_encoder,
                        &EngineResult::owned(state),
                        0,
                        encoding_policy,
                        framing,
                        resources,
                        sink,
                        &mut publication,
                    )?;
                    return Ok(PipelineReport {
                        publication: PublicationStatus::Complete {
                            items: 1,
                            published_bytes: publication.published_bytes,
                        },
                        disposition: PipelineDisposition::Emitted,
                        access: access_report,
                    });
                }
                ElementAnswer::None => {}
            }
        }
        // Everything flows through the residual: a resolved value or a pushed-down
        // missing/null both stream through it (an identity residual forwards one
        // item, an `.[]` residual fans out or errors). A flagged-step pushdown
        // mismatch suppresses; an unflagged pushdown mismatch, and any typed/iterate
        // mismatch discovered mid-fan-out, is a hard error for this single value.
        // A Located range decline rebinds the whole-document floor: the prefix
        // was not resolved, so the whole program runs.
        let (disposition, stream) = match if run_whole {
            program.try_run_whole_value(engine, resources)
        } else {
            program.try_run(engine, resources)
        }
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?
        .with_iteration_cap(policy.max_iterations)
        {
            EngineRun::Stream { stream, input } => (disposition_of(input), stream),
            // A suppressed value publishes zero items: report a completed
            // zero-item publication, byte-compatible with jq (prints nothing, exit
            // 0), without constructing an encoder or driving a stream.
            EngineRun::Suppressed => {
                return Ok(PipelineReport {
                    publication: PublicationStatus::Complete {
                        items: 0,
                        published_bytes: 0,
                    },
                    disposition: PipelineDisposition::Suppressed,
                    access: access_report,
                });
            }
            // The single-value entry never reports to a sink; it aborts with the
            // exit class and leaves rendering to its caller. A PUSHED-DOWN
            // mismatch is still a typed failure with a rendered message — it is
            // reported to the sink here, exactly like a mid-residual runtime
            // failure, so the abort does not swallow the stderr line.
            EngineRun::Pushdown(error) => {
                let mut lines = InputLines::new();
                let input_line = lines.at_value_end(source.bytes(), source.bytes().len());
                return Err(report_and_fail_runtime(
                    sink,
                    &mut publication,
                    pushdown_error(error),
                    input_line,
                    None,
                ));
            }
        };
        let mut publication = Publication::new();
        match drive_run_stream(
            &factory,
            &mut reused_encoder,
            stream,
            0,
            encoding_policy,
            framing,
            resources,
            sink,
            &mut publication,
        )? {
            StreamStop::Complete(items) => Ok(PipelineReport {
                publication: PublicationStatus::Complete {
                    items,
                    published_bytes: publication.published_bytes,
                },
                disposition,
                access: access_report,
            }),
            // A single value aborts on a runtime mismatch, keeping the prefix it
            // already published (partial-publication status is retained). The
            // typed failure's message is reported to the sink FIRST — the reference's
            // stderr line — so the abort does not swallow the message the
            // sequence path's continue-past law preserves: the terminal failure
            // only sets the exit class (the facade's `Reported` law).
            StreamStop::Runtime { error, .. } => {
                let mut lines = InputLines::new();
                let input_line = lines.at_value_end(source.bytes(), source.bytes().len());
                Err(report_and_fail_runtime(sink, &mut publication, error, input_line, None))
            }
            StreamStop::SplitName { index, detail, .. } => {
                Err(publication.fail(PipelineFailure::SplitName { index, detail }))
            }
            StreamStop::ValueFailure { error, .. } => {
                let mut lines = InputLines::new();
                let input_line = lines.at_value_end(source.bytes(), source.bytes().len());
                Err(report_and_fail_codec(sink, &mut publication, error, input_line, None))
            }
            // A single value aborts on an uncaught program-raised error VALUE;
            // its compact rendering is reported to the sink before the owned
            // value travels in the terminal failure, for the same reason.
            StreamStop::Raised { value, .. } => {
                let mut lines = InputLines::new();
                let input_line = lines.at_value_end(source.bytes(), source.bytes().len());
                // Best-effort report, exactly like the sequence drive: a
                // report-construction failure never masks the raise it was
                // rendering — the terminal failure stays `Raised`.
                Err(report_and_fail_raised(sink, &mut publication, value, input_line, None))
            }
            StreamStop::Halt { status, message } => Err(publication.fail(PipelineFailure::Halt { status, message })),
        }
    })();
    // The terminal bookkeeping happens OUTSIDE the run body: the cost record
    // reads the ledger after every charge is settled, and the failure record
    // is the terminal record of an errored run (it survives any overflow).
    match outcome {
        Ok(report) => {
            if let Some(diagnostics) = diagnostics {
                diagnostics.record_cost(&resources.snapshot());
            }
            Ok(report)
        }
        Err(error) => {
            if let Some(diagnostics) = diagnostics {
                diagnostics.record_failure(error.failure());
            }
            Err(error)
        }
    }
}

/// How one RANGE-LOCATE run ended — the range-projection bare-slice
/// publish (`.catalog[100:110]`).
///
/// Every non-`Completed` outcome leaves the sink untouched. The drive publishes
/// exactly one item and only after the codec has resolved the whole path, so
/// each decline arm is reachable strictly before the first byte: a bind failure
/// precedes the first poll, and the container dispatch is read off the LOCATED
/// record, which is the codec's terminal.
#[derive(Debug)]
pub enum RangeLocateRun {
    /// The located range array was published as one item.
    Completed(PipelineReport),
    /// The input was not a single document; fall back to `execute_sequence`.
    NotSingleDocument,
    /// The route declined this (codec, document) pair — the codec advertises no
    /// located route for a range footprint, or the container at the path is not
    /// an ARRAY. NOTHING was published; fall back to the ordinary route.
    NotApplicable,
}

/// Publishes the ARRAY one trailing slice materializes, decoded from the byte
/// region that holds exactly its in-range elements.
///
/// `requirement` must be `program`'s range-locate requirement
/// ([`CompiledProgram::try_range_locate_requirement`]) and `program` must be
/// [`CompiledProgram::range_locate_eligible`]. There is no residual: the row's
/// whole program IS the path, so the located value is the published value and no
/// program graph runs at all.
///
/// # THE CONTAINER DISPATCH (range-projection)
///
/// The codec learns the len-relative clamp and nothing else: at a range step
/// over a non-array it reports the container KIND. This drive turns every one of
/// those into [`RangeLocateRun::NotApplicable`], so the reference's string slice, its
/// `null` slice, its missing-path `null` and its object slice error all come out
/// of the FLOOR, rendered from the AUTHORED bound spellings the pushdown never
/// consumed. That is the decline arm the `Located` route was missing, and it
/// lives HERE — before publication — rather than in `EngineRun`.
///
/// # Errors
///
/// Returns [`PipelineError`] when registry selection, encoding, or the sink
/// fails. A codec failure during validation is NOT an error: it is
/// [`RangeLocateRun::NotSingleDocument`], because nothing is published before
/// the located record exists and the ordinary route reports the same failure
/// with the sequence path's per-value semantics.
#[allow(
    clippy::too_many_arguments,
    reason = "the pipeline keeps each ownership boundary explicit"
)]
pub(crate) fn execute_range_locate<Sink: ItemSink>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'_>,
    input_format: &FormatId,
    input_dialect: &DialectId,
    requirement: &AccessRequirement,
    output_format: &FormatId,
    output_dialect: &DialectId,
    policy: PipelinePolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<RangeLocateRun, PipelineError<Sink::Error>> {
    let mut publication = Publication::new();
    validate_credits(policy.cooperative_credits, &publication)?;
    let decoder = catalog
        .decoder(input_format, input_dialect)
        .map_err(|error| publication.fail(PipelineFailure::Registry(error)))?;
    let mut provider = decoder
        .create_provider(source, policy.decode, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let Ok(handle) = provider.bind(requirement) else {
        return Ok(RangeLocateRun::NotApplicable);
    };
    // W3-T3: a DEMAND-fallback binding serves the whole document, not the
    // range this drive locates — decline and let the ordinary drive take it.
    if handle.demand_fallback() {
        return Ok(RangeLocateRun::NotApplicable);
    }
    let mut access = provider
        .open(&handle, resources)
        .map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    // Nothing is published before the located record exists, so a decode
    // failure is a clean fall-through rather than a failure.
    let outcome = {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(policy.cooperative_credits);
        match access.decode(&mut run) {
            Ok(outcome) => outcome,
            // A failed decode (including the route's own terminal) means the
            // ordinary route owns this request, exactly as the poll-era
            // Complete/error arms fell through.
            Err(_) => return Ok(RangeLocateRun::NotSingleDocument),
        }
    };
    let engine =
        CodecInputResult::try_from_access(outcome).map_err(|error| publication.fail(PipelineFailure::Codec(error)))?;
    let access_report = engine.report();
    // The scoped session validates only the FIRST adjacent value when the
    // request opted into adjacency, and this rung publishes the WHOLE request's
    // output in one item. A second value therefore means the sequence path owns
    // this input, and nothing has been published yet.
    if let Some(consumed) = access_report.consumed_offset() {
        let consumed = usize::try_from(consumed).unwrap_or(usize::MAX);
        if skip_value_separator(source.bytes(), consumed, policy.decode.value_separator) < source.bytes().len() {
            return Ok(RangeLocateRun::NotSingleDocument);
        }
    }
    // THE CONTAINER DISPATCH. Only a resolved node — the codec's array arm — is
    // served here; `Missing` and `TypeMismatch` are the floor's.
    let (codec_outcome, _) = engine.into_parts();
    let CodecInputOutcome::Result(result) = codec_outcome else {
        return Ok(RangeLocateRun::NotApplicable);
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
    let mut reused_encoder = ReusableEncoderSession::new();
    encode_one(
        &factory,
        &mut reused_encoder,
        &result,
        0,
        encoding_policy,
        framing,
        resources,
        sink,
        &mut publication,
    )?;
    Ok(RangeLocateRun::Completed(PipelineReport {
        publication: PublicationStatus::Complete {
            items: 1,
            published_bytes: publication.published_bytes,
        },
        disposition: PipelineDisposition::Emitted,
        access: access_report,
    }))
}

/// The terminal outcome of driving one engine result stream to a stop.
pub(crate) enum StreamStop {
    /// The stream completed after publishing this many items in total.
    Complete(u64),
    /// The stream aborted with a per-value runtime mismatch after publishing a
    /// (possibly empty) prefix; `items` is the total published including it.
    Runtime { items: u64, error: RuntimeError },
    /// The stream aborted with a program-raised error VALUE (`error/0-1`) that no
    /// `try` caught, after publishing a (possibly empty) prefix; `items` is the
    /// total published including it, and `value` is the owned raised value.
    Raised { items: u64, value: Value },
    /// `halt`/`halt_error` terminated the run: the process exit status and the
    /// optional message value to print compact to stderr.
    Halt { status: u32, message: Option<Value> },
    /// The stream aborted on the ONE per-value CODEC failure the per-value
    /// class admits (`RawNulByte` under `--raw-output0`): the offending item
    /// was NOT published, and `items` counts the published prefix only. The
    /// caller decides like `Runtime`: report and continue (a sequence) or
    /// report and abort (a single-document drive).
    ValueFailure { items: u64, error: CodecError },
    /// The split-name program refused one item (non-string, empty, or a
    /// runtime failure). The item published nothing; a sequence reports and
    /// continues, a single-document drive aborts.
    SplitName { items: u64, index: u64, detail: String },
}

/// Reclaims the cached overlay box after one item's encode: the encoder only
/// reads the overlay (a `downcast_ref` borrow), so the box comes back
/// unchanged and is returned to the cache. Anything else in the slot — or an
/// empty slot — invalidates the cache and forces a rebuild on the next item.
fn reclaim_cached_overlay(
    resources: &mut ResourceContext<'_>,
    cached_overlay: &mut Option<(usize, Box<dyn core::any::Any>)>,
) {
    match resources.take_host_extension() {
        Some(boxed) => match boxed.downcast::<Vec<jqf_codec_core::comment::CommentEncodeOverlay>>() {
            Ok(overlay) => {
                if let Some((_, slot)) = cached_overlay {
                    *slot = overlay;
                }
            }
            Err(_) => *cached_overlay = None,
        },
        None => *cached_overlay = None,
    }
}

fn comment_encode_overlay(
    deltas: &[FactDelta],
) -> Result<Vec<jqf_codec_core::comment::CommentEncodeOverlay>, CodecError> {
    use jqf_codec_core::comment::{CommentEncodeOverlay, FOOT, HEAD, INLINE};
    let mut overlay = Vec::new();
    for delta in deltas {
        if !matches!(delta.role.as_str(), HEAD | INLINE | FOOT) {
            // A non-comment fact write (an attribute, or a YAML
            // style/tag/anchor/alias role) has no plain-run encode path: no
            // format overlays it here, so accepting the write and dropping
            // the delta would silently lose it. Refuse naming the gap; the
            // edit lane applies these roles as byte edits.
            let message = std::format!(
                "fact write to role \"{}\" cannot be encoded on this run — \
                 only comment facts encode outside the edit lane",
                delta.role
            );
            let base = CodecError::new(jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
            let Some(diagnostic) = jqf_source::Diagnostic::try_new(
                jqf_source::Namespace::new("pipeline").code("fact-encode"),
                jqf_source::Severity::Error,
                &message,
            ) else {
                return Err(base);
            };
            return Err(base.with_diagnostic(diagnostic));
        }
        let lines = match &delta.payload {
            Value::Null => None,
            Value::Array(items) if items.is_empty() => None,
            Value::Array(items) => {
                let mut lines = Vec::new();
                for item in items {
                    let Value::String(text) = item else {
                        // The payload vocabulary is codec-controlled; a
                        // non-string line inside a comment delta is a broken
                        // contract, never an empty comment.
                        return Err(CodecError::new(
                            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                                contract: "comment fact array payload holds a non-string line",
                            },
                        ));
                    };
                    lines.push(String::from(text.as_str()));
                }
                Some(lines)
            }
            Value::String(text) => Some(vec![String::from(text.as_str())]),
            _ => {
                return Err(CodecError::new(
                    jqf_codec_core::CodecFailureKind::InternalContractViolation {
                        contract: "comment fact delta payload is not null, string, or string array",
                    },
                ));
            }
        };
        overlay.push(CommentEncodeOverlay {
            node: delta.node,
            role: delta.role.clone(),
            lines,
        });
    }
    Ok(overlay)
}

/// Drives one engine result stream, encoding each ordered item through the
/// shared per-item publication path. A machine failure (control, ledger, or
/// internal contract) is a hard [`PipelineError`]; a reference-semantic index/iterate
/// mismatch is a [`StreamStop::Runtime`] the caller decides to abort on
/// ([`execute_value_document`]) or report and continue past (`execute_sequence`). Only the
/// ordered items come from the engine; cooperative resume and publication stay
/// SDK-owned.
#[allow(
    clippy::too_many_arguments,
    reason = "one stream keeps its encoder, counter, policy, framing, resources, sink, and publication explicit"
)]
pub(crate) fn drive_run_stream<Sink: ItemSink>(
    factory: &jqf_codec_core::ErasedEncoderFactory,
    reused_encoder: &mut ReusableEncoderSession,
    mut stream: EngineRunStream<'_, '_>,
    mut items: u64,
    policy: OrderedEncodingPolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<StreamStop, PipelineError<Sink::Error>> {
    // Cached across items: the run's delta list only GROWS during one stream
    // (fact writes append in order), so an unchanged delta length means
    // unchanged content. The overlay is built once and re-installed per item,
    // and rebuilt only when a new delta lands — not once per item.
    let mut cached_overlay: Option<(usize, Box<dyn core::any::Any>)> = None;
    loop {
        match stream.poll(resources) {
            Ok(RunPoll::Pending) => resume(resources, policy.cooperative_credits, publication)?,
            Ok(RunPoll::Item(result)) => {
                let delta_len = stream.fact_deltas().len();
                if cached_overlay
                    .as_ref()
                    .is_none_or(|(cached_len, _)| *cached_len != delta_len)
                {
                    match comment_encode_overlay(stream.fact_deltas()) {
                        Ok(overlay) => {
                            cached_overlay = if overlay.is_empty() {
                                None
                            } else {
                                Some((delta_len, Box::new(overlay) as Box<dyn core::any::Any>))
                            };
                        }
                        Err(error) => {
                            return Err(publication.fail(PipelineFailure::Codec(error)));
                        }
                    }
                }
                let previous = match &mut cached_overlay {
                    None => None,
                    Some((_, overlay)) => {
                        let previous = resources.take_host_extension();
                        let boxed = core::mem::replace(overlay, Box::new(()) as Box<dyn core::any::Any>);
                        resources.set_host_extension(boxed);
                        Some(previous)
                    }
                };
                let encoded = encode_one(
                    factory,
                    reused_encoder,
                    &result,
                    items,
                    policy,
                    framing,
                    resources,
                    sink,
                    publication,
                );
                // Reclaim the overlay box for the next item before restoring
                // whatever the caller had installed underneath it. Only a
                // run that installed one touches the slot at all: the host
                // extension is shared (it carries, for example, the input
                // cursor), so an empty cache must leave it alone.
                if previous.is_some() {
                    reclaim_cached_overlay(resources, &mut cached_overlay);
                }
                if let Some(Some(ext)) = previous {
                    resources.set_host_extension(ext);
                }
                match encoded {
                    Ok(()) => {}
                    // RawNulByte — through the shared admission test
                    // `is_per_value_codec_kind` — is the one per-value codec
                    // kind: jq skips the offending value (nothing published
                    // for it) and continues; the caller (sequence vs
                    // single-document) decides like `StreamStop::Runtime`.
                    Err(error) => {
                        if let PipelineFailure::Codec(codec) = error.failure()
                            && is_per_value_codec_kind(codec.kind())
                        {
                            // The item boundary was opened and never finished:
                            // the skipped value publishes nothing further, so
                            // close it here or every later report inherits a
                            // stale InProgress status.
                            publication.item_open = false;
                            return Ok(StreamStop::ValueFailure {
                                items,
                                error: codec.clone(),
                            });
                        }
                        if let PipelineFailure::SplitName { index, detail } = error.failure() {
                            return Ok(StreamStop::SplitName {
                                items,
                                index: *index,
                                detail: detail.clone(),
                            });
                        }
                        return Err(error);
                    }
                }
                items = items
                    .checked_add(1)
                    .ok_or_else(|| overflow::<Sink::Error>(publication))?;
            }
            Ok(RunPoll::Complete) => return Ok(StreamStop::Complete(items)),
            Err(error) => {
                return match split_run_error(error) {
                    RunError::Machine(error) => Err(publication.fail(PipelineFailure::Codec(error))),
                    // A program-raised error VALUE (`error/0-1`) that escaped
                    // every `try`: a distinct terminal disposition carrying the
                    // owned value the facade renders.
                    RunError::Raised(value) => Ok(StreamStop::Raised { items, value }),
                    // `halt`/`halt_error` terminate the whole run; the sequence
                    // must abort immediately (the reference's `jq_halt` is process level).
                    RunError::Halt { status, message } => Ok(StreamStop::Halt { status, message }),
                    RunError::Runtime(error) => Ok(StreamStop::Runtime { items, error }),
                };
            }
        }
    }
}

/// Reports one uncaught program-raised value to the sink (a best-effort clone, so
/// a report allocation failure never masks the raise) and builds the terminal
/// [`PipelineFailure::Raised`] carrying the owned value.
pub(crate) fn report_and_fail_raised<Sink: ItemSink>(
    sink: &mut Sink,
    publication: &mut Publication,
    value: Value,
    input_line: u64,
    filename: Option<&str>,
) -> PipelineError<Sink::Error> {
    let reported = value.clone();
    if let Ok(report) = SequenceValueError::try_for_raised(0, input_line, filename, reported)
        && let Err(error) = sink.report_value_error(report)
    {
        return publication.fail(PipelineFailure::Sink(error));
    }
    publication.fail(PipelineFailure::Raised(RaisedError { value }))
}

/// Reports one per-value CODEC failure (the single admitted kind,
/// `RawNulByte`) to the sink and builds the terminal failure carrying it. A
/// single-document route aborts on it exactly as on a runtime mismatch.
pub(crate) fn report_and_fail_codec<Sink: ItemSink>(
    sink: &mut Sink,
    publication: &mut Publication,
    error: CodecError,
    input_line: u64,
    filename: Option<&str>,
) -> PipelineError<Sink::Error> {
    if let Err(sink_error) = sink.report_value_error(SequenceValueError::try_for_codec(0, input_line, filename, &error))
    {
        return publication.fail(PipelineFailure::Sink(sink_error));
    }
    publication.fail(PipelineFailure::Codec(error))
}

/// Reports one typed runtime error to the sink and builds the terminal failure
/// carrying its exit class. A single-document route aborts on the first such
/// error (the reference prints the emitted prefix, reports, and exits nonzero), so the two
/// steps always happen together.
pub(crate) fn report_and_fail_runtime<Sink: ItemSink>(
    sink: &mut Sink,
    publication: &mut Publication,
    error: RuntimeError,
    input_line: u64,
    filename: Option<&str>,
) -> PipelineError<Sink::Error> {
    let mismatch = error.mismatch;
    if let Err(sink_error) = sink.report_value_error(error.into_sequence_error(0, input_line, filename)) {
        return publication.fail(PipelineFailure::Sink(sink_error));
    }
    publication.fail(mismatch.into_failure())
}

/// Plan 133 R1's count fast-path: a count-class program's value served by the
/// document-core consumer from the lazy document's span skeleton — no executor
/// run, no leaf materialization.
///
/// `Ok(None)` means the program is not a count row, the outcome is not a
/// document-backed result, or the consumer DECLINED (a shape it cannot prove);
/// the caller then runs the residual over the same outcome, which reproduces
/// the floor byte for byte. A document the consumer cannot navigate is the
/// same decline — the residual run surfaces the real error.
pub(crate) fn count_answer(
    outcome: &CodecInputOutcome<'_>,
    program: &CompiledProgram,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<Value>, CodecError> {
    // The mismatch dial beyond lenient takes the floor: the count consumer
    // answers missing keys as the reference's null (one output), but the
    // strict/warn dials must fire their mismatch cells on exactly those
    // reads — the floor's engine walk is the only evaluation that fires
    // them. The count fast-path is lenient-only, exactly as the
    // requirement lowering already is.
    if resources.mismatch_policy() != jqf_resource::policy::MismatchPolicy::Lenient {
        return Ok(None);
    }
    let Some(demand) = program.count_demand() else {
        return Ok(None);
    };
    let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
        return Ok(None);
    };
    match located.product().document().count_children_demand(demand, resources) {
        Ok(jqf_data::CountVerdict::Count(n)) => Ok(Some(count_value(n)?)),
        // A decline (a shape the skeleton cannot prove) and a document the
        // consumer cannot navigate are the same fall-through: the residual
        // run reproduces the floor.
        Ok(jqf_data::CountVerdict::Decline) | Err(_) => Ok(None),
    }
}

/// Plan 133 R6's element-iteration fast-path verdict.
pub(crate) enum ElementAnswer {
    /// A [`jqf_data::ElementRow::FanOut`] demand: every element's probe value
    /// was published through the sink (the consumer's visit-all-or-none
    /// contract guarantees a decline publishes NOTHING — the caller falls
    /// back to the floor with zero bytes on the sink).
    FanOut { items: u64 },
    /// A [`jqf_data::ElementRow::ReduceFold`] demand: the folded state, to be
    /// published as exactly one item by the caller.
    Fold(Value),
    /// Not an element program, a non-lenient dial, a non-document outcome, or
    /// the consumer declined; the caller runs the residual over the same
    /// outcome, which reproduces the floor byte for byte.
    None,
}

/// Plan 133 R6's element-iteration fast-path: a fan-out/fold program's values
/// served by the document-core consumer iterating the lazy document's span
/// skeleton — no executor run, no whole-tree materialization.
///
/// A [`jqf_data::ElementRow::FanOut`] demand publishes each element's probe
/// value through `sink` as it lands; a [`jqf_data::ElementRow::ReduceFold`]
/// demand folds the probe values into the caller-returned state (published
/// only on completion, so a mid-fold decline publishes nothing).
#[allow(
    clippy::too_many_arguments,
    reason = "one fast-path keeps its encoder, sink, and publication explicit"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one arm per element row: the answer IS the dispatch table over the demand ladder, and splitting it would hide which rows are covered"
)]
pub(crate) fn element_answer<Sink: ItemSink>(
    outcome: &CodecInputOutcome<'_>,
    program: &CompiledProgram,
    factory: &jqf_codec_core::ErasedEncoderFactory,
    reused_encoder: &mut ReusableEncoderSession,
    encoding_policy: OrderedEncodingPolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<ElementAnswer, PipelineError<Sink::Error>> {
    // The mismatch dial beyond lenient takes the floor, exactly as the count
    // fast-path: the element consumer answers missing keys as the reference's
    // null, but the strict/warn dials must fire their mismatch cells on
    // exactly those reads.
    if resources.mismatch_policy() != jqf_resource::policy::MismatchPolicy::Lenient {
        return Ok(ElementAnswer::None);
    }
    let Some(demand) = program.element_demand() else {
        return Ok(ElementAnswer::None);
    };
    let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
        return Ok(ElementAnswer::None);
    };
    match demand.row {
        jqf_data::ElementRow::FanOut => {
            if let Some(fields) = program.element_construct_fields() {
                return construct_fan_out(
                    located.product().document(),
                    demand,
                    fields,
                    factory,
                    reused_encoder,
                    encoding_policy,
                    framing,
                    resources,
                    sink,
                    publication,
                );
            }
            if program.element_collect() {
                let mut values: std::vec::Vec<Value> = std::vec::Vec::new();
                let mut visitor = |value: &Value, _visitor_resources: &mut ResourceContext<'_>| {
                    // Growth is fallible like every neighboring path: a
                    // refused reservation declines to the floor (nothing has
                    // been published) instead of aborting the process.
                    if values.try_reserve(1).is_err() {
                        return Err(DataError::InvalidDocument);
                    }
                    values.push(value.clone());
                    Ok(())
                };
                let verdict = located
                    .product()
                    .document()
                    .visit_elements(demand, resources, &mut visitor);
                match verdict {
                    Ok(jqf_data::ElementVerdict::Completed(_)) => {
                        let Ok(array) = jqf_data::Array::try_from_vec(values) else {
                            return Ok(ElementAnswer::None);
                        };
                        encode_one(
                            factory,
                            reused_encoder,
                            &EngineResult::owned(Value::Array(array)),
                            0,
                            encoding_policy,
                            framing,
                            resources,
                            sink,
                            publication,
                        )?;
                        Ok(ElementAnswer::FanOut { items: 1 })
                    }
                    Ok(jqf_data::ElementVerdict::Decline) | Err(_) => Ok(ElementAnswer::None),
                }
            } else {
                let mut items = 0u64;
                let mut publish_error: Option<PipelineError<Sink::Error>> = None;
                let mut visitor = |value: &Value, visitor_resources: &mut ResourceContext<'_>| {
                    if publish_error.is_some() {
                        return Err(DataError::InvalidDocument);
                    }
                    if let Err(error) = encode_one(
                        factory,
                        reused_encoder,
                        &EngineResult::owned(value.clone()),
                        items,
                        encoding_policy,
                        framing,
                        visitor_resources,
                        sink,
                        publication,
                    ) {
                        publish_error = Some(error);
                        return Err(DataError::InvalidDocument);
                    }
                    let Some(next) = items.checked_add(1) else {
                        publish_error = Some(overflow::<Sink::Error>(publication));
                        return Err(DataError::InvalidDocument);
                    };
                    items = next;
                    Ok(())
                };
                let verdict = located
                    .product()
                    .document()
                    .visit_elements(demand, resources, &mut visitor);
                // A publish failure outranks every consumer verdict; a decline or
                // a navigation failure falls through to the floor.
                if let Some(error) = publish_error {
                    return Err(error);
                }
                match verdict {
                    Ok(jqf_data::ElementVerdict::Completed(_)) => Ok(ElementAnswer::FanOut { items }),
                    // A decline or navigation failure with nothing on the
                    // sink falls through to the floor. With a prefix already
                    // published it cannot: the floor would republish those
                    // elements — the only honest answer is the hard stop.
                    Ok(jqf_data::ElementVerdict::Decline) | Err(_) if items == 0 => Ok(ElementAnswer::None),
                    Ok(_) | Err(_) => Err(publication.fail(PipelineFailure::Codec(CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "fan-out published a prefix then declined",
                        },
                    )))),
                }
            }
        }
        jqf_data::ElementRow::ReduceFold => {
            let Some(delta) = demand.increment else {
                return Ok(ElementAnswer::None);
            };
            // The fold's internal state: the histogram's distinct keys in
            // first-insertion order with their exact-integer counts (the
            // row's only legal state — the object-increment update can never
            // produce a float, decimal, or non-number value). The final
            // object is built once, on completion.
            let mut counts: std::vec::Vec<(std::string::String, i64)> = std::vec::Vec::new();
            let mut index: std::collections::HashMap<std::string::String, usize> = std::collections::HashMap::new();
            let mut visitor = |value: &Value, _visitor_resources: &mut ResourceContext<'_>| {
                // A non-string probe value is the reference's
                // "Cannot index object with …" raise; the floor renders it.
                let Value::String(key_text) = value.untagged() else {
                    return Err(DataError::InvalidDocument);
                };
                let key = key_text.as_str();
                if let Some(&at) = index.get(key) {
                    let Some((_, count)) = counts.get_mut(at) else {
                        return Err(DataError::InvalidDocument);
                    };
                    let Some(sum) = count.checked_add(delta) else {
                        return Err(DataError::InvalidDocument);
                    };
                    *count = sum;
                } else {
                    // Same fallible-growth law for the fold's accumulators:
                    // either reservation refused declines to the floor.
                    if counts.try_reserve(1).is_err() || index.try_reserve(1).is_err() {
                        return Err(DataError::InvalidDocument);
                    }
                    let owned = key.to_owned();
                    index.insert(owned.clone(), counts.len());
                    counts.push((owned, delta));
                }
                Ok(())
            };
            let verdict = located
                .product()
                .document()
                .visit_elements(demand, resources, &mut visitor);
            let Ok(jqf_data::ElementVerdict::Completed(_)) = verdict else {
                // A mid-fold decline or a navigation failure: nothing was
                // published; the floor serves the fold byte for byte.
                return Ok(ElementAnswer::None);
            };
            let Ok(mut builder) = ObjectBuilder::try_with_capacity(counts.len()) else {
                return Ok(ElementAnswer::None);
            };
            for (key, count) in counts {
                let Ok(key) = ObjectKey::try_from_str(&key) else {
                    return Ok(ElementAnswer::None);
                };
                if builder
                    .try_insert_or_replace(key, Value::Number(Number::integer(Integer::from_i64(count))))
                    .is_err()
                {
                    return Ok(ElementAnswer::None);
                }
            }
            match builder.try_finish() {
                Ok(object) => Ok(ElementAnswer::Fold(Value::Object(object))),
                Err(_) => Ok(ElementAnswer::None),
            }
        }
    }
}

/// `PATH[] | {static keys}` fan-out: visit whole elements, reconstruct each
/// published object from the static field set. A two-pass walk so a
/// mid-stream construct decline (a number element, a type-mismatched
/// member) publishes nothing — `FanOut`'s visit-all-or-none contract.
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors element_answer's explicit encoder/sink/publication ownership"
)]
/// The check pass's no-op element visitor: [`Document::visit_elements`] only
/// needs the walk's verdict — the probe itself does the proving — but its
/// visitor signature requires a fallible return.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the visitor signature Document::visit_elements requires returns Result; the probe carries the whole check"
)]
fn check_only(_value: &Value, _resources: &mut ResourceContext<'_>) -> Result<(), DataError> {
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "mirrors element_answer's explicit encoder/sink/publication ownership"
)]
fn construct_fan_out<Sink: ItemSink>(
    document: &jqf_data::Document<'_>,
    demand: &jqf_data::ElementDemand,
    fields: &[(std::string::String, Vec<jqf_data::CountStep>)],
    factory: &jqf_codec_core::ErasedEncoderFactory,
    reused_encoder: &mut ReusableEncoderSession,
    encoding_policy: OrderedEncodingPolicy<'_>,
    framing: FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    publication: &mut Publication,
) -> Result<ElementAnswer, PipelineError<Sink::Error>> {
    // Check pass: probe EVERY field's FULL static path before a byte is
    // published — the construct walk reads all of them per element, so a
    // path that fails to resolve on any in-range element (a number element,
    // a type-mismatched member) must decline here, while nothing is on the
    // sink yet and the floor rerun stays duplicate-free. A Path probe
    // navigates spans without materializing them: cheap.
    if fields.is_empty() {
        return Ok(ElementAnswer::None);
    }
    let mut check = demand.clone();
    for (_, path) in fields {
        check.probe = jqf_data::ElementProbe::Path(path.clone());
        match document.visit_elements(&check, resources, &mut check_only) {
            Ok(jqf_data::ElementVerdict::Completed(_)) => {}
            Ok(jqf_data::ElementVerdict::Decline) | Err(_) => return Ok(ElementAnswer::None),
        }
    }

    let mut items = 0u64;
    let mut publish_error: Option<PipelineError<Sink::Error>> = None;
    let mut visitor = |value: &Value, visitor_resources: &mut ResourceContext<'_>| {
        if publish_error.is_some() {
            return Err(DataError::InvalidDocument);
        }
        let Some(constructed) = construct_static_object(value, fields) else {
            return Err(DataError::InvalidDocument);
        };
        if let Err(error) = encode_one(
            factory,
            reused_encoder,
            &EngineResult::owned(constructed),
            items,
            encoding_policy,
            framing,
            visitor_resources,
            sink,
            publication,
        ) {
            publish_error = Some(error);
            return Err(DataError::InvalidDocument);
        }
        let Some(next) = items.checked_add(1) else {
            publish_error = Some(overflow::<Sink::Error>(publication));
            return Err(DataError::InvalidDocument);
        };
        items = next;
        Ok(())
    };
    let verdict = document.visit_elements(demand, resources, &mut visitor);
    if let Some(error) = publish_error {
        return Err(error);
    }
    match verdict {
        Ok(jqf_data::ElementVerdict::Completed(_)) => Ok(ElementAnswer::FanOut { items }),
        // Unreachable after a completing check pass (every field's full path
        // was proven over every element), but a decline or navigation error
        // with bytes already on the sink must abort hard, never fall through
        // to a floor rerun that would republish the prefix.
        Ok(jqf_data::ElementVerdict::Decline) | Err(_) if items == 0 => Ok(ElementAnswer::None),
        Ok(_) | Err(_) => Err(publication.fail(PipelineFailure::Codec(CodecError::new(
            jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "fan-out published a prefix then declined",
            },
        )))),
    }
}

fn construct_static_object(
    element: &Value,
    fields: &[(std::string::String, Vec<jqf_data::CountStep>)],
) -> Option<Value> {
    let mut builder = ObjectBuilder::try_with_capacity(fields.len()).ok()?;
    for (key, path) in fields {
        let probe = jqf_data::ElementProbe::Path(path.clone());
        let value = jqf_data::owned_probe_value(element, &probe)?;
        let key = ObjectKey::try_from_str(key).ok()?;
        builder.try_insert_or_replace(key, value).ok()?;
    }
    builder.try_finish().ok().map(Value::Object)
}

pub(crate) fn count_value(count: u64) -> Result<Value, CodecError> {
    let integer = i64::try_from(count)
        .map(Integer::from_i64)
        .map_err(|_| CodecError::new(jqf_codec_core::CodecFailureKind::Overflow))?;
    Ok(Value::Number(Number::integer(integer)))
}

pub(crate) const fn disposition_of(input: RunInput) -> PipelineDisposition {
    match input {
        RunInput::Resolved => PipelineDisposition::Emitted,
        RunInput::Missing => PipelineDisposition::Missing,
    }
}

#[cfg(test)]
mod tests {
    use super::super::Value;
    use super::comment_encode_overlay;
    use jqf_engine::FactDelta;

    fn delta(role: &str) -> FactDelta {
        FactDelta {
            path: Value::Null,
            node: jqf_data::NodeId::try_from_index(0).unwrap(),
            role: String::from(role),
            kind: String::new(),
            payload: Value::Null,
        }
    }

    #[test]
    fn non_comment_fact_write_is_refused_not_dropped() {
        // A style/tag/anchor/alias (or attribute) delta has no plain-run
        // encode overlay: the run must refuse, never accept-and-drop.
        for role in ["tag", "style", "anchor", "alias", "attribute"] {
            let error = comment_encode_overlay(&[delta(role)]).expect_err(role);
            assert_eq!(
                error.kind(),
                jqf_codec_core::CodecFailureKind::UnsupportedRepresentation,
                "role {role}"
            );
        }
    }

    #[test]
    fn comment_roles_still_overlay() {
        for role in [
            jqf_codec_core::comment::HEAD,
            jqf_codec_core::comment::INLINE,
            jqf_codec_core::comment::FOOT,
        ] {
            let overlay = comment_encode_overlay(&[delta(role)]).expect(role);
            assert_eq!(overlay.len(), 1);
            assert_eq!(overlay[0].role, role);
        }
    }
}
