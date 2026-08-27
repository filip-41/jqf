//! Projection-class, explain-plan, and floor-oracle receipts.
//!
//! [`projection_class_label`] is the stable spelling the demand-transfer
//! table and equivalence classes pin against. The floor oracle drives
//! designated-vs-`[.][0] |` identity over the projection probe document.
//! Label helpers stay here; the oracle driver is [`crate::harness::oracle_run_over`].

use crate::harness::{OracleOutcome, OracleRoute, oracle_run_over, program_for, resources};
use jqf_codec_core::AccessResultKind;
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CompiledProgram, ExplainPlan, ProjectionClass, StaticForwardStep};
use jqf_sdk::CodecCatalog;
use std::fmt::Write as _;

/// Projection-class receipt: the DIAGNOSTIC projection class of every probe
/// program, including the three adversarial pairs, the `reduce ..` shape, a
/// bound-handle escape pair, and a `select` pass-through pair.
///
/// The class selects no route and lowers no requirement — this receipt is the
/// only thing that observes it today, so a wrong answer is a failing receipt
/// rather than wrong bytes.
pub(crate) fn assert_projection_classes() -> Result<(), String> {
    // (program, expected class). `Fields[…]` names are sorted and deduplicated.
    const TABLE: &[(&str, &str)] = &[
        // ---- the projection table, in table order ----
        ("reduce .catalog[] as $x (0; . + 1)", "Structure"),
        // The commutative mirror is the same fold row.
        ("reduce .catalog[] as $x (0; 1 + .)", "Structure"),
        ("[.catalog[]] | length", "Structure"),
        (".catalog | map(.name) | length", "Structure"),
        ("reduce .catalog[].id as $i (0; . + $i)", "Fields[id]"),
        ("[.catalog[] | select(.id > 35990)] | length", "Fields[id]"),
        // The all-static-key construct under a payload-free demand: the member
        // values carry the outgoing Structure demand, so the count table's
        // construct row can serve single-path shapes. A dynamic key keeps the
        // Subtree fallback and names its fields.
        ("[.catalog[] | {x: .id}] | length", "Structure"),
        ("[.catalog[] | {(.k): .v}] | length", "Fields[k,v]"),
        // The three adversarial pairs.
        ("[.catalog[] | select(.id > 35990)]", "Subtree"),
        ("[.catalog[] | length]", "Subtree"),
        ("map(.name) | length", "Structure"),
        // ---- shape notes ----
        // Born-P2 by SHAPE, not by demand: `$x` is never read, but `..` has no
        // `.[]` element boundary at all, so there is nothing to project.
        ("reduce .. as $x (0; . + 1)", "Subtree"),
        // Bound-handle escape vs. a handle consumed only by projected steps.
        ("[.catalog[] as $x | $x]", "Subtree"),
        ("[.catalog[] as $x | $x.id]", "Fields[id]"),
        ("[.catalog[] as $x | $x.id] | length", "Structure"),
        // `select` pass-through: the union of the condition's demand and the
        // pass-through's, and nothing more.
        ("[.catalog[] | select(.id > 35990) | .name]", "Fields[id,name]"),
        // Projected member navigation — the S0 witness lane's located shape.
        (".catalog[].name", "Fields[name]"),
        (".catalog[0].id", "Subtree"),
        // The conservative default, through a registered builtin with no pinned
        // transfer function.
        ("[.catalog[] | keys] | length", "Subtree"),
    ];

    for (source, expected) in TABLE {
        let resources = resources();
        let program = program_for(source, &resources)?;
        let actual = projection_class_label(&program);
        if actual != *expected {
            return Err(format!(
                "projection class mismatch for {source:?}: expected {expected}, got {actual}"
            ));
        }
    }
    Ok(())
}

/// The explain-vertical receipt: the derived routing facts `--explain` renders
/// must match a hand table, and must be the SAME facts the route selector
/// reads.
///
/// Each row pins the whole plan — class, pushdown path, every ladder rung,
/// and the boundary consumer — in the canonical spelling the CLI renderer also
/// uses. A fact that drifts from the route it describes fails here before it
/// can reach the CLI.
pub(crate) fn assert_explain_plan() -> Result<(), String> {
    // (program, expected canonical explain label). `whole` is the eager
    // whole-document consumption class.
    const TABLE: &[(&str, &str)] = &[
        // The bare identity: the source-preserving round-trip lane.
        (
            ".",
            "identity=1 modifies=0 whole=1 morsel_path=1 input_family=0 class=Subtree \
             pushdown=[] rungs=rl:0 m:1 consumer=none",
        ),
        // The element count of a static container: served by the whole-document
        // route now (the element-stream count fold was deleted with the
        // element-stream result kind).
        (
            "[.catalog[]] | length",
            "identity=0 modifies=0 whole=1 morsel_path=0 input_family=0 class=Structure \
             pushdown=.catalog rungs=rl:0 m:1 consumer=Collect",
        ),
        // The select-projection union case: the count is whole-document now.
        // The BACKWARD lattice class stays Fields[id].
        (
            "[.catalog[] | select(.id > 35990) | .name] | length",
            "identity=0 modifies=0 whole=1 morsel_path=0 input_family=0 class=Fields[id] \
             pushdown=.catalog rungs=rl:0 m:1 consumer=Collect",
        ),
        // A fold over a static container: the whole-document floor serves it.
        // `whole=1` — the fold visits every element its generator yields.
        (
            "reduce .catalog[].id as $i (0; . + $i)",
            "identity=0 modifies=0 whole=1 morsel_path=0 input_family=0 class=Fields[id] \
             pushdown=[] rungs=rl:0 m:1 consumer=Fold",
        ),
        // A fan-out over a WHOLE element, consumer Residual.
        (
            ".catalog[].name",
            "identity=0 modifies=0 whole=1 morsel_path=0 input_family=0 class=Fields[name] \
             pushdown=.catalog rungs=rl:0 m:1 consumer=Residual",
        ),
        // A shallow answer: no rung below the morsel lane applies.
        (
            ".catalog | keys",
            "identity=0 modifies=0 whole=0 morsel_path=0 input_family=0 class=Subtree \
             pushdown=.catalog rungs=rl:0 m:1 consumer=none",
        ),
        // A collect whose BODY is a bound-handle escape: `$x` reads the whole
        // element, so no per-element shape row admits it and the route is the
        // whole document. The boundary consumer is named `Binding` — the `as`
        // binder whose SOURCE holds the boundary.
        (
            "[.catalog[] as $x | $x]",
            "identity=0 modifies=0 whole=1 morsel_path=0 input_family=0 class=Subtree \
             pushdown=[] rungs=rl:0 m:1 consumer=Binding",
        ),
        // A plain located static path: the whole chain pushes down, no rung
        // below the morsel lane applies.
        (
            ".catalog[0].id",
            "identity=0 modifies=0 whole=0 morsel_path=1 input_family=0 class=Subtree \
             pushdown=.catalog[0].id rungs=rl:0 m:1 consumer=none",
        ),
    ];

    let mut mismatches = Vec::new();
    for (source, expected) in TABLE {
        let resources = resources();
        let program = program_for(source, &resources)?;
        let actual = explain_label(&program.explain());
        if actual != *expected {
            mismatches.push(format!("{source:?} -> {actual}"));
        }
    }
    if !mismatches.is_empty() {
        return Err(format!("explain plan mismatches:\n{}", mismatches.join("\n")));
    }
    Ok(())
}

/// Plan-serialization receipt : the routing-facts plan round-trips
/// byte-stable, the deserialized record equals the freshly derived one, and
/// the same source compiles to identical plan bytes on a second compile. The
/// plan is the `--explain` plan — the facts read through the route selector's
/// accessors — so byte stability is the drift check: a serialized plan that
/// does not equal a fresh derivation cannot describe the same route.
pub(crate) fn assert_plan_serialization() -> Result<(), String> {
    const SOURCES: &[&str] = &[
        ".",
        ".[]",
        "[.catalog[]] | length",
        "[.catalog[] | select(.id > 35990) | .name] | length",
        "reduce .catalog[].id as $i (0; . + $i)",
        ".catalog[].name",
        ".catalog | keys",
        "[.catalog[] as $x | $x]",
        ".catalog[0].id",
    ];
    for source in SOURCES {
        let res = resources();
        let program = program_for(source, &res)?;
        let record = program.plan_record();
        let bytes = program.serialize_plan();
        // The plan is byte-stable across compiles of the same source.
        let res2 = resources();
        let program2 = program_for(source, &res2)?;
        if program2.serialize_plan() != bytes {
            return Err(format!(
                "plan bytes are not byte-stable for {source:?}: a second compile of the same \
                 source produced different plan bytes"
            ));
        }
        // The deserialized record equals the freshly derived plan — a loaded
        // plan cannot drift from the route it documents.
        let decoded = jqf_engine::PlanRecord::deserialize(&bytes)
            .map_err(|error| format!("plan decode failed for {source:?}: {error:?}"))?;
        if decoded != record {
            return Err(format!("plan round-trip drifted for {source:?}: decoded != derived"));
        }
        // Re-serializing the decoded record reproduces the exact bytes.
        if decoded.serialize() != bytes {
            return Err(format!(
                "plan re-serialize drifted for {source:?}: decoded.serialize() != original bytes"
            ));
        }
    }
    Ok(())
}

/// Renders one program's projection class as the stable receipt spelling
/// (`Structure`, `Fields[a,b]`, `Subtree`).
fn projection_class_str(class: &ProjectionClass<'_>) -> String {
    match class {
        ProjectionClass::Structure => "Structure".to_owned(),
        ProjectionClass::Subtree => "Subtree".to_owned(),
        ProjectionClass::Fields(fields) => {
            let mut label = "Fields[".to_owned();
            for (position, name) in fields.names().iter().enumerate() {
                if position > 0 {
                    label.push(',');
                }
                label.push_str(name);
            }
            label.push(']');
            label
        }
    }
}

pub(crate) fn projection_class_label(program: &CompiledProgram) -> String {
    projection_class_str(&program.projection_class())
}

/// One static codec step in the canonical receipt spelling of a pushed-down
/// path: `.key`, `[0]`, or the slice form `[start:end]` with either bound open.
fn render_explain_steps(steps: &[StaticForwardStep<'_>]) -> String {
    let mut out = String::new();
    for step in steps {
        match step {
            StaticForwardStep::ObjectKey(key) => {
                out.push('.');
                out.push_str(key);
            }
            StaticForwardStep::ArrayIndex(index) => {
                let _ = write!(out, "[{index}]");
            }
            StaticForwardStep::ArrayRange { start, end } => {
                out.push('[');
                if let Some(start) = start {
                    let _ = write!(out, "{start}");
                }
                out.push(':');
                if let Some(end) = end {
                    let _ = write!(out, "{end}");
                }
                out.push(']');
            }
        }
    }
    if out.is_empty() {
        out.push_str("[]");
    }
    out
}

/// The canonical compact spelling of an [`ExplainPlan`] the explain receipt
/// compares against its hand table. Every fact `--explain` renders appears
/// here exactly once, so a fact that drifts from the route it describes fails
/// the battery before it can reach the CLI.
fn explain_label(plan: &ExplainPlan<'_>) -> String {
    let rungs = &plan.rungs;
    let mut out = format!(
        "identity={} modifies={} whole={} morsel_path={} input_family={} class={} pushdown={}",
        bool_as_int(plan.identity),
        bool_as_int(plan.modifies),
        bool_as_int(plan.consumes_whole_document),
        bool_as_int(plan.morsel_static_path),
        bool_as_int(plan.uses_input_family),
        projection_class_str(&plan.projection_class),
        render_explain_steps(&plan.pushdown),
    );
    let _ = write!(
        out,
        " rungs=rl:{} m:{}",
        bool_as_int(rungs.range_locate),
        bool_as_int(rungs.morsel),
    );
    let consumer = match plan.boundary_consumer {
        Some(consumer) => format!("{consumer:?}"),
        None => "none".to_owned(),
    };
    let _ = write!(out, " consumer={consumer}");
    out
}

const fn bool_as_int(value: bool) -> u8 {
    if value { 1 } else { 0 }
}

/// The one document every projection-vs-floor oracle pair runs over: a catalog
/// shaped like the fixture (an object of records under `.catalog`,
/// with a sibling key the projected lanes never read).
const PROJECTION_ORACLE_INPUT: &[u8] = br#"{"catalog":[{"id":0,"name":"item-0","tags":["a","b"]},{"id":1,"name":"item-1","tags":["c"]},{"id":2,"name":"item-2","tags":[]}],"meta":{"n":3}}"#;

/// The projection-vs-floor oracle harness.
///
/// One mechanism: run a (program, document) pair through a DESIGNATED route and
/// through the FLOOR, and require byte-identical publication and an identical
/// outcome class. It is the standing net under the classifier — receipts prove
/// the classifier AGREES with a hand table, only force-routing proves the
/// projection is SOUND.
///
/// The access inventory is TWO slots plus the record stream: slot 0 Whole →
/// `CompleteDocument`, slot 1 Exact → `Located`. The `Designated` arm drives
/// the same selector the CLI does — the bare-slice publish rung when the
/// program is range-locate eligible, else the program's own located /
/// whole-document requirement — over the pair table's lanes, and the FLOOR
/// forces `[.][0] | (P)` so every lane compares a specialized route against
/// the materialized whole document. The pair table and the comparison do not
/// change.
pub(crate) fn assert_projection_floor_oracle(
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
) -> Result<(), String> {
    // Every lane of the classification table, plus a run that ABORTS (the exit
    // half of "byte+exit compare" must be exercised, not just the byte half).
    const PAIRS: &[&str] = &[
        ".",
        ".catalog[].name",
        ".catalog[].id",
        ".catalog[0].id",
        ".catalog[]",
        "[.catalog[]] | length",
        ".catalog | map(.name) | length",
        "reduce .catalog[] as $x (0; . + 1)",
        "reduce .catalog[].id as $i (0; . + $i)",
        "[.catalog[] | select(.id > 1)] | length",
        "[.catalog[] | select(.id > 1)]",
        "[.catalog[] | length]",
        "[.catalog[] as $x | $x.id]",
        // Aborts on the first element: `.id` is a number, so `.id.x` raises.
        ".catalog[].id.x",
        // Range projection: count, collect, and projected publish over a range path.
        ".catalog[1:3] | length",
        "[.catalog[1:3][]] | length",
        "[.catalog[1:3][].name] | length",
        "[.catalog[1:3][].name]",
        ".catalog[1:3][].name",
    ];

    let mut streamed_lanes = 0_u32;
    for program in PAIRS {
        let designated = oracle_run(OracleRoute::Designated, catalog, format, dialect, program)?;
        let floor = oracle_run(OracleRoute::Floor, catalog, format, dialect, program)?;
        if floor.result != AccessResultKind::CompleteDocument {
            return Err(format!(
                "floor route for {program:?} did not take the whole-document route: {:?}",
                floor.result
            ));
        }
        if designated.bytes != floor.bytes
            || designated.completed != floor.completed
            || designated.failure_class != floor.failure_class
        {
            return Err(format!(
                "projection-vs-floor divergence for {program:?}: designated=({:?}, completed={}, class={:?}) floor=({:?}, completed={}, class={:?})",
                designated.bytes,
                designated.completed,
                designated.failure_class,
                floor.bytes,
                floor.completed,
                floor.failure_class,
            ));
        }
        if designated.result != AccessResultKind::CompleteDocument || designated.range_located {
            streamed_lanes += 1;
        }
    }
    // The harness is only worth anything if at least one lane actually left the
    // floor: `designated ≡ floor` must be a real comparison, not floor ≡ floor.
    if streamed_lanes == 0 {
        return Err("projection-vs-floor oracle drove no fast lane".to_owned());
    }
    Ok(())
}

/// Drives one (program, document) pair through one oracle route.
pub(crate) fn oracle_run(
    route: OracleRoute,
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    program_source: &str,
) -> Result<OracleOutcome, String> {
    oracle_run_over(route, catalog, format, dialect, program_source, PROJECTION_ORACLE_INPUT)
}
