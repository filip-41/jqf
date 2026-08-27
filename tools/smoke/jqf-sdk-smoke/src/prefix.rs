//! Prefix-pushdown, fusion, and pipeline-fault receipts.
//!
//! The prefix family proves a scoped prefix publishes the same requirement,
//! SCOPED route, and bytes as the bare prefix. Pipeline-fault receipts
//! (ordered-many, adversarial sinks, output limit, cancellation) pin the
//! drive's failure accounting. Projection/explain live in [`crate::projection`].

use crate::harness::{
    CONTROL, FaultMode, FaultSink, ManyProducer, PartialSink, ToggleControl, execute_root, json_dialect, program_for,
    resources, resources_with, run,
};
use jqf_codec_core::{
    AccessAdapter, AccessResultKind, DecodeRequest, DiagnosticPolicy, PreservationOutcome, PreservationRequest,
    ValidationMode,
};
use jqf_data::{DiagnosticCoverage, DialectId, FormatId, Value};
use jqf_engine::{
    CodecRequirementPolicy, StaticForwardStep, try_lower_forward_requirement, try_lower_root_requirement,
};
use jqf_sdk::{
    CodecCatalog, FacadeFraming, OrderedEncodingPolicy, PipelineDisposition, PipelinePolicy, PublicationStatus,
    encode_ordered,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// One maximal-prefix row: `source` must push down to `bare` and publish `expected`.
struct PrefixRow {
    source: &'static str,
    bare: &'static str,
    input: &'static [u8],
    expected: &'static [u8],
    label: &'static str,
}

const PREFIX_ROWS: &[PrefixRow] = &[
    PrefixRow {
        source: ".a | (.b + .c)",
        bare: ".a",
        input: br#"{"a":{"b":2,"c":3}}"#,
        expected: b"5\n",
        label: "arith-prefix",
    },
    PrefixRow {
        source: ".a | if . then 1 else 2 end",
        bare: ".a",
        input: br#"{"a":5}"#,
        expected: b"1\n",
        label: "conditional-prefix",
    },
    PrefixRow {
        source: ".a | try .b catch 0",
        bare: ".a",
        input: br#"{"a":5}"#,
        expected: b"0\n",
        label: "try-prefix",
    },
    PrefixRow {
        source: ".catalog | reduce .[] as $x (0; . + 1)",
        bare: ".catalog",
        input: br#"{"catalog":[10,20,30,40,50]}"#,
        expected: b"5\n",
        label: "reduce-prefix",
    },
    PrefixRow {
        source: ".a | (.b as $v | $v + 1)",
        bare: ".a",
        input: br#"{"a":{"b":5}}"#,
        expected: b"6\n",
        label: "bind-prefix",
    },
    PrefixRow {
        source: ".catalog[2] | [..] | length",
        bare: ".catalog[2]",
        input: br#"{"catalog":[{"id":0},{"id":1},{"id":2,"tags":["a","b","c"]}]}"#,
        expected: b"6\n",
        label: "descent-prefix",
    },
    PrefixRow {
        source: ".catalog[2].tags[1:3]",
        bare: ".catalog[2].tags",
        input: br#"{"catalog":[{"id":0},{"id":1},{"id":2,"tags":["a","b","c"]}]}"#,
        expected: b"[\"b\",\"c\"]\n",
        label: "slice-prefix",
    },
    PrefixRow {
        source: ".items | map(.id)",
        bare: ".items",
        input: br#"{"items":[{"id":1},{"id":2}]}"#,
        expected: b"[1,2]\n",
        label: "call-prefix",
    },
];

/// Shared maximal-prefix family: each row's `source` lowers the same requirement
/// as `bare` and publishes through the SCOPED route. Bind-source (two positives
/// plus declining halves) is [`assert_bind_source_prefix_route`].
pub(crate) fn assert_prefix_route_family(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    for row in PREFIX_ROWS {
        assert_prefix_route_publishes(
            catalog,
            format,
            dialect,
            row.source,
            row.bare,
            row.input,
            row.expected,
            row.label,
        )?;
    }
    Ok(())
}

/// Bind-SOURCE prefix receipt: a static path at the head
/// of a binder or loop SOURCE now pushes down, so `.catalog[1].id as $i | [$i,
/// $i*2]` lowers the SAME requirement as bare `.catalog[1].id` and fires the
/// scoped route — it used to read the whole document because the only static
/// path in the program lived inside the `Bind` source, where the pushdown spine
/// stopped dead.
///
/// The law has two halves and both are asserted here. The source may push down
/// only when every OTHER graph reading the outer dot is document-independent
/// (a binder's body, a loop's init), and only when the codec resolves the source
/// COMPLETELY — a source with a residual `.[]` after its prefix would fan out
/// over the located container, which for a container that is most of the
/// document loses to a single-pass whole parse. So `reduce .catalog[].id as $i
/// (0; . + $i)` keeps the whole-document route, and `.a as $x | .b` (whose body
/// reads the outer dot) keeps it too.
pub(crate) fn assert_bind_source_prefix_route(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    const INPUT: &[u8] = br#"{"catalog":[{"id":0},{"id":1},{"id":2}],"meta":{"n":3},"other":"x"}"#;
    assert_prefix_route_publishes(
        catalog,
        format,
        dialect,
        ".catalog[1].id as $i | [$i, $i*2]",
        ".catalog[1].id",
        INPUT,
        b"[1,2]\n",
        "bind-source-prefix",
    )?;
    // A loop source is the same law: `init` reads the outer dot, so a literal
    // init lets the source's static path push down.
    assert_prefix_route_publishes(
        catalog,
        format,
        dialect,
        "reduce .meta.n as $n (0; . + $n)",
        ".meta.n",
        INPUT,
        b"3\n",
        "loop-source-prefix",
    )?;
    // The declining halves keep the whole-document route exactly as before.
    for (source, why) in [
        // The body reads the outer dot, which the located value is not.
        (".catalog[1].id as $i | .meta", "body reads the outer dot"),
        // The init reads the outer dot.
        ("reduce .meta.n as $n (.meta.n; . + $n)", "init reads the outer dot"),
        // The source fans out over the located container.
        (
            "reduce .catalog[].id as $i (0; . + $i)",
            "source fans out over the container",
        ),
    ] {
        let declined_resources = resources();
        let program = program_for(source, &declined_resources)?;
        let requirement = program
            .try_requirement(&declined_resources)
            .map_err(|error| format!("declined requirement {source:?}: {:?}", error.kind()))?;
        if !requirement.footprint().is_whole() || requirement.result() != AccessResultKind::CompleteDocument {
            return Err(format!(
                "{source:?} must keep the whole-document route ({why}), got {:?}",
                requirement.result()
            ));
        }
        let mut declined_sink = PartialSink {
            bytes: Vec::new(),
            boundaries: Vec::new(),
            reports: Vec::new(),
        };
        let mut run_resources = resources();
        let report = run(
            catalog,
            INPUT,
            &requirement,
            &program,
            format,
            dialect,
            &mut run_resources,
            &mut declined_sink,
        )?;
        if report.access_route().route() != jqf_codec_json::FULL_PHYSICAL_ROUTE_ID {
            return Err(format!(
                "{source:?} must keep the whole-document route ({why}), fired {:?}",
                report.access_route().route()
            ));
        }
    }
    Ok(())
}

/// The shared maximal-prefix pushdown receipt: `source`'s requirement must be
/// structurally identical to `bare`'s, and running `source` over `input` must
/// publish `expected` through the codec's SCOPED physical route.
#[allow(
    clippy::too_many_arguments,
    reason = "one receipt shape shared by the prefix-pushdown receipts; the parameters are the receipt's own fields"
)]
fn assert_prefix_route_publishes(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    source: &str,
    bare: &str,
    input: &[u8],
    expected: &[u8],
    label: &str,
) -> Result<(), String> {
    let mut scoped_resources = resources();
    let program = program_for(source, &scoped_resources)?;
    let requirement = program
        .try_requirement(&scoped_resources)
        .map_err(|error| format!("{label} requirement: {:?}", error.kind()))?;

    let bare_resources = resources();
    let bare_program = program_for(bare, &bare_resources)?;
    let bare_requirement = bare_program
        .try_requirement(&bare_resources)
        .map_err(|error| format!("bare requirement: {:?}", error.kind()))?;

    if requirement.footprint() != bare_requirement.footprint()
        || requirement.footprint().fingerprint() != bare_requirement.footprint().fingerprint()
        || requirement.result() != bare_requirement.result()
    {
        return Err(format!(
            "{label} pushdown mismatch: scoped={requirement:?} bare={bare_requirement:?}"
        ));
    }

    let mut sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let report = run(
        catalog,
        input,
        &requirement,
        &program,
        format,
        dialect,
        &mut scoped_resources,
        &mut sink,
    )?;
    if sink.bytes != expected
        || report.access_route().route() != jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID
        || report.access_route().slot().get() != 1
    {
        return Err(format!(
            "{label} receipt mismatch: bytes={:?} route={:?} slot={}",
            sink.bytes,
            report.access_route().route(),
            report.access_route().slot().get()
        ));
    }
    Ok(())
}

/// Constructor-shape receipts:
///
/// 1. Prefix pushdown into a constructor body. `.a | {x: .b}` pushes the static
///    prefix `.a` down — same footprint/fingerprint/result as bare `.a`, so the
///    scoped route still fires — and the residual object constructor runs over
///    the located `.a`, publishing one owned object `{"x":5}`.
/// 2. Collect-of-fan-out: `[.a[].b]` publishes the same bytes and the same
///    physical route as `[.a[] | .b]`.
pub(crate) fn assert_constructor_shapes(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    assert_prefix_route_publishes(
        catalog,
        format,
        dialect,
        ".a | {x: .b}",
        ".a",
        br#"{"a":{"b":5}}"#,
        b"{\"x\":5}\n",
        "constructor-body",
    )?;

    // Collect-of-fan-out: same bytes and the same physical route.
    let fan: &[u8] = br#"{"a":[{"b":1},{"b":2}]}"#;
    let (collect_bytes, collect_route) = run_published(catalog, fan, "[.a[].b]", format, dialect)?;
    let (piped_bytes, piped_route) = run_published(catalog, fan, "[.a[] | .b]", format, dialect)?;
    if collect_bytes != b"[1,2]\n" || collect_bytes != piped_bytes || collect_route != piped_route {
        return Err(format!(
            "collect-of-fan-out equivalence mismatch: collect={collect_bytes:?}@{collect_route:?} piped={piped_bytes:?}@{piped_route:?}"
        ));
    }
    Ok(())
}

/// `map(f)` ≡ `[.[] | f]` (the Lowering IS the plan): `map(.id)` and its
/// expansion `[.[] | .id]` produce the identical requirement (footprint,
/// fingerprint, result authority) and publish byte-identical output over the
/// same physical route — proof the lowering rewrites to the exact same graph.
pub(crate) fn assert_map_lowering_equivalence(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    let mut map_resources = resources();
    let map_program = program_for("map(.id)", &map_resources)?;
    let map_requirement = map_program
        .try_requirement(&map_resources)
        .map_err(|error| format!("map requirement: {:?}", error.kind()))?;

    let mut expanded_resources = resources();
    let expanded_program = program_for("[.[] | .id]", &expanded_resources)?;
    let expanded_requirement = expanded_program
        .try_requirement(&expanded_resources)
        .map_err(|error| format!("expansion requirement: {:?}", error.kind()))?;

    if map_requirement.footprint() != expanded_requirement.footprint()
        || map_requirement.footprint().fingerprint() != expanded_requirement.footprint().fingerprint()
        || map_requirement.result() != expanded_requirement.result()
    {
        return Err(format!(
            "map-lowering requirement mismatch: map={map_requirement:?} expanded={expanded_requirement:?}"
        ));
    }

    let mut map_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let map_report = run(
        catalog,
        br#"[{"id":1},{"id":2}]"#,
        &map_requirement,
        &map_program,
        format,
        dialect,
        &mut map_resources,
        &mut map_sink,
    )?;
    let mut expanded_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let expanded_report = run(
        catalog,
        br#"[{"id":1},{"id":2}]"#,
        &expanded_requirement,
        &expanded_program,
        format,
        dialect,
        &mut expanded_resources,
        &mut expanded_sink,
    )?;
    if map_sink.bytes != b"[1,2]\n"
        || map_sink.bytes != expanded_sink.bytes
        || map_report.access_route().route() != expanded_report.access_route().route()
        || map_report.access_route().route() != jqf_codec_json::FULL_PHYSICAL_ROUTE_ID
    {
        return Err(format!(
            "map-lowering receipt mismatch: map={:?}@{:?} expanded={:?}@{:?}",
            map_sink.bytes,
            map_report.access_route().route(),
            expanded_sink.bytes,
            expanded_report.access_route().route()
        ));
    }
    Ok(())
}

/// Compiles and runs `source` over `bytes`, returning published bytes and the physical route id.
fn run_published(
    catalog: CodecCatalog<'_, '_>,
    bytes: &[u8],
    source: &str,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(Vec<u8>, jqf_codec_core::PhysicalRouteId), String> {
    let mut resources = resources();
    let program = program_for(source, &resources)?;
    let requirement = program
        .try_requirement(&resources)
        .map_err(|error| format!("{source} requirement: {:?}", error.kind()))?;
    let mut sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let report = run(
        catalog,
        bytes,
        &requirement,
        &program,
        format,
        dialect,
        &mut resources,
        &mut sink,
    )?;
    Ok((sink.bytes, report.access_route().route()))
}

/// The comma vertical's prefix-pushdown thesis, proven by
/// receipt: a scoped prefix upstream of a choice residual
/// (`.catalog[2] | (.id, .name)`) pushes exactly its static prefix (`.catalog[2]`)
/// down to the codec, so it produces the SAME `AccessRequirement` and fires the
/// SAME scoped fastest-tool route as the bare prefix `.catalog[2]` alone. The
/// residual `Choice(.id, .name)` runs in the executor over the scoped-decoded
/// subtree; the codec never materializes the whole document. Byte output differs
/// (two choice members vs the located object), so only requirement and route are
/// asserted identical.
pub(crate) fn assert_choice_prefix_route_identity(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    const INPUT: &[u8] = br#"{"catalog":[{"id":0,"name":"item-0"},{"id":1,"name":"item-1"},{"id":2,"name":"item-2"}]}"#;

    let mut choice_resources = resources();
    let choice_program = program_for(".catalog[2] | (.id, .name)", &choice_resources)?;
    let choice_requirement = choice_program
        .try_requirement(&choice_resources)
        .map_err(|error| format!("choice requirement: {:?}", error.kind()))?;

    let mut bare_resources = resources();
    let bare_program = program_for(".catalog[2]", &bare_resources)?;
    let bare_requirement = bare_program
        .try_requirement(&bare_resources)
        .map_err(|error| format!("bare requirement: {:?}", error.kind()))?;

    // The choice-residual program's requirement is structurally identical to the
    // bare prefix's: same footprint, fingerprint, and result authority.
    if choice_requirement.footprint() != bare_requirement.footprint()
        || choice_requirement.footprint().fingerprint() != bare_requirement.footprint().fingerprint()
        || choice_requirement.result() != bare_requirement.result()
    {
        return Err(format!(
            "choice prefix requirement mismatch: choice={choice_requirement:?} bare={bare_requirement:?}"
        ));
    }

    let mut choice_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let choice_report = run(
        catalog,
        INPUT,
        &choice_requirement,
        &choice_program,
        format,
        dialect,
        &mut choice_resources,
        &mut choice_sink,
    )?;

    let mut bare_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let bare_report = run(
        catalog,
        INPUT,
        &bare_requirement,
        &bare_program,
        format,
        dialect,
        &mut bare_resources,
        &mut bare_sink,
    )?;

    // Same scoped route (id + slot) as the bare prefix; the choice residual emits
    // the two members (`2`, then `"item-2"`) of the scoped-decoded object.
    let choice_route = choice_report.access_route();
    let bare_route = bare_report.access_route();
    if choice_route.route() != bare_route.route()
        || choice_route.slot() != bare_route.slot()
        || choice_route.route() != jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID
        || choice_report.disposition() != PipelineDisposition::Emitted
        || choice_sink.bytes != b"2\n\"item-2\"\n"
        || bare_sink.bytes != b"{\"id\":2,\"name\":\"item-2\"}\n"
    {
        return Err(format!(
            "choice prefix route mismatch: choice={choice_report:?} bare={bare_report:?}"
        ));
    }
    Ok(())
}

/// The comma-precedence equivalence, proven by
/// receipt: `(.a, .b) | .c` and its unparenthesized spelling `.a, .b | .c` parse
/// to the SAME graph (comma binds tighter than pipe), so they produce a
/// structurally identical `AccessRequirement` and execute the exact same physical
/// route with byte-identical output.
pub(crate) fn assert_comma_pipe_equivalence(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    const INPUT: &[u8] = br#"{"a":{"c":1},"b":{"c":2}}"#;

    let mut grouped_resources = resources();
    let grouped_program = program_for("(.a, .b) | .c", &grouped_resources)?;
    let grouped_requirement = grouped_program
        .try_requirement(&grouped_resources)
        .map_err(|error| format!("grouped requirement: {:?}", error.kind()))?;

    let mut bare_resources = resources();
    let bare_program = program_for(".a, .b | .c", &bare_resources)?;
    let bare_requirement = bare_program
        .try_requirement(&bare_resources)
        .map_err(|error| format!("bare requirement: {:?}", error.kind()))?;

    // A top-level comma shares one document authority: whole-document route, so
    // both spellings request the complete document with identical footprints.
    if grouped_requirement.footprint() != bare_requirement.footprint()
        || grouped_requirement.footprint().fingerprint() != bare_requirement.footprint().fingerprint()
        || grouped_requirement.result() != bare_requirement.result()
    {
        return Err(format!(
            "comma/pipe requirement mismatch: grouped={grouped_requirement:?} bare={bare_requirement:?}"
        ));
    }

    let mut grouped_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let grouped_report = run(
        catalog,
        INPUT,
        &grouped_requirement,
        &grouped_program,
        format,
        dialect,
        &mut grouped_resources,
        &mut grouped_sink,
    )?;

    let mut bare_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let bare_report = run(
        catalog,
        INPUT,
        &bare_requirement,
        &bare_program,
        format,
        dialect,
        &mut bare_resources,
        &mut bare_sink,
    )?;

    // Identical executed route (id + slot) and byte-identical output (`1`, `2`).
    let grouped_route = grouped_report.access_route();
    let bare_route = bare_report.access_route();
    if grouped_requirement.result() != AccessResultKind::CompleteDocument
        || grouped_route.route() != bare_route.route()
        || grouped_route.slot() != bare_route.slot()
        || grouped_route.route() != jqf_codec_json::FULL_PHYSICAL_ROUTE_ID
        || grouped_sink.bytes != bare_sink.bytes
        || grouped_sink.bytes != b"1\n2\n"
    {
        return Err(format!(
            "comma/pipe route mismatch: grouped={grouped_report:?} bare={bare_report:?}"
        ));
    }
    Ok(())
}

/// `.a[].b` is an element row: it lowers the whole-document requirement with
/// an element hint so the span skeleton survives. Bare `.a` stays the scoped
/// forward route. Byte output differs (fan-out vs the located array).
pub(crate) fn assert_prefix_pushdown_route_contrast(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    const INPUT: &[u8] = br#"{"a":[{"b":1},{"b":2}]}"#;

    let mut fan_resources = resources();
    let fan_program = program_for(".a[].b", &fan_resources)?;
    let fan_requirement = fan_program
        .try_requirement(&fan_resources)
        .map_err(|error| format!("fan requirement: {:?}", error.kind()))?;

    let mut bare_resources = resources();
    let bare_program = program_for(".a", &bare_resources)?;
    let bare_requirement = bare_program
        .try_requirement(&bare_resources)
        .map_err(|error| format!("bare requirement: {:?}", error.kind()))?;

    // The fan-out row is an element row, so its requirement is the lazy
    // whole-document one with the element demand hint (the codec's span
    // skeleton must survive for the document-core consumer to iterate it).
    // The bare prefix keeps the scoped located route.
    if !fan_requirement.footprint().is_whole()
        || fan_requirement.result() != AccessResultKind::CompleteDocument
        || fan_requirement.element().is_none()
        || bare_requirement.footprint().is_whole()
        || bare_requirement.result() != AccessResultKind::Located
        || bare_requirement.element().is_some()
    {
        return Err(format!(
            "prefix pushdown requirement mismatch: fan={fan_requirement:?} bare={bare_requirement:?}"
        ));
    }

    let mut fan_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let fan_report = run(
        catalog,
        INPUT,
        &fan_requirement,
        &fan_program,
        format,
        dialect,
        &mut fan_resources,
        &mut fan_sink,
    )?;

    let mut bare_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let bare_report = run(
        catalog,
        INPUT,
        &bare_requirement,
        &bare_program,
        format,
        dialect,
        &mut bare_resources,
        &mut bare_sink,
    )?;

    // the fan-out row takes the LAZY WHOLE-DOCUMENT route with
    // the element demand hint (the codec's span skeleton survives for the
    // document-core consumer), where the bare prefix keeps the scoped located
    // route. The consumer fans `.a` out and projects `.b` (`1`, then `2`),
    // byte-identical to the old scoped route's publication.
    let fan_route = fan_report.access_route();
    let bare_route = bare_report.access_route();
    if fan_route.route() != jqf_codec_json::FULL_PHYSICAL_ROUTE_ID
        || bare_route.route() != jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID
        || fan_report.disposition() != PipelineDisposition::Emitted
        || fan_sink.bytes != b"1\n2\n"
        || bare_sink.bytes
            != br#"[{"b":1},{"b":2}]
"# {
        return Err(format!(
            "prefix pushdown route mismatch: fan={fan_report:?} bare={bare_report:?}"
        ));
    }
    Ok(())
}

/// The fusion thesis, proven by receipt rather than timing:
/// a static path `.a.b` and its pipe-of-paths spelling `.a | .b` must produce a
/// structurally identical `AccessRequirement` AND execute the exact same
/// physical route. The pipe fuses to the same single stage, so the scoped
/// fastest-tool route fires for both — the ladder's timing lane measures the
/// consequence; this asserts the mechanism.
pub(crate) fn assert_fusion_route_identity(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    const INPUT: &[u8] = br#"{"a":{"b":42}}"#;

    let mut path_resources = resources();
    let path_program = program_for(".a.b", &path_resources)?;
    let path_requirement = path_program
        .try_requirement(&path_resources)
        .map_err(|error| format!("path requirement: {:?}", error.kind()))?;

    let mut pipe_resources = resources();
    let pipe_program = program_for(".a | .b", &pipe_resources)?;
    let pipe_requirement = pipe_program
        .try_requirement(&pipe_resources)
        .map_err(|error| format!("pipe requirement: {:?}", error.kind()))?;

    // Structural requirement identity (account-independent): same footprint,
    // fingerprint, and result authority.
    if path_requirement.footprint() != pipe_requirement.footprint()
        || path_requirement.footprint().fingerprint() != pipe_requirement.footprint().fingerprint()
        || path_requirement.result() != pipe_requirement.result()
    {
        return Err(format!(
            "fusion requirement mismatch: path={path_requirement:?} pipe={pipe_requirement:?}"
        ));
    }

    let mut path_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let path_report = run(
        catalog,
        INPUT,
        &path_requirement,
        &path_program,
        format,
        dialect,
        &mut path_resources,
        &mut path_sink,
    )?;

    let mut pipe_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let pipe_report = run(
        catalog,
        INPUT,
        &pipe_requirement,
        &pipe_program,
        format,
        dialect,
        &mut pipe_resources,
        &mut pipe_sink,
    )?;

    // Identical executed physical route — the same scoped fastest-tool route id
    // and slot (the `provider_id` counter is per-provider-instance and so
    // differs between two independent runs; it is not the route identity) —
    // identical published bytes, and the scoped route actually fired.
    let path_route = path_report.access_route();
    let pipe_route = pipe_report.access_route();
    if path_route.route() != pipe_route.route()
        || path_route.slot() != pipe_route.slot()
        || path_route.route() != jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID
        || path_sink.bytes != pipe_sink.bytes
        || path_sink.bytes != b"42\n"
    {
        return Err(format!(
            "fusion route mismatch: path={path_report:?} pipe={pipe_report:?}"
        ));
    }
    Ok(())
}

pub(crate) fn assert_authoritative_empty_diagnostics(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    let mut resources = resources();
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::All);
    let requirement = try_lower_forward_requirement(policy, &[StaticForwardStep::ObjectKey("selected")], &resources)
        .map_err(|error| format!("{:?}", error.kind()))?;
    let mut sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let program = program_for(".selected", &resources)?;
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(13), SourceKind::Input),
        "diagnostics.json",
        br#"{"selected":true}"#,
        0,
    );
    let request = jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(br#"{"selected":true}"#))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_output_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::All,
                dialect: json_dialect(),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::Report,
            encode_options: None,
            cooperative_credits: 7,
            split: None,

            max_iterations: None,
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, &mut sink).map_err(|error| format!("diagnostic access: {error:?}"))?;
    let report = match outcome {
        jqf_sdk::Outcome::Served(jqf_sdk::Report::Pipeline(report)) => report,
        other => return Err(format!("diagnostic outcome unexpected: {other:?}")),
    };
    // An exact `Located` requirement Direct-binds the scoped route,
    // even under `DiagnosticPolicy::All`: the authoritative-empty diagnostic
    // coverage is carried by the scoped materialization, not a whole-route
    // adapter.
    if sink.bytes != b"true\n"
        || report.access_route().route() != jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID
        || report.access_report().adapter() != AccessAdapter::None
        || report.access_report().diagnostics() != DiagnosticCoverage::AuthoritativeEmpty
    {
        return Err(format!("diagnostic report mismatch: {report:?}"));
    }
    Ok(())
}

pub(crate) fn assert_ordered_many(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    let mut resources = resources();
    let mut producer = ManyProducer {
        items: vec![
            Value::try_string("one").map_err(|error| format!("text: {error:?}"))?,
            Value::Bool(true),
            Value::Null,
        ]
        .into_iter(),
        pending: true,
    };
    let mut sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let report = encode_ordered(
        catalog,
        &mut producer,
        format,
        dialect,
        OrderedEncodingPolicy {
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::Report,
            options: None,
            cooperative_credits: 7,
            split: None,
            flush_each_item: false,
        },
        FacadeFraming::item_suffix(b"\n"),
        &mut resources,
        &mut sink,
    )
    .map_err(|error| format!("ordered producer: {error:?}"))?;
    if sink.bytes != b"\"one\"\ntrue\nnull\n"
        || sink.boundaries != [(true, 0), (false, 0), (true, 1), (false, 1), (true, 2), (false, 2)]
        || sink.reports.len() != 3
        || sink.reports[0].codec_bytes() != 5
        || sink.reports[1].codec_bytes() != 4
        || sink.reports[2].codec_bytes() != 4
        || sink.reports.iter().any(|item| item.framing_bytes() != 1)
        || sink.reports.iter().any(|item| {
            item.physical_encoder() != jqf_codec_json::ENCODE_PHYSICAL_ROUTE_ID
                || !matches!(
                    item.preservation(),
                    Some(preservation)
                        if preservation.semantic_values() == PreservationOutcome::Exact
                            && preservation.tags_and_facts() == PreservationOutcome::Exact
                            && preservation.ordering() == PreservationOutcome::Exact
                            && preservation.presentation() == PreservationOutcome::Normalized
                )
        })
        || report.publication()
            != (PublicationStatus::Complete {
                items: 3,
                published_bytes: 16,
            })
    {
        return Err(format!("ordered many mismatch: {report:?}"));
    }
    Ok(())
}

pub(crate) fn assert_adversarial_boundaries(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    policy: CodecRequirementPolicy,
) -> Result<(), String> {
    for (mode, expected) in [
        (FaultMode::Zero, "SinkContract"),
        (FaultMode::Oversized, "SinkContract"),
        (FaultMode::Begin, "begin failure"),
        (FaultMode::Finish, "finish failure"),
    ] {
        let mut resources = resources();
        let requirement =
            try_lower_root_requirement(policy, Some(0), &resources).map_err(|error| format!("{:?}", error.kind()))?;
        let mut sink = FaultSink {
            mode,
            bytes: Vec::new(),
        };
        let program = program_for(".", &resources)?;
        let error = execute_root(
            catalog,
            b"true",
            &requirement,
            &program,
            format,
            dialect,
            &mut resources,
            &mut sink,
        )
        .expect_err("fault sink must fail");
        let (expected_publication, expected_output_bytes) = match mode {
            FaultMode::Begin => (PublicationStatus::NotStarted, 0),
            FaultMode::Finish => (
                PublicationStatus::InProgress {
                    completed_items: 0,
                    published_bytes: 5,
                },
                5,
            ),
            FaultMode::Zero | FaultMode::Oversized => (
                PublicationStatus::InProgress {
                    completed_items: 0,
                    published_bytes: 0,
                },
                0,
            ),
            FaultMode::CancelAfterWrite(_) | FaultMode::CancelAfterFraming(_, _) => unreachable!(),
        };
        if !format!("{error:?}").contains(expected)
            || resources.snapshot().output_bytes() != expected_output_bytes
            || resources.snapshot().output_reserved_bytes() != 0
            || error.publication() != Some(expected_publication)
        {
            return Err(format!("fault mode mismatch: {error:?}"));
        }
    }

    assert_output_limit(catalog, format, dialect, policy)?;
    assert_publication_cancellation(catalog, format, dialect)?;
    Ok(())
}

pub(crate) fn assert_output_limit(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    policy: CodecRequirementPolicy,
) -> Result<(), String> {
    let mut resources = resources_with(&CONTROL, 3, 7);
    let requirement =
        try_lower_root_requirement(policy, Some(0), &resources).map_err(|error| format!("{:?}", error.kind()))?;
    let mut sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let program = program_for(".", &resources)?;
    let Err(error) = execute_root(
        catalog,
        b"true",
        &requirement,
        &program,
        format,
        dialect,
        &mut resources,
        &mut sink,
    ) else {
        return Err("output limit must fail".into());
    };
    if !format!("{error:?}").contains("OutputBytes")
        || resources.snapshot().output_bytes() != 0
        || resources.snapshot().output_reserved_bytes() != 0
        || !sink.bytes.is_empty()
        || error.publication()
            != Some(PublicationStatus::InProgress {
                completed_items: 0,
                published_bytes: 0,
            })
    {
        return Err(format!("output limit mismatch: {error:?}"));
    }
    Ok(())
}

pub(crate) fn assert_publication_cancellation(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    for (control, mode, expected_bytes) in [
        (ToggleControl(core::sync::atomic::AtomicBool::new(false)), 0_u8, 1_u64),
        (ToggleControl(core::sync::atomic::AtomicBool::new(false)), 1_u8, 5_u64),
    ] {
        let mut resources = resources_with(&control, u64::MAX, 7);
        let mut producer = ManyProducer {
            items: vec![Value::Bool(true)].into_iter(),
            pending: false,
        };
        let fault = if mode == 0 {
            FaultMode::CancelAfterWrite(&control)
        } else {
            FaultMode::CancelAfterFraming(&control, 4)
        };
        let mut sink = FaultSink {
            mode: fault,
            bytes: Vec::new(),
        };
        let Err(error) = encode_ordered(
            catalog,
            &mut producer,
            format,
            dialect,
            OrderedEncodingPolicy {
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                options: None,
                cooperative_credits: 7,
                split: None,
                flush_each_item: false,
            },
            FacadeFraming::item_suffix(b"\n"),
            &mut resources,
            &mut sink,
        ) else {
            return Err("cancellation must stop publication".into());
        };
        if !format!("{error:?}").contains("Cancelled")
            || resources.snapshot().output_bytes() != expected_bytes
            || resources.snapshot().output_reserved_bytes() != 0
            || error.publication()
                != (PublicationStatus::InProgress {
                    completed_items: 0,
                    published_bytes: expected_bytes,
                })
        {
            return Err(format!("publication cancellation mismatch: {error:?}"));
        }
    }
    Ok(())
}
