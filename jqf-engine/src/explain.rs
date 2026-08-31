//! The explain plan: a compiled program's routing facts in one borrowable view.
//!
//! This is `--explain`'s engine side. The plan is a bundle of DERIVED facts —
//! the committed [`ShortcutKind`], route-ladder eligibility, the projection
//! demand class, the pushed-down prefix, and the named element boundary and
//! its consumer — read off the compiled program through exactly the accessors
//! the route selector reads. It changes no routing and lowers no requirement;
//! it exists to be rendered by the CLI and asserted by receipts. A fact that
//! disagrees with the route the selector actually takes is a classifier bug,
//! which is why the receipts pin both sides.

use alloc::vec::Vec;

use crate::analysis::{BoundaryConsumer, ProjectionClass};
use crate::codec_requirement::StaticForwardStep;
use crate::compile::{CompiledProgram, Shortcut};

/// The closed job finish committed, without the demand payload.
///
/// This is the explain/plan snapshot of [`Shortcut`]: a new fast path is a new
/// arm, not another boolean on the plan. `None` means the residual graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutKind {
    /// No document or pass-through job — the residual graph.
    None,
    /// `length` / `PATH | length` / collect-count.
    Count,
    /// `keys` / `PATH | keys`.
    Keys,
    /// `type` / `PATH | type`.
    Type,
    /// Fan-out / fold / collected construct answered by element visit.
    Element,
    /// Bare slice publish `PATH[a:b]`.
    RangeLocate,
    /// Bare identity `.` — output is input.
    Identity,
    /// `has(LITERAL)` / `PATH | has(LITERAL)`.
    Has,
    /// `any` / `all` over a static element path.
    AnyAll,
    /// `min` / `max` / `min_by` / `max_by` of a numeric array (or numeric probe).
    MinMax,
}

impl ShortcutKind {
    /// The `--explain` spelling of this job.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Count => "count",
            Self::Keys => "keys",
            Self::Type => "type",
            Self::Element => "element",
            Self::RangeLocate => "range_locate",
            Self::Identity => "identity",
            Self::Has => "has",
            Self::AnyAll => "any_all",
            Self::MinMax => "min_max",
        }
    }

    fn from_shortcut(shortcut: &Shortcut) -> Self {
        match shortcut {
            Shortcut::None => Self::None,
            Shortcut::Count(_) => Self::Count,
            Shortcut::Keys(_) => Self::Keys,
            Shortcut::Type(_) => Self::Type,
            Shortcut::Element { .. } => Self::Element,
            Shortcut::RangeLocate => Self::RangeLocate,
            Shortcut::Identity => Self::Identity,
            Shortcut::Has(_) => Self::Has,
            Shortcut::AnyAll(_) => Self::AnyAll,
            Shortcut::MinMax(_) => Self::MinMax,
        }
    }
}

/// The explain rung row: morsel-lane eligibility.
///
/// The boolean is the compiled program's own predicate — nothing here is
/// derived a second time, so the plan cannot drift from the routing it
/// documents. Identity echo and range-locate span cut are the shortcut
/// (and [`crate::HostIo`]), not rungs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RungEligibility {
    /// Whether the record morsel / adjacent-value parallel lane may take this
    /// program (every overload is `Effects::Pure`).
    pub morsel: bool,
}

/// One borrowable bundle of a compiled program's routing facts.
///
/// Packed facts (`consumes_whole_document`, `pushdown`) are read off finish.
/// `projection_class` re-walks because the class borrows interned field
/// names and cannot be stored without a second copy.
#[derive(Clone, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each boolean is one routing fact of the explain plan; grouping them would hide the facts"
)]
pub struct ExplainPlan<'program> {
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
    /// The one job finish committed. `None` is the residual graph.
    pub shortcut: ShortcutKind,
    /// Whether the compiled program reads the ~inputs resident cursor.
    pub uses_inputs_cursor: bool,
}

/// Reads the explain plan off a compiled program.
#[must_use]
pub fn plan(compiled: &CompiledProgram) -> ExplainPlan<'_> {
    ExplainPlan {
        modifies: compiled.modifies(),
        consumes_whole_document: compiled.consumes_whole_document(),
        morsel_static_path: compiled.is_morsel_static_path(),
        uses_input_family: compiled.uses_input_family(),
        projection_class: compiled.projection_class(),
        pushdown: compiled.pushdown_path(),
        rungs: RungEligibility {
            morsel: compiled.is_morsel_eligible(),
        },
        boundary_consumer: compiled.element_boundary_consumer(),
        topk_rows: compiled.topk_rows(),
        shortcut: ShortcutKind::from_shortcut(compiled.shortcut()),
        uses_inputs_cursor: compiled.uses_inputs_cursor(),
    }
}
