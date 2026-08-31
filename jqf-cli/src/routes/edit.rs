//! The single-run input-model routes: `--edit`, `--diff`, `-n`, and `-s`.
//!
//! These are the explicit input-model arms of the ladder: each runs the whole input as its subject (or the document,
//! for `--edit`) and reports [`RouteOutcome:Served`] with its own epilogue, except `--edit`, which declines when the
//! lane cannot serve the program so the demand-projection rungs get the request byte-for-byte.

use std::io::Write as IoWrite;

use jqf_codec_core::{DecodeRequest, ItemByteOwner, RouteCapability};
use jqf_engine::{CompileOptions, try_compile_program};
use jqf_sdk::{FacadeFraming, Input, Outcome, PipelinePolicy, Report};

use crate::args::CliInputSelection;
use crate::errors::{CliFailure, compile_failure, requirement_failure};
use crate::input::{parse_diff_document, trailing_bytes};
use crate::output::{EditBufferSink, write_output_bytes};
use crate::plan::{RouteContext, RouteOutcome};
use crate::{eprint_line_buffered, record_and_render_failure, record_route};

/// The edit lane (`--edit`): the whole document is the output subject. The SDK buffers the run (the exactly-one-output
/// law errors a document whose run publishes zero or many results, so a failing edit publishes nothing), then the CLI
/// writes the bytes to the chosen destination. A decline — programs whose shapes the lane cannot serve — falls through
/// to the ordinary ladder byte-for-byte.
pub(crate) fn edit(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut edit_sink = EditBufferSink { bytes: &mut buffer };
    // Who owns the byte after the edit document is the codec's own declaration now (item G): the same `item_byte_owner`
    // lookup the output lane reads, keyed on the INPUT (format, dialect) -- edit is a same-format lane, and the edit
    // document's trailing byte is a property of the input dialect's document encoding. The formats whose edit documents
    // RETAIN their own trailing bytes in the source segment -- the file's own newline, the `---` that opens a later
    // YAML document -- declare `Codec` (the facade would double-terminate or splice a separator into the file's bytes,
    // and the in-place law demands the file's own trailing bytes survive verbatim). This replaces the hand-written
    // 11-arm matches! that used to live here, declared per format instead of per dialect.
    let input_format =
        jqf_data::FormatId::try_new(ctx.input_selection.format.id()).map_err(|_| CliFailure::Message {
            class: crate::errors::ExitClass::Usage,
            message: format!("invalid built-in format identity: {}", ctx.input_selection.format.id()),
        })?;
    let input_dialect =
        jqf_data::DialectId::try_new(ctx.input_selection.dialect.id()).map_err(|_| CliFailure::Message {
            class: crate::errors::ExitClass::Usage,
            message: format!(
                "invalid built-in dialect identity: {}",
                ctx.input_selection.dialect.id()
            ),
        })?;
    let framing = match ctx
        .catalog
        .item_byte_owner(&input_format, &input_dialect)
        .map_err(|error| CliFailure::Message {
            class: crate::errors::ExitClass::Usage,
            message: format!(
                "cannot resolve inter-item byte owner for {} {}: {error:?}",
                ctx.input_selection.format.id(),
                ctx.input_selection.dialect.id()
            ),
        })? {
        ItemByteOwner::Codec => FacadeFraming::item_suffix(b""),
        ItemByteOwner::Facade => FacadeFraming::item_suffix(b"\n"),
    };
    let request = crate::routes::base_request(ctx, Input::Whole(ctx.input))
        .with_resources(ctx.resources)
        .with_framing(framing)
        .editing();
    match jqf_sdk::execute(request, &mut edit_sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?
    {
        Outcome::Served(Report::Sequence(report)) => {
            if ctx.diagnostics {
                eprint_line_buffered(&format!("jqf: edit: completed {report:?}"));
            }
            record_route(ctx.diagnostics_buffer, "edit");
            // The alias escape hatch engaged (/ lane C3): the run descended into an alias-referenced node under
            // `--edit-expand-alias`, so the shared anchor's authored span was rewritten and EVERY alias site changed
            // with it. The flag is an acceptance of exactly that semantics, never a correctness fix — the user is
            // warned once, per request.
            if ctx.resources.edit_alias_expansion_engaged() {
                eprint_line_buffered(
                    "jqf: warning: --edit-expand-alias: edited through an alias; \
                     the shared anchor was rewritten and every alias site changed",
                );
            }
            // The source's original trailing bytes replace the facade's final newline on every destination, so identity
            // `--edit` is byte-identical to the input and `--check` agrees with `--in-place`.
            let mut bytes = buffer;
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            bytes.extend_from_slice(trailing_bytes(ctx.input));
            if ctx.edit_check {
                // `--edit --check` is the gofmt -l verdict: exit 1 iff the edit WOULD change the file, exit 0 if the
                // would-be output is byte-identical, print NOTHING and write NOTHING in either case. The comparison is
                // against the bytes a write would produce. The exit-1 carrier is a message-less `Halt` (main prints
                // nothing for it and exits with the code); `Served` carries the unchanged exit 0.
                if bytes == ctx.input {
                    return Ok(RouteOutcome::Served);
                }
                return Err(CliFailure::Halt {
                    status: 1,
                    message: None,
                });
            }
            write_output_bytes(&bytes, ctx.in_place.or(ctx.output_path), ctx.no_atomic)?;
            Ok(RouteOutcome::Served)
        }
        Outcome::Served(other) => Err(CliFailure::from(format!(
            "the edit drive published a non-sequence report: {other:?}"
        ))),
        Outcome::Declined => {
            if ctx.diagnostics {
                eprint_line_buffered("jqf: edit: declined");
            }
            Ok(RouteOutcome::Declined)
        }
    }
}

/// `--diff OLD NEW`: read the two files as exactly ONE document each (each in its per-side format) and run the fixed
/// diff program over the pair. The run is a single null-first sequence drive over the codec catalog, exactly like `-n`;
/// only the file DECODE is per-side.
pub(crate) fn diff(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    let (old_path, new_path) = ctx
        .diff_pair
        .expect("the diff route is only resolved for a --diff request");
    // The side dialects live in THIS scope so the policies can borrow them: `DecodeRequest:dialect` (123 X5) is a
    // reference the decoder factories dispatch on, and a cross-format diff's sides need their own formats' dialects —
    // the request's (the old format's) would send the new side to the wrong factory dispatch.
    let old_side_dialect = resolve_side_dialect(ctx.diff_old_selection)?;
    let new_side_dialect = resolve_side_dialect(ctx.diff_new_selection)?;
    let old_policy = diff_side_policy(ctx, ctx.diff_old_selection, &old_side_dialect)?;
    let new_policy = diff_side_policy(ctx, ctx.diff_new_selection, &new_side_dialect)?;
    let old = parse_diff_document(old_path, ctx.diff_old_selection, ctx.catalog, old_policy, ctx.resources)?;
    let new = parse_diff_document(new_path, ctx.diff_new_selection, ctx.catalog, new_policy, ctx.resources)?;
    let diff_bindings = vec![(String::from("$__old"), old), (String::from("$__new"), new)];
    let compiled = try_compile_program(
        "diff($__old; $__new)",
        ctx.compile_policy,
        CompileOptions {
            cli_vars: &diff_bindings,
            split_exp: false,
            ..Default::default()
        },
        ctx.resources,
    )
    .map_err(|error| compile_failure(&error, "diff($__old; $__new)"))?;
    let requirement = compiled
        .try_requirement(ctx.resources)
        .map_err(|error| requirement_failure(&error))
        .map(|requirement| crate::routes::with_decode_fact_intent(requirement, ctx, false))?;
    // The stdin drive sees an EMPTY source for `--diff` (the route reads its own two files; the eager input is never
    // read) — the drive exists only to attach the input-family cursor the fixed program never pulls. It must still
    // CREATE the input codec's provider, and the CSV payload provider refuses the adjacent-value policy (a record
    // payload is exactly one text), so the null-first drive runs under a single-document policy: with an empty source
    // both arms decode zero documents and the run is the program over null, exactly as before.
    let null_first_policy = jqf_sdk::PipelinePolicy {
        decode: jqf_codec_core::DecodeRequest {
            allow_adjacent_values: false,
            ..ctx.policy.decode
        },
        ..ctx.policy
    };
    let request = crate::routes::base_request(ctx, Input::Whole(ctx.input))
        .with_resources(ctx.resources)
        .with_program(&compiled)
        .with_policy(null_first_policy)
        .with_requirement(&requirement)
        .with_null_input();
    jqf_sdk::execute(request, ctx.sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
    // The explain-block contract: the block names the route that served the request, and this route is the fixed diff
    // program, not the ordinary sequence rung.
    record_route(ctx.diagnostics_buffer, "diff");

    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    Ok(RouteOutcome::Completed)
}

/// The per-side `--diff` decode policy: the request's pipeline policy with `allow_adjacent_values` keyed on the SIDE's
/// own format's adjacency — never the request's — because a cross-format diff (`--old-format toml`) decodes each file
/// with its own drive shape.
fn diff_side_policy<'ctx>(
    ctx: &RouteContext<'ctx, '_>,
    selection: CliInputSelection,
    dialect: &'ctx jqf_data::DialectId,
) -> Result<PipelinePolicy<'ctx>, CliFailure> {
    let format = jqf_data::FormatId::try_new(selection.format.id()).map_err(|_| CliFailure::Message {
        class: crate::errors::ExitClass::Usage,
        message: format!("invalid built-in format identity: {}", selection.format.id()),
    })?;
    let adjacent = ctx
        .catalog
        .route_capabilities(&format, dialect)
        .map_err(|error| CliFailure::from(format!("cannot resolve route capabilities: {error:?}")))?
        .contains(&RouteCapability::AdjacentValues);
    Ok(PipelinePolicy {
        decode: DecodeRequest {
            allow_adjacent_values: adjacent,
            // The side's OWN dialect, never the request's: the decoder factories dispatch on `DecodeRequest:dialect`
            // (123 X5), and a cross-format diff's new side must decode as its own format — the spread would inherit the
            // request's dialect (the old format's) and the yaml factory dispatched on the toml dialect finds no route.
            dialect,
            ..ctx.policy.decode
        },
        ..ctx.policy
    })
}

/// Resolves one `--diff` side's registered dialect id, owned so the side policy can borrow it (see the call site in
/// [`diff`]).
fn resolve_side_dialect(selection: CliInputSelection) -> Result<jqf_data::DialectId, CliFailure> {
    jqf_data::DialectId::try_new(selection.dialect.id()).map_err(|_| CliFailure::Message {
        class: crate::errors::ExitClass::Usage,
        message: format!("invalid built-in dialect identity: {}", selection.dialect.id()),
    })
}

/// the adopted `-n`: the filter runs once over `null` (with the input family served from the eager cursor). The whole
/// input is the subject, so the demand-projection rungs — each of which names a `.[]` boundary or a single located path
/// — decline by construction.
pub(crate) fn null_first(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    // The streamed `-n` input-family lane : the source was never read whole — the route pulls its bytes on demand
    // through the engine's streaming cursor, so a fold's retention is O(window + accumulator) instead of one
    // whole-input copy. The main driver computed the shape fact (one strict-NDJSON or adjacent-JSON source, no
    // `-R`/`--stream` rewrite) and skipped the read; this arm is the only server for it.
    if ctx.streamed_null_first {
        let tally = crate::routes::stream::ReadTally::default();
        let read = crate::routes::stream::tallied(
            &tally,
            crate::routes::serve_null_input_source(ctx.streamed_null_first_file),
        );
        let request = crate::routes::base_request(ctx, Input::Streaming(Box::new(read)))
            .with_resources(ctx.resources)
            .with_null_input();
        let outcome = jqf_sdk::execute(request, ctx.sink);
        // This lane reads the source itself, so it charges what it actually pulled — the whole-read lanes charge where
        // they read. The charge happens even when the drive failed: the ledger saw every read.
        let charged = crate::routes::stream::charge_input(ctx.resources, tally.get());
        outcome
            .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))
            .and(charged)?;
        record_route(ctx.diagnostics_buffer, "sequence");

        ctx.sink
            .output
            .flush()
            .map_err(|error| format!("cannot flush stdout: {error}"))?;
        if ctx.diagnostics {
            eprint_line_buffered(&format!(
                "jqf: precision_boundary_events={}",
                ctx.resources.precision_boundary_events()
            ));
        }
        return Ok(RouteOutcome::Served);
    }
    // Whole-file JSON input-family programs parse values ON DEMAND through the streaming cursor instead of the eager
    // cursor: the eager cursor holds one owned Value per record, which on a large NDJSON file is O(records) of
    // allocator commit (~2.6 KB/record on the 148 MB corpus fixture). The pipe path already serves this shape through
    // `execute_null_first_sequence_streaming`; a retained file pays one copy of its bytes and then reads the same way.
    if crate::routes::json_input_family_streamable(ctx) {
        let owned = ctx.input.to_vec();
        let request = crate::routes::base_request(
            ctx,
            Input::Streaming(Box::new(crate::routes::serve_whole_buffer(owned))),
        )
        .with_resources(ctx.resources)
        .with_null_input();
        jqf_sdk::execute(request, ctx.sink)
            .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
        record_route(ctx.diagnostics_buffer, "sequence");

        ctx.sink
            .output
            .flush()
            .map_err(|error| format!("cannot flush stdout: {error}"))?;
        if ctx.diagnostics {
            eprint_line_buffered(&format!(
                "jqf: precision_boundary_events={}",
                ctx.resources.precision_boundary_events()
            ));
        }
        return Ok(RouteOutcome::Served);
    }
    // The eager cursor materializes every input value, so each per-value decode must produce a COMPLETE document:
    // lowering the program's own requirement bound a scoped route per value whose located outcome the drive cannot
    // interpret (on the merge base: `jqf -n 'input|.[0]'` and `jqf -s '.[0]'` both failed with "input-sequence decode
    // produced a non-result outcome", exit 5).
    let requirement = ctx
        .compiled
        .try_whole_document_requirement(ctx.resources)
        .map_err(|error| requirement_failure(&error))
        .map(|requirement| crate::routes::with_decode_fact_intent(requirement, ctx, false))?;
    let request = crate::routes::base_request(ctx, Input::Whole(ctx.input))
        .with_resources(ctx.resources)
        .with_requirement(&requirement)
        .with_null_input();
    jqf_sdk::execute(request, ctx.sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
    record_route(ctx.diagnostics_buffer, "sequence");

    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    Ok(RouteOutcome::Completed)
}

/// the adopted `-s`: the filter runs once over the array of every decoded input.
pub(crate) fn slurped(ctx: &mut RouteContext<'_, '_>) -> Result<RouteOutcome, CliFailure> {
    // A slurped `map(F) | add` / `map(F) | length` over a whole JSON file streams the fold through the input family
    // instead of materializing every record into the slurp array. jq's `map` is `[.[] | F]`, so the streamed equivalent
    // `reduce inputs as $x (INIT;. + ([($x | F)] | AGG))` is byte-identical and keeps the allocator high-water at the
    // current record (~310-334 MiB vs ~1.3 GiB on the 148 MB NDJSON fixture). The rewrite is the ENGINE's arena row:
    // the compiled program recognizes the shape on its own arena and returns the fold, so the CLI never parses or
    // rewrites the user's source text.
    if crate::routes::json_streamable_source(ctx)
        && !ctx.single_document_input
        && ctx.input.len() >= 16 * 1024 * 1024
        && let Some(compiled) = ctx.compiled.streaming_slurp_aggregate()
    {
        let owned = ctx.input.to_vec();
        let request = crate::routes::base_request(
            ctx,
            Input::Streaming(Box::new(crate::routes::serve_whole_buffer(owned))),
        )
        .with_resources(ctx.resources)
        .with_program(&compiled)
        .with_null_input();
        jqf_sdk::execute(request, ctx.sink)
            .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
        record_route(ctx.diagnostics_buffer, "sequence");

        ctx.sink
            .output
            .flush()
            .map_err(|error| format!("cannot flush stdout: {error}"))?;
        if ctx.diagnostics {
            eprint_line_buffered(&format!(
                "jqf: precision_boundary_events={}",
                ctx.resources.precision_boundary_events()
            ));
        }
        return Ok(RouteOutcome::Served);
    }
    // The whole-document law of the eager drives; see `null_first`.
    let requirement = ctx
        .compiled
        .try_whole_document_requirement(ctx.resources)
        .map_err(|error| requirement_failure(&error))
        .map(|requirement| crate::routes::with_decode_fact_intent(requirement, ctx, false))?;
    let request = crate::routes::base_request(ctx, Input::Whole(ctx.input))
        .with_resources(ctx.resources)
        .with_requirement(&requirement)
        .slurped();
    jqf_sdk::execute(request, ctx.sink)
        .map_err(|error| record_and_render_failure(ctx.diagnostics, ctx.diagnostics_buffer, &error))?;
    record_route(ctx.diagnostics_buffer, "sequence");

    ctx.sink
        .output
        .flush()
        .map_err(|error| format!("cannot flush stdout: {error}"))?;
    if ctx.diagnostics {
        eprint_line_buffered(&format!(
            "jqf: precision_boundary_events={}",
            ctx.resources.precision_boundary_events()
        ));
    }
    Ok(RouteOutcome::Served)
}
