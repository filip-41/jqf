//! The one compile-time job a finished program commits to.
//!
//! [`Shortcut`] is a closed sum: a new fast path is a new arm, not another
//! `Option` field on [`super::CompiledProgram`]. [`commit`] picks at most one
//! document oracle (count, element, keys, type, has, any/all, min/max), then
//! range-locate, then identity. Everything else is [`Shortcut::None`] — the
//! residual graph. Accessors on the compiled program match this enum; they do
//! not keep a parallel bag of options.
//!
//! Finish also commits [`Access`] (what the codec decodes). Empty prefix is
//! Whole. Count and element graph-run skip `demand.path`, not the split prefix.

use alloc::vec::Vec;

use jqf_data::{CountDemand, CountStep, ElementDemand};

use crate::AnyAllDemand;
use crate::HasDemand;
use crate::MinMaxDemand;
use crate::analysis::element::ConstructFields;
use crate::program::Program;

/// The job compile picked. `None` means run the graph.
#[derive(Debug)]
pub(crate) enum Shortcut {
    /// No document or pass-through job — the residual graph.
    None,
    /// `length` / `PATH | length` / collect-count.
    Count(CountDemand),
    /// `keys` / `PATH | keys`. Empty path is the document root.
    Keys(Vec<CountStep>),
    /// `type` / `PATH | type`. Empty path is the document root.
    Type(Vec<CountStep>),
    /// Fan-out / fold / collected construct answered by element visit.
    Element {
        /// The document-core demand the consumer iterates.
        demand: ElementDemand,
        /// Static-key object construction riding a fan-out, when present.
        construct: Option<ConstructFields>,
        /// `[FAN-OUT]`: collect every probe into one published array.
        collect: bool,
    },
    /// Bare slice publish `PATH[a:b]`.
    RangeLocate,
    /// Bare identity `.` — output is input.
    Identity,
    /// `has(LITERAL)` / `PATH | has(LITERAL)`.
    Has(HasDemand),
    /// `any` / `all` over a static element path.
    AnyAll(AnyAllDemand),
    /// `min` / `max` / `min_by` / `max_by` of a numeric array (or numeric probe).
    MinMax(MinMaxDemand),
}

/// Picks the one job the recognizers named. Later arms are not stored.
pub(crate) fn commit(
    program: &Program,
    count: Option<CountDemand>,
    element: Option<ElementDemand>,
    construct: Option<ConstructFields>,
    collect: bool,
) -> Shortcut {
    let nodes = program.nodes();
    let root = program.root();
    let keys = crate::analysis::count::keys_demand(nodes, root);
    let type_path = crate::analysis::count::type_demand_path(nodes, root);
    let has = crate::analysis::count::has_demand(nodes, root);
    let any_all = crate::analysis::any_all::any_all_demand(nodes, root);
    let min_max = crate::analysis::min_max::min_max_demand(nodes, root);
    let range_locate = program.range_locate();
    let identity = program.is_identity();
    let oracles = u8::from(count.is_some())
        + u8::from(element.is_some())
        + u8::from(keys.is_some())
        + u8::from(type_path.is_some())
        + u8::from(has.is_some())
        + u8::from(any_all.is_some())
        + u8::from(min_max.is_some())
        + u8::from(range_locate)
        + u8::from(identity);
    debug_assert!(
        oracles <= 1,
        "shortcut recognizers named {oracles} jobs; the sum is closed"
    );
    if let Some(demand) = count {
        return Shortcut::Count(demand);
    }
    if let Some(demand) = element {
        return Shortcut::Element {
            demand,
            construct,
            collect,
        };
    }
    if let Some(path) = keys {
        return Shortcut::Keys(path);
    }
    if let Some(path) = type_path {
        return Shortcut::Type(path);
    }
    if let Some(demand) = has {
        return Shortcut::Has(demand);
    }
    if let Some(demand) = any_all {
        return Shortcut::AnyAll(demand);
    }
    if let Some(demand) = min_max {
        return Shortcut::MinMax(demand);
    }
    if range_locate {
        return Shortcut::RangeLocate;
    }
    if identity {
        return Shortcut::Identity;
    }
    Shortcut::None
}

/// What the codec is asked to decode. Empty prefix is Whole: Exact has no steps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Access {
    /// Whole (`.`, `.[]`, bare `length`/`keys`/`type`/`has`/`min`, empty-path element).
    Whole,
    /// A nonempty static Key/Index path. YAML Exact names the child in the full graph;
    /// JSON Exact republishes that child as the document root.
    Exact,
}

/// Access the codec must satisfy for this shortcut. Empty prefix is Whole.
///
/// Element Exact of a nonempty PATH is committed here. `access.rs`
/// `pack_exact` attaches the element demand the way count does; Exact miss
/// rebind reads that packed hint, not a second `element_demand()` walk.
/// Nonempty range-locate packs Exact; empty `.[a:b]` stays Whole.
pub(crate) fn commit_access(program: &Program, shortcut: &Shortcut) -> Access {
    match shortcut {
        Shortcut::Count(demand) if !demand.path.is_empty() => Access::Exact,
        Shortcut::Keys(path) if !path.is_empty() => Access::Exact,
        Shortcut::Type(path) if !path.is_empty() => Access::Exact,
        Shortcut::Has(demand) if !demand.path.is_empty() => Access::Exact,
        // `all(.users[]; .id)` has a nonempty demand path on a whole-document
        // split. Exact has no steps then, so Whole; the oracle walks
        // `demand.path` from the root.
        Shortcut::AnyAll(demand) if !demand.path.is_empty() && !program.split().is_whole_document() => Access::Exact,
        Shortcut::MinMax(demand) if !demand.path.is_empty() => Access::Exact,
        Shortcut::Element { demand, .. } if !demand.path.is_empty() => Access::Exact,
        Shortcut::RangeLocate | Shortcut::None if !program.split().is_whole_document() => Access::Exact,
        _ => Access::Whole,
    }
}
