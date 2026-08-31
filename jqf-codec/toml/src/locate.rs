//! Exact-path steps and the empty-path root observation.
//!
//! Non-empty paths go through [`crate::walk`]. This module owns the step vocabulary, signed-bound slice helpers, and
//! the empty-path root table.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::ValueKind;

use crate::grammar::{TableTree, Tree};

/// The owned exact-path vocabulary is core's: every pushed-down route of every codec copies the requirement's
/// [`jqf_codec_core::PortableStep`]s the same way, for the same session lifetime.
pub(crate) use jqf_codec_core::{OwnedStep, own_steps};

/// The located exact-path observation over the parsed tree.
#[derive(Debug)]
pub(crate) enum Located<'tree> {
    /// A located scalar, array, or inline-table value.
    Value(&'tree Tree),
    /// A located standard table (the root, a child table, or an array-of-tables element).
    Table(&'tree TableTree),
    /// A located array-of-tables container.
    ArrayOfTables(&'tree [TableTree]),
    /// The step at which navigation stopped: no member or position exists.
    Missing { step: usize },
}

/// Resolves one exact path over the parsed tree, starting at the root table.
///
/// v1 resolves only the empty path (the root table). Non-empty paths are answered by the byte walk; calling this helper
/// with steps is a contract violation.
pub(crate) fn locate<'tree>(root: &'tree TableTree, steps: &[OwnedStep]) -> Result<Located<'tree>, CodecError> {
    if !steps.is_empty() {
        return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "TOML tree locate accepts only the empty exact path",
        }));
    }
    Ok(Located::Table(root))
}

/// Resolves a range pair against the observed length: a strictly-negative bound counts from the end (saturating at the
/// head), a non-negative bound clamps to `len`, and `start >= end` selects nothing.
pub(crate) fn resolve_range(len: usize, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let start = match start {
        None => 0,
        Some(value) => resolve_bound(Some(value), len),
    };
    let end = match end {
        None => len,
        Some(value) => resolve_bound(Some(value), len),
    };
    // `start >= end` selects nothing; the pair is reported as-is and the caller materializes an empty slice.
    (start, end)
}

/// One slice bound resolved by its AUTHORED sign: a strictly-negative bound counts from the end of a container of `len`
/// elements (saturating at the head), a non-negative bound clamps to the far edge.
pub(crate) fn resolve_bound(bound: Option<i64>, len: usize) -> usize {
    match bound {
        None => 0,
        Some(value) if value >= 0 => usize::try_from(value).unwrap_or(usize::MAX).min(len),
        Some(value) => len.saturating_sub(usize::try_from(value.unsigned_abs()).unwrap_or(usize::MAX)),
    }
}

/// The semantic kind of a parsed value, for a mismatch observation. Shared with the byte walker's value navigation.
pub(crate) fn tree_kind(value: &Tree) -> ValueKind {
    match value {
        Tree::String(_) => ValueKind::String,
        Tree::Integer { .. } | Tree::Float(..) | Tree::Decimal(..) => ValueKind::Number,
        Tree::Bool(..) => ValueKind::Bool,
        Tree::LocalDate(..) => ValueKind::LocalDate,
        Tree::LocalTime(..) => ValueKind::LocalTime,
        Tree::LocalDateTime(..) => ValueKind::LocalDateTime,
        Tree::OffsetDateTime(..) => ValueKind::OffsetDateTime,
        Tree::Array { .. } => ValueKind::Array,
        Tree::InlineTable { .. } => ValueKind::Object,
        Tree::Commented { value, .. } => tree_kind(value),
    }
}
