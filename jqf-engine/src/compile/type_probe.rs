//! Type-demand requirement probes.

use super::*;

fn ledger() -> ResourceContext<'static> {
    let account = jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u32::MAX,
    ))
    .expect("account");
    let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work");
    jqf_resource::ResourceContext::new(account, &jqf_resource::ContinueControl, work).expect("resources")
}

#[test]
fn type_lowers_whole_document_with_the_hint() {
    let resources = ledger();
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let program = try_compile_program("type", policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(
        program.program.split().is_whole_document(),
        "type must be whole-document split"
    );
    assert!(program.type_demand(), "type must carry the type demand");
    let requirement = program.try_requirement(&resources).expect("lowers");
    assert!(requirement.footprint().is_whole(), "type must lower whole");
    assert!(requirement.type_demand(), "requirement must carry the hint");
}

#[test]
fn histogram_fold_has_a_prune_and_attaches_it() {
    let resources = ledger();
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let source = "reduce (.catalog[] | .attrs.warehouse) as $w ({}; .[$w] += 1)";
    let program = try_compile_program(source, policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(
        program.program.split().is_whole_document(),
        "the fold must be whole-document"
    );
    assert!(
        program.prune_tree().is_some(),
        "the fold must derive a kept-subtree tree"
    );
    let requirement = program.try_requirement(&resources).expect("lowers");
    assert!(requirement.prune().is_some(), "the requirement must carry the prune");
    // Unbounded element rows decode EAGER so the prune is armed (JSON
    // drops prune under a nonzero frontier). Bounded prefixes stay lazy.
    assert_eq!(requirement.lazy_frontier(), 0, "the unbounded fold is eager");
    assert!(
        requirement.element().is_some(),
        "the fold must carry the element demand hint"
    );
}

#[test]
fn type_question_declines_and_path_type_exact_locates() {
    let resources = ledger();
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    for source in ["type?", "[.a] | type", "type, ."] {
        let program = try_compile_program(source, policy, CompileOptions::new(), &resources).expect("compiles");
        assert!(!program.type_demand(), "{source} must not be a type row");
    }
    let path_type = try_compile_program(".a | type", policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(path_type.type_demand(), "PATH | type is a type row");
    assert_eq!(
        path_type.type_demand_path().expect("path"),
        &[jqf_data::CountStep::ObjectKey(alloc::string::String::from("a"))][..]
    );
    let requirement = path_type.try_requirement(&resources).expect("lowers");
    assert!(
        !requirement.footprint().is_whole(),
        "PATH | type Exact-locates the prefix so residual type sees the named node"
    );
    assert!(requirement.type_demand(), "PATH | type still carries the kind hint");
}

#[test]
fn slice_then_type_does_not_lower_as_bare_root_type() {
    let resources = ledger();
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let program = try_compile_program(".[1:3] | type", policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(
        !program.type_demand(),
        "a trailing slice is not a type row: empty-path plus range would type the root"
    );
    let requirement = program.try_requirement(&resources).expect("lowers");
    assert!(
        !requirement.type_demand(),
        ".[1:3] | type must not take the Whole+empty-path type arm"
    );
    let users = try_compile_program(".users[1:3] | type", policy, CompileOptions::new(), &resources).expect("compiles");
    let users_requirement = users.try_requirement(&resources).expect("lowers");
    assert!(
        !users_requirement.footprint().is_whole(),
        ".users[1:3] | type Exact-locates the prefix"
    );
}

/// Engine-lowered Exact `type_demand` at `.users` must not retain the array's child nodes.
#[test]
fn exact_type_demand_at_users_does_not_retain_child_nodes() {
    let mut resources = ledger();
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let program = try_compile_program(".users | type", policy, CompileOptions::new(), &resources).expect("compiles");
    let requirement = program.try_requirement(&resources).expect("lowers");
    assert!(requirement.type_demand(), "PATH | type carries the kind hint");
    assert!(
        !requirement.footprint().is_whole(),
        "PATH | type Exact-locates the prefix"
    );
    let bytes = br#"{"users":[1,2,3]}"#;
    let source = jqf_source::ResolvedSource::new(
        jqf_source::SourceRef::new(jqf_source::SourceId::new(0), jqf_source::SourceKind::Input),
        "input.json",
        bytes,
        0,
    );
    let registration = jqf_codec_json::registration().expect("json");
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source,
            jqf_codec_core::DecodeRequest {
                validation: jqf_codec_core::ValidationMode::Strict,
                diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                dialect: &jqf_data::DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .expect("provider");
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let result = {
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.decode(&mut run).expect("decode")
    };
    let jqf_codec_core::AccessOutcome::Located(located) = result.outcome() else {
        panic!("PATH | type must locate")
    };
    let jqf_codec_core::ExactSelectionRecord::Node { node, .. } = located.result() else {
        panic!("located node")
    };
    let document = located.product().document();
    let view = document.value_view(*node).expect("view");
    assert_eq!(
        view.kind().expect("kind"),
        jqf_data::ValueKind::Array,
        ".users is an array"
    );
    let array = view.array().expect("array").expect("array view");
    assert_eq!(array.len(), 0, "type_demand must not retain child nodes");
    assert_eq!(document.node_count(), 1, "kind-only document is the empty array");
}
