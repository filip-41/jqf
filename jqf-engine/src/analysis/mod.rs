//! The facts over a lowered [`crate::program`] arena.
//!
//! Two families live here. The ARENA facts are the ones a [`crate::program::Program`]
//! stores as fields — the codec-pushdown split ([`demand`]), the projection
//! class ([`projection`]), the correlated-scan table ([`join`]), the
//! partial-sort table ([`partial`]) and the range locate ([`locate`]). The
//! EXECUTOR-STRATEGY facts are the ones derived on the way to a route
//! ([`count`], [`element`], `lazy`, `prune`, `tailcall`, [`topk`]).
//!
//! The arena-fact types are fields of the arena. Nothing on the builtins
//! side of the boundary reads them.
//!
//! Three facts select no route by themselves. [`join`]'s correlated-scan
//! table names the `select` scans that may be driven from a sorted keyed index
//! instead of element by element: it changes no output, no route, and no
//! requirement — only how many children the executor visits.
//!
//! [`partial`]'s partial-sort table names the `sort`/`sort_by` calls whose sole
//! consumer is a provably-constant-bound head/tail slice, so the executor may
//! answer them with the bounded-heap `top_k` partial sort instead of a full
//! sort. It too changes no output, no route, and no requirement — only how the
//! executor computes the answer (the recognizer door); [`topk`] keeps its
//! executor-facing reader.
//!
//! [`count`]'s demand table names the count-class rows: the container path and
//! per-item probe a document-core consumer answers from a lazy document's span
//! skeleton. Like [`join`] and [`partial`] it selects no route by itself; the
//! SDK drive reads the demand and publishes the consumer's answer when the
//! document can prove it, and the ordinary route stands when it cannot.

// The arena facts.
mod demand;
mod join;
mod locate;
mod partial;
mod projection;

// The executor-strategy facts.
pub(crate) mod count;
pub(crate) mod element;
mod lazy;
pub(crate) mod path_steps;
mod prune;
mod tailcall;
mod topk;

pub use demand::{BoundaryConsumer, ElementBoundary};
pub(crate) use demand::{PushdownSplit, analyze, fuse_with_callables};
pub(crate) use join::{AntiJoinScan, CorrelatedScan, anti_joins, correlated_scans};
pub(crate) use lazy::consumes_whole_document;
pub(crate) use locate::is_range_locate;
pub use partial::TopKConsumer;
pub(crate) use partial::{PartialSort, partial_sorts};
pub use projection::ProjectionClass;
pub(crate) use projection::classify;
pub(crate) use prune::{PruneNode, prune_tree, pulled_record_prune_tree};
pub(crate) use tailcall::mark_tail_calls;
pub(crate) use topk::topk_count;

/// The v1 range-admission law of the DOCUMENT-side demand consumers (the
/// count and element-iteration rows): a normalized range is admitted only when
/// neither bound is strictly negative.
///
/// Their resolution (`jqf-data`'s `resolve_range`) is a pure clamp over
/// non-negative readings — a negative bound needs the container's length,
/// which their early-stop skeleton walk gives up. The len-relative wrap for a
/// negative bound is the codec's two-pass range scan, which these tables
/// do not consume, so a strictly-negative
/// bound keeps declining this row.
pub(crate) fn admits_document_range(range: &(Option<i64>, Option<i64>)) -> bool {
    range.0.is_none_or(|value| value >= 0) && range.1.is_none_or(|value| value >= 0)
}
