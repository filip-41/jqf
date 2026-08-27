//! SDK/FFI smoke battery: table-driven receipts through `jqf-sdk` and the C ABI.
//!
//! Each receipt pins one law (route inventory, prefix pushdown, force-route
//! corpus, FFI row). `main` runs the inline pipeline rows then the `assert_*`
//! inventory and prints one line. Helpers live in sibling modules; they are
//! the harness, not product code.

mod codecs;
mod corpus;
mod equivalence;
mod ffi;
mod grants;
mod harness;
mod prefix;
mod projection;
mod records;
mod transfer;

use crate::codecs::{
    assert_codec_route_inventories, assert_edit_capability_declarations, assert_every_codec_answers_every_demand,
    assert_flat_route_inventory, assert_mismatch_policy, assert_render_surface, assert_xml_force_route,
};
use crate::corpus::assert_force_route_corpus;
use crate::equivalence::assert_equivalence_classes;
use crate::ffi::assert_ffi_correct_core;
use crate::grants::assert_task_grants;
use crate::harness::{FailingSink, PartialSink, is_scoped_exact_report, program_for, resources, run};
use crate::prefix::{
    assert_adversarial_boundaries, assert_authoritative_empty_diagnostics, assert_bind_source_prefix_route,
    assert_choice_prefix_route_identity, assert_comma_pipe_equivalence, assert_constructor_shapes,
    assert_fusion_route_identity, assert_map_lowering_equivalence, assert_ordered_many,
    assert_prefix_pushdown_route_contrast, assert_prefix_route_family,
};
use crate::projection::{
    assert_explain_plan, assert_plan_serialization, assert_projection_classes, assert_projection_floor_oracle,
};
use crate::records::{assert_csv_route, assert_json_seq_route, assert_record_route};
use crate::transfer::assert_demand_transfer_registry;

use jqf_codec_core::{AccessAdapter, DiagnosticPolicy, ValidationMode};
use jqf_data::{DiagnosticCoverage, DialectId, FormatId};
use jqf_engine::{
    CodecRequirementPolicy, StaticForwardStep, try_lower_forward_requirement, try_lower_root_requirement,
};
use jqf_sdk::{CodecCatalog, PipelineDisposition, PublicationStatus};

#[allow(
    clippy::too_many_lines,
    reason = "the deterministic smoke keeps its linear scenario inventory directly auditable"
)]
fn main() -> Result<(), String> {
    let registration = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let toml_registration = jqf_codec_toml::registration_1_0().map_err(|error| format!("{error:?}"))?;
    let xml_registration = jqf_codec_xml::registration().map_err(|error| format!("{error:?}"))?;
    let properties_registration = jqf_codec_ini::registration().map_err(|error| format!("{error:?}"))?;
    let ini_registration = jqf_codec_ini::registration_ini().map_err(|error| format!("{error:?}"))?;
    let dotenv_registration = jqf_codec_ini::registration_dotenv().map_err(|error| format!("{error:?}"))?;
    let registrations = [
        &registration,
        &toml_registration,
        &xml_registration,
        &properties_registration,
        &ini_registration,
        &dotenv_registration,
    ];
    let catalog = CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| error.to_string())?;
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| error.to_string())?;
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);

    let mut root_resources = resources();
    let root_requirement =
        try_lower_root_requirement(policy, Some(0), &root_resources).map_err(|error| format!("{:?}", error.kind()))?;
    let mut root_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let root_program = program_for(".", &root_resources)?;
    let root = run(
        catalog,
        br#"{"a":[1,true,"x"]}"#,
        &root_requirement,
        &root_program,
        &format,
        &dialect,
        &mut root_resources,
        &mut root_sink,
    )?;
    if root_sink.bytes
        != br#"{"a":[1,true,"x"]}
"# || root_sink.boundaries != [(true, 0), (false, 0)]
        || root.publication()
            != (PublicationStatus::Complete {
                items: 1,
                published_bytes: 19,
            })
        || root_resources.snapshot().output_bytes() != 19
        || root_sink.reports.len() != 1
        || root_sink.reports[0].physical_encoder() != jqf_codec_json::ENCODE_PHYSICAL_ROUTE_ID
        || root_sink.reports[0].codec_bytes() != 18
        || root_sink.reports[0].framing_bytes() != 1
        || root.access_route().route() != jqf_codec_json::FULL_PHYSICAL_ROUTE_ID
        || root.access_report().adapter() != AccessAdapter::None
        || root.access_report().diagnostics() != DiagnosticCoverage::NotRequested
    {
        return Err(format!("root receipt mismatch: {root:?}"));
    }

    let mut exact_root_resources = resources();
    let exact_root_requirement = try_lower_forward_requirement(policy, &[], &exact_root_resources)
        .map_err(|error| format!("{:?}", error.kind()))?;
    let mut exact_root_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    // The exact-root located selection (empty forward path) has no source
    // spelling; identity carries the interpretation (a resolved value ignores
    // step flags), so the requirement under test drives the route unchanged.
    let exact_root_program = program_for(".", &exact_root_resources)?;
    let exact_root = run(
        catalog,
        br#"{"a":[1,true,"x"]}"#,
        &exact_root_requirement,
        &exact_root_program,
        &format,
        &dialect,
        &mut exact_root_resources,
        &mut exact_root_sink,
    )?;
    if exact_root_sink.bytes
        != br#"{"a":[1,true,"x"]}
"# || exact_root.disposition() != PipelineDisposition::Emitted
        || !is_scoped_exact_report(exact_root)
    {
        return Err(format!("exact-root receipt mismatch: {exact_root:?}"));
    }

    let mut forward_resources = resources();
    let forward_requirement = try_lower_forward_requirement(
        policy,
        &[StaticForwardStep::ObjectKey("a"), StaticForwardStep::ArrayIndex(1)],
        &forward_resources,
    )
    .map_err(|error| format!("{:?}", error.kind()))?;
    let mut forward_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let forward_program = program_for(".a[1]", &forward_resources)?;
    let selected = run(
        catalog,
        br#"{"a":[1,{"selected":2}]}"#,
        &forward_requirement,
        &forward_program,
        &format,
        &dialect,
        &mut forward_resources,
        &mut forward_sink,
    )?;
    if forward_sink.bytes != b"{\"selected\":2}\n"
        || selected.disposition() != PipelineDisposition::Emitted
        || !is_scoped_exact_report(selected)
    {
        return Err(format!("forward receipt mismatch: {selected:?}"));
    }

    let mut signed_index_resources = resources();
    let signed_index_requirement = try_lower_forward_requirement(
        policy,
        &[StaticForwardStep::ObjectKey("items"), StaticForwardStep::ArrayIndex(-1)],
        &signed_index_resources,
    )
    .map_err(|error| format!("{:?}", error.kind()))?;
    let mut signed_index_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let signed_index_program = program_for(".items[-1]", &signed_index_resources)?;
    let signed_index = run(
        catalog,
        br#"{"items":[10,20]}"#,
        &signed_index_requirement,
        &signed_index_program,
        &format,
        &dialect,
        &mut signed_index_resources,
        &mut signed_index_sink,
    )?;
    if signed_index_sink.bytes != b"20\n"
        || signed_index.disposition() != PipelineDisposition::Emitted
        || !is_scoped_exact_report(signed_index)
    {
        return Err(format!("signed-index receipt mismatch: {signed_index:?}"));
    }

    let mut missing_resources = resources();
    let missing_requirement =
        try_lower_forward_requirement(policy, &[StaticForwardStep::ObjectKey("missing")], &missing_resources)
            .map_err(|error| format!("{:?}", error.kind()))?;
    let mut missing_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let missing_program = program_for(".missing", &missing_resources)?;
    let missing = run(
        catalog,
        br#"{"a":1}"#,
        &missing_requirement,
        &missing_program,
        &format,
        &dialect,
        &mut missing_resources,
        &mut missing_sink,
    )?;
    if missing.publication()
        != (PublicationStatus::Complete {
            items: 1,
            published_bytes: 5,
        })
        || missing.disposition() != PipelineDisposition::Missing
        || !is_scoped_exact_report(missing)
        || missing_sink.bytes != b"null\n"
        || missing_sink.boundaries != [(true, 0), (false, 0)]
    {
        return Err(format!("missing receipt mismatch: {missing:?}"));
    }

    let mut mismatch_resources = resources();
    let mismatch_requirement = try_lower_forward_requirement(
        policy,
        &[StaticForwardStep::ObjectKey("a"), StaticForwardStep::ObjectKey("b")],
        &mismatch_resources,
    )
    .map_err(|error| format!("{:?}", error.kind()))?;
    let mut mismatch_sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };
    let mismatch_program = program_for(".a.b", &mismatch_resources)?;
    let Err(mismatch) = run(
        catalog,
        br#"{"a":1}"#,
        &mismatch_requirement,
        &mismatch_program,
        &format,
        &dialect,
        &mut mismatch_resources,
        &mut mismatch_sink,
    ) else {
        return Err("type mismatch must abort the request before encoding".into());
    };
    if !mismatch.contains("TypeMismatch")
        || !mismatch.contains("step_index: 1")
        || !mismatch.contains("Number")
        || !mismatch.contains("NotStarted")
        || !mismatch_sink.bytes.is_empty()
        || !mismatch_sink.boundaries.is_empty()
    {
        return Err(format!("type mismatch receipt mismatch: {mismatch}"));
    }

    assert_authoritative_empty_diagnostics(catalog, &format, &dialect)?;

    let mut failure_resources = resources();
    let failure_requirement = try_lower_root_requirement(policy, Some(0), &failure_resources)
        .map_err(|error| format!("{:?}", error.kind()))?;
    let mut failing_sink = FailingSink { bytes: Vec::new() };
    let failure_program = program_for(".", &failure_resources)?;
    let Err(failure) = run(
        catalog,
        br#"{"published":true}"#,
        &failure_requirement,
        &failure_program,
        &format,
        &dialect,
        &mut failure_resources,
        &mut failing_sink,
    ) else {
        return Err("injected sink failure must escape".into());
    };
    if !failure.contains("injected sink failure")
        || failing_sink.bytes.len() != 4
        || failure_resources.snapshot().output_bytes() != 4
        || failure_resources.snapshot().output_reserved_bytes() != 0
    {
        return Err(format!("sink failure accounting mismatch: {failure}"));
    }

    assert_ordered_many(catalog, &format, &dialect)?;
    assert_adversarial_boundaries(catalog, &format, &dialect, policy)?;
    assert_fusion_route_identity(catalog, &format, &dialect)?;
    assert_prefix_pushdown_route_contrast(catalog, &format, &dialect)?;
    assert_choice_prefix_route_identity(catalog, &format, &dialect)?;
    assert_comma_pipe_equivalence(catalog, &format, &dialect)?;
    assert_constructor_shapes(catalog, &format, &dialect)?;
    assert_map_lowering_equivalence(catalog, &format, &dialect)?;
    assert_prefix_route_family(catalog, &format, &dialect)?;
    assert_projection_classes()?;
    assert_explain_plan()?;
    assert_plan_serialization()?;
    assert_demand_transfer_registry()?;
    assert_equivalence_classes(catalog, &format, &dialect)?;
    assert_projection_floor_oracle(catalog, &format, &dialect)?;
    assert_bind_source_prefix_route(catalog, &format, &dialect)?;
    assert_force_route_corpus(catalog, &format, &dialect)?;
    let xml_format = FormatId::try_new(jqf_codec_xml::FORMAT_ID).map_err(|error| error.to_string())?;
    let xml_dialect = DialectId::try_new(jqf_codec_xml::XML_DOCUMENT_DIALECT_ID).map_err(|error| error.to_string())?;
    assert_xml_force_route(catalog, &xml_format, &xml_dialect)?;
    assert_record_route(&format, &dialect)?;
    assert_csv_route()?;
    assert_json_seq_route()?;
    assert_codec_route_inventories()?;
    assert_flat_route_inventory()?;
    assert_edit_capability_declarations()?;
    assert_render_surface()?;
    assert_every_codec_answers_every_demand()?;
    assert_ffi_correct_core();
    assert_task_grants()?;

    // The mismatch dial is a request field: lenient yields the value and
    // counts nothing, warn yields it and counts the cell, strict raises.
    assert_mismatch_policy(catalog, &format, &dialect)?;

    println!(
        "sdk-smoke: root=true exact_root=true forward=true signed_index=true missing=true type_mismatch=true diagnostics=true sink_failure=true ordered_many=true adversarial_boundaries=true fusion_route=true prefix_pushdown_route=true choice_prefix_route=true comma_pipe_equivalence=true constructor_route=true call_prefix_route=true map_lowering_equivalence=true arith_prefix_route=true conditional_prefix_route=true try_prefix_route=true reduce_prefix_route=true bind_prefix_route=true descent_prefix_route=true slice_prefix_route=true projection_class=true explain_plan=true plan_serialization=true demand_transfer_registry=true equivalence_classes=true projection_floor_oracle=true bind_source_prefix_route=true force_route_corpus=true xml_force_route=true record_route=true csv_record_route=true json_seq_routes=true toml_routes=true yaml_routes=true html_routes=true json_routes=true jsonc_routes=true json5_routes=true cbor_seq_routes=true jqft_routes=true jqfb_routes=true xml_routes=true messagepack_routes=true properties_routes=true ini_routes=true dotenv_routes=true edit_capability=true render_surface=true every_codec_answers_every_demand=true ffi_correct_core=true task_grants=true mismatch_policy=true"
    );
    Ok(())
}
