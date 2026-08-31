//! Element/count/keys/tag requirement probes.

use super::*;

#[test]
fn element_requirement_carries_the_hint() {
    let resources = ResourceContext::new(
        jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u32::MAX,
        ))
        .expect("account"),
        &jqf_resource::ContinueControl,
        jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
    )
    .expect("resources");
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    for (source, should, eager) in [
        (".catalog[] | .name", true, false),
        (".catalog[] | {id,name,sku}", true, false),
        (".[] | length", true, false),
        (".catalog | map(.name)", true, false),
        (".catalog | map({id, name})", true, false),
        ("[.catalog[] | {id}]", true, false),
        (
            "reduce (.catalog[] | .attrs.warehouse) as $w ({}; .[$w] += 1)",
            true,
            true,
        ),
        (".catalog[] | select(.id > 1) | .name", true, false),
        (".catalog | map(select(.id > 1))", true, false),
        (".", false, false),
    ] {
        let program = try_compile_program(source, policy, CompileOptions::new(), &resources).expect("compiles");
        let demand = match program.shortcut() {
            Shortcut::Element { demand, .. } => Some(demand),
            _ => None,
        };
        assert_eq!(demand.is_some(), should, "{source}");
        let requirement = program.try_requirement(&resources).expect("lowers");
        if !should {
            continue;
        }
        let empty_path = demand.expect("row").path.is_empty();
        if empty_path {
            assert!(requirement.element().is_some(), "{source} Whole element keeps the hint");
            assert!(requirement.footprint().is_whole(), "{source} empty prefix stays Whole");
            assert_eq!(
                requirement.lazy_frontier(),
                u32::from(!eager),
                "{source} fan-out stays lazy; unbounded fold is eager"
            );
        } else {
            assert_eq!(program.access(), Access::Exact, "{source}");
            assert!(
                !requirement.footprint().is_whole(),
                "{source} Exact-locates the container"
            );
            assert!(
                requirement.element().is_some(),
                "{source} Exact still carries the element hint"
            );
            assert_eq!(
                requirement.lazy_frontier(),
                u32::from(!eager),
                "{source} fan-out stays lazy; unbounded fold is eager"
            );
        }
    }
}

#[test]
fn unbounded_count_and_collect_are_eager_bounded_element_stays_lazy() {
    let resources = ResourceContext::new(
        jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u32::MAX,
        ))
        .expect("account"),
        &jqf_resource::ContinueControl,
        jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
    )
    .expect("resources");
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let count = try_compile_program(".catalog | length", policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(
        matches!(count.shortcut(), Shortcut::Count(_)),
        "PATH|length is a count row"
    );
    assert_eq!(
        count.try_requirement(&resources).expect("lowers").lazy_frontier(),
        0,
        "unbounded count is eager"
    );
    let collected =
        try_compile_program("[.catalog[] | .id]", policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(
        matches!(collected.shortcut(), Shortcut::Element { .. }),
        "collected fan-out is an element row"
    );
    assert_eq!(
        collected.try_requirement(&resources).expect("lowers").lazy_frontier(),
        1,
        "unbounded collect stays lazy so the span leaf can extract"
    );
    let limited =
        try_compile_program("limit(2; .catalog[] | .id)", policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(
        matches!(limited.shortcut(), Shortcut::Element { .. }),
        "limit fan-out is an element row"
    );
    assert_eq!(
        limited.try_requirement(&resources).expect("lowers").lazy_frontier(),
        1,
        "bounded prefix stays lazy"
    );
}

#[test]
fn collect_piped_to_add_rewrites_to_reduce() {
    let resources = ResourceContext::new(
        jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u32::MAX,
        ))
        .expect("account"),
        &jqf_resource::ContinueControl,
        jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
    )
    .expect("resources");
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let program =
        try_compile_program("[.catalog[] | .n] | add", policy, CompileOptions::new(), &resources).expect("compiles");
    assert!(
        matches!(program.arena()[program.root().index()], ProgramNode::Reduce { .. }),
        "[STREAM]|add must be a reduce"
    );
}

#[test]
fn tag_accessor_inserts_intrinsic_tag_clause() {
    let resources = ResourceContext::new(
        jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u64::MAX,
            u32::MAX,
        ))
        .expect("account"),
        &jqf_resource::ContinueControl,
        jqf_resource::WorkMeter::try_new_v1(4096).expect("work"),
    )
    .expect("resources");
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let tagged = try_compile_program(".@tag", policy, CompileOptions::new(), &resources).expect("compiles");
    let demand = tagged.try_requirement(&resources).expect("lowers");
    assert!(
        demand
            .demand()
            .clauses()
            .iter()
            .any(|clause| matches!(clause, jqf_codec_core::DemandClause::IntrinsicTag)),
        ".@tag names the intrinsic-tag clause"
    );
    let commented = try_compile_program(".@comment", policy, CompileOptions::new(), &resources).expect("compiles");
    let demand = commented.try_requirement(&resources).expect("lowers");
    assert!(
        demand.demand().clauses().iter().any(|clause| matches!(
            clause,  jqf_codec_core::DemandClause::AttachedFact { role, .. } if role.as_str() == "comment"
        )),
        ".@comment names the comment attached-fact clause"
    );
    let aliased = try_compile_program(".@comment_head", policy, CompileOptions::new(), &resources).expect("compiles");
    let demand = aliased.try_requirement(&resources).expect("lowers");
    assert!(
        demand.demand().clauses().iter().any(|clause| matches!(
            clause,  jqf_codec_core::DemandClause::AttachedFact { role, .. } if role.as_str() == "comment"
        )),
        ".@comment_head lowers as the comment attached-fact clause"
    );
    let href = try_compile_program(".&href", policy, CompileOptions::new(), &resources).expect("compiles");
    let demand = href.try_requirement(&resources).expect("lowers");
    assert!(
        demand.demand().clauses().iter().any(|clause| matches!(
            clause,  jqf_codec_core::DemandClause::Attribute(name) if name.local_name() == "href"
        )),
        ".&href names the attribute clause"
    );
    let plain = try_compile_program(".", policy, CompileOptions::new(), &resources).expect("compiles");
    let demand = plain.try_requirement(&resources).expect("lowers");
    assert!(
        demand.demand().clauses().iter().all(|clause| !matches!(
            clause,
            jqf_codec_core::DemandClause::IntrinsicTag
                | jqf_codec_core::DemandClause::AttachedFact { .. }
                | jqf_codec_core::DemandClause::Attribute(_)
        )),
        "identity does not demand tag, fact, or attribute clauses"
    );
    let xpath = try_compile_program("xpath(\"//item\")", policy, CompileOptions::new(), &resources).expect("compiles");
    let demand = xpath.try_requirement(&resources).expect("lowers");
    assert!(
        demand
            .demand()
            .clauses()
            .iter()
            .any(|clause| matches!(clause, jqf_codec_core::DemandClause::Topology(_))),
        "xpath names topology"
    );
    assert!(
        demand.demand().clauses().iter().any(|clause| matches!(
            clause,
            jqf_codec_core::DemandClause::AttachedFact { role, .. } if role.as_str() == "content"
        )),
        "xpath names content fact"
    );
    assert!(matches!(
        try_compile_program("keys", policy, CompileOptions::new(), &resources)
            .expect("compiles")
            .shortcut(),
        Shortcut::Keys(_)
    ));
    let keys = try_compile_program(".users | keys", policy, CompileOptions::new(), &resources).expect("compiles");
    let Shortcut::Keys(path) = keys.shortcut() else {
        panic!("PATH | keys is a keys row");
    };
    assert_eq!(path.len(), 1);
}
