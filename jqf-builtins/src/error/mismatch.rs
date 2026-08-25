//! The mismatch dial's cell taxonomy and per-cell resolution law.
//!
//! Each [`MismatchCell`] is one frozen site where the reference produces a VALUE rather than an error — the dial's
//! unit of reporting.
//!
//! The law per policy position:
//! - `Lenient` matches the reference byte for byte — no new stderr byte, no
//!   new exit code, no new answer (still the compat corpus's obligation).
//! - `Warn` answers the reference's value and exit code, and counts the event into the
//!   request's per-cell report (`ResourceContext::note_mismatch`); the CLI prints the capped, aggregated report once
//!   after the run, through the informational diagnostic channel.
//! - `Strict` turns the event into a raise on the SEMANTIC channel
//!   ([`EngineRunError::MismatchRaised`], exit class 5).
//!
//! Intent suppression is a structural property: a cell inside an `Alternative` (`//`) operand or a `try` body, or on a
//! `?`-marked step, fires no event under any position. The `Alternative`/`try` half is maintained by the evaluators as
//! a suppression depth on the request context (the graph machine's `AlternativeLeft`/`Try` frame lifecycle and the
//! path-mode evaluator's operand drives); the `?` half is per-step and arrives as the `suppressed` argument from the
//! stage walk. The match below is exhaustive over the policy, so a new policy mode fails to COMPILE at every site until
//! its behavior lands; and the cell enum is crate-private with every variant constructed exactly at its site, so the
//! frozen table's wiring is compiler-checked too (an unwired cell is a dead-code warning).

use jqf_resource::ResourceContext;
use jqf_resource::policy::MismatchPolicy;

use crate::error::EngineRunError;

/// One frozen mismatch cell: a site where the reference answers a VALUE, which the dial may report (warn) or raise on
/// (strict).
///
/// The variants are the frozen table's rows in order; each doc comment states the value the reference answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MismatchCell {
    /// `{"a":1} | .b` → `null` — the object key the query assumed is absent.
    MissingKey,
    /// `[1,2] | .[9]` → `null` (also `.[2]`, `.[-5]`, `[] | .[0]`, and the dynamic `.[$x]` out-of-range form).
    IndexOutOfRange,
    /// `null | .a` → `null` (also the `null | .a.b` chain).
    FieldOnNull,
    /// `null | .[0]` / `null | .[1:2]` → `null` (any index or slice into null; iteration into null RAISES and is
    /// deliberately not a cell).
    IndexOrSliceOnNull,
    /// `null + 1` / `1 + null` → `1` (also `null + null` → `null` and the string/array/object operand forms; `* - /
    /// %` with null RAISE and are not cells).
    NullAdditiveIdentity,
    /// `getpath(["a","b"])` → `null` (missing, out-of-range, or null-holding step; a wrong-kind NON-null step raises
    /// and is not a cell), and the jqf `json_pointer` extension's miss → `[]` (its RFC 6901 read law).
    PathMiss,
    /// `1 < "a"` → `true` — an ordering operator over different kind bands (null < false < true < numbers < strings
    /// < arrays < objects). Fires at PROGRAM comparison nodes only; builtin internals (`sort`, `min`, …) never
    /// evaluate a comparison node and are immune by design.
    CrossKindOrdering,
    /// `null | .a = 1` → `{"a":1}`: assignment, update, `+=`, or `setpath` through null or a missing member CREATES
    /// the container.
    AssignmentVivifies,
    /// `{"b":1} | del(.a)` → `{"b":1}`: a delete component that names nothing is a silent no-op (also `delpaths`).
    DeletePathMiss,
    /// `[1,2] | .[0:9]` → `[1,2]`: an authored slice bound past the container's length clamps instead of null or
    /// erroring (also `"abc" | .[1:9]` → `"bc"`).
    SliceClamped,
    /// `null | length` → `0`, `null | reverse` → `[]`: null answered as the empty container. The zero-length family
    /// (`0`, `""`, `{}`, `[]`) is the reference's DEFINED length law, ruled not a cell.
    NullAsEmptyContainer,
}

impl MismatchCell {
    /// The frozen table's row index — the report counter's index and the strict raise's cell identity.
    pub const fn index(self) -> usize {
        match self {
            MismatchCell::MissingKey => 0,
            MismatchCell::IndexOutOfRange => 1,
            MismatchCell::FieldOnNull => 2,
            MismatchCell::IndexOrSliceOnNull => 3,
            MismatchCell::NullAdditiveIdentity => 4,
            MismatchCell::PathMiss => 5,
            MismatchCell::CrossKindOrdering => 6,
            MismatchCell::AssignmentVivifies => 7,
            MismatchCell::DeletePathMiss => 8,
            MismatchCell::SliceClamped => 9,
            MismatchCell::NullAsEmptyContainer => 10,
        }
    }
}

/// Notes a cross-kind ordering comparison at a PROGRAM comparison node, when the operator is an ordering op and the
/// operands' kind bands differ (`1 < "a"` → `true`).
///
/// Builtin internals never call this — `sort`, `min`, `max`, `unique`, `group_by`, `bsearch` and friends compare
/// through the comparator laws directly, never through a program comparison node, so they stay immune by construction
/// and a mixed-type `sort` keeps working under any policy.
///
/// Returns `Err(EngineRunError::MismatchRaised)` under strict (the cell raises at the comparison node); the callers are
/// fallible and propagate it.
pub fn note_cross_kind_ordering(
    op: crate::semantics::binary::BinaryKind,
    left: &jqf_data::Value,
    right: &jqf_data::Value,
    resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    if matches!(
        op,
        crate::semantics::binary::BinaryKind::Less
            | crate::semantics::binary::BinaryKind::LessEqual
            | crate::semantics::binary::BinaryKind::Greater
            | crate::semantics::binary::BinaryKind::GreaterEqual
    ) && left.kind() != right.kind()
    {
        resolve_at(resources, MismatchCell::CrossKindOrdering, false, ())?;
    }
    Ok(())
}

/// Resolves one mismatch cell under the request's policy, returning the lenient answer untouched — the dial's single
/// choke point.
///
/// - `Lenient`: the answer passes through byte-for-byte (still the
///   compat corpus's obligation under the default).
/// - `Warn`: the event is counted into the request's per-cell report and the
///   lenient answer passes through (the reference's value and exit code).
/// - `Strict`: `Err(EngineRunError::MismatchRaised)` — the event becomes a
///   semantic-channel raise with exit class 5.
///
/// `suppressed` is the SITE's own intent marker (a `?`-marked step's flag; false where no step exists). The
/// `Alternative`/`try` half of the suppression law is read from the request context's suppression depth, so a cell
/// inside an intent marker never fires under any position. `cell` is the site's own identity: it is consumed on warn
/// and strict, and passing it everywhere is what makes the frozen table's wiring compiler-checked.
pub fn resolve_at<T>(
    resources: &ResourceContext<'_>,
    cell: MismatchCell,
    suppressed: bool,
    lenient: T,
) -> Result<T, EngineRunError> {
    if suppressed || resources.mismatch_suppressed() {
        return Ok(lenient);
    }
    match resources.mismatch_policy() {
        MismatchPolicy::Lenient => Ok(lenient),
        MismatchPolicy::Warn => {
            resources.note_mismatch(cell.index());
            Ok(lenient)
        }
        MismatchPolicy::Strict => Err(EngineRunError::MismatchRaised {
            cell: u16::try_from(cell.index()).unwrap_or(u16::MAX),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::policy::MISMATCH_CELL_COUNT;

    #[test]
    fn the_cell_table_is_eleven_rows_in_frozen_order() {
        let cells = [
            MismatchCell::MissingKey,
            MismatchCell::IndexOutOfRange,
            MismatchCell::FieldOnNull,
            MismatchCell::IndexOrSliceOnNull,
            MismatchCell::NullAdditiveIdentity,
            MismatchCell::PathMiss,
            MismatchCell::CrossKindOrdering,
            MismatchCell::AssignmentVivifies,
            MismatchCell::DeletePathMiss,
            MismatchCell::SliceClamped,
            MismatchCell::NullAsEmptyContainer,
        ];
        assert_eq!(cells.len(), MISMATCH_CELL_COUNT);
        for (index, cell) in cells.iter().enumerate() {
            assert_eq!(cell.index(), index);
        }
    }
}
