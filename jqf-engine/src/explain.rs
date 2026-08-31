//! The explain plan: a compiled program's routing facts in one borrowable view.
//!
//! This is `--explain`'s engine side. The plan is a bundle of DERIVED facts —
//! route-ladder eligibility, the projection demand class, the pushed-down
//! prefix, and the named element boundary and its consumer — read off the
//! compiled program through exactly the accessors the route selector reads. It
//! changes no routing and lowers no requirement; it exists to be rendered by
//! the CLI and asserted by receipts. A fact that disagrees with the route the
//! selector actually takes is a classifier bug, which is why the receipts pin
//! both sides.

use alloc::vec::Vec;

use crate::analysis::{BoundaryConsumer, ProjectionClass};
use crate::codec_requirement::StaticForwardStep;
use crate::compile::CompiledProgram;

/// The explain rung row: every rung's eligibility in one struct.
///
/// Each boolean is the compiled program's own predicate — nothing here is
/// derived a second time, so the plan cannot drift from the routing it
/// documents. The row names no route by itself; it is the receipt surface the
/// plan serializes and the smoke batteries assert on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each boolean is one route rung of the explain ladder; grouping them would hide the ladder"
)]
pub struct RungEligibility {
    /// The bare-slice publish `PATH[a:b]`.
    pub range_locate: bool,
    /// Whether the record morsel / adjacent-value parallel lane may take this
    /// program (every overload is `Effects::Pure`).
    pub morsel: bool,
}

/// One borrowable bundle of a compiled program's routing facts.
///
/// Everything here is read-only and derived on demand; constructing it charges
/// one small allocation (the pushed-down prefix vector) and no arena bytes.
#[derive(Clone, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each boolean is one routing fact of the explain plan; grouping them would hide the facts"
)]
pub struct ExplainPlan<'program> {
    /// Whether the program is the bare identity filter `.`.
    pub identity: bool,
    /// Whether the program contains any assignment (`=`/`|=` family) node.
    pub modifies: bool,
    /// Whether the root evaluation consumes the entire input document.
    pub consumes_whole_document: bool,
    /// Whether the program is the MORSEL-static-path class (a bare static
    /// Key/Index chain, identity included).
    pub morsel_static_path: bool,
    /// Whether the program reads the input family (`input`/`inputs`).
    pub uses_input_family: bool,
    /// The backward demand lattice class of one streamed element.
    pub projection_class: ProjectionClass<'program>,
    /// The pushed-down static prefix as codec path steps; empty is the root
    /// selection (the whole-document arm).
    pub pushdown: Vec<StaticForwardStep<'program>>,
    /// Every rung's eligibility.
    pub rungs: RungEligibility,
    /// What consumes the named boundary's elements, when one is named.
    pub boundary_consumer: Option<BoundaryConsumer>,
    /// How many rows of the CLOSED partial-sort table this program's graph
    /// matches — the recognizer's verdict
    /// on the `sort | .[0:k]` spelling family, which the executor answers
    /// with the bounded-heap partial sort instead of a full sort.
    pub topk_rows: usize,
    /// Whether finish cached a count-table row for this program.
    pub count_route: bool,
    /// Whether finish cached an element-boundary row.
    pub element_route: bool,
    /// Whether finish cached a keys-publish row.
    pub keys_route: bool,
    /// Whether finish cached a type-class row.
    pub type_route: bool,
    /// Whether the compiled program reads the ~inputs resident cursor.
    pub uses_inputs_cursor: bool,
}

/// Reads the explain plan off a compiled program.
#[must_use]
pub fn plan(compiled: &CompiledProgram) -> ExplainPlan<'_> {
    ExplainPlan {
        identity: compiled.is_identity(),
        modifies: compiled.modifies(),
        consumes_whole_document: compiled.consumes_whole_document(),
        morsel_static_path: compiled.is_morsel_static_path(),
        uses_input_family: compiled.uses_input_family(),
        projection_class: compiled.projection_class(),
        pushdown: compiled.pushdown_path(),
        rungs: RungEligibility {
            range_locate: compiled.range_locate_eligible(),
            morsel: compiled.is_morsel_eligible(),
        },
        boundary_consumer: compiled.element_boundary_consumer(),
        topk_rows: compiled.topk_rows(),
        count_route: compiled.count_demand().is_some(),
        element_route: compiled.element_demand().is_some(),
        keys_route: compiled.keys_demand().is_some(),
        type_route: compiled.type_demand(),
        uses_inputs_cursor: compiled.uses_inputs_cursor(),
    }
}
