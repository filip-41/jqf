//! The raw-text fact: whether one engine result's ROOT is text the `-r` raw arm prints verbatim.
//!
//! One job: answer whether one engine result is a TEXT-FAMILY scalar — a string, a byte string, or one of the
//! temporal spellings — under the `-r` raw arm law (a ROOT string prints its bytes verbatim, unquoted and uncoloured;
//! every other item prints normally). jqf's raw arm extends the string law to the projected-text scalars exactly as the
//! JSON encoder's verbatim arm does, so the fact names that whole family.
//!
//! The CLI's colour rendering reads this fact per item: a raw-printed root text's bytes ARE the string — they are the
//! value, not a rendering — so colour must never touch them. The check reads a borrowed value: a located node through
//! its authoritative document view (payload-transparent, like [`is_truthy`]: a tagged text reports its payload's
//! family), or an owned value directly.
//!
//! Negative space: it produces no value, drives no cardinality, and charges no ledger.

use jqf_data::{DataError, ScalarView, Value, ValueView};

use crate::codec_result::EngineResult;

/// Whether `value`'s root is a text-family scalar under the raw arm law.
///
/// `true` for a string, byte string, or temporal scalar; `false` for every container, `null`, a bool, a number, and a
/// tag layer whose payload is not text. A located value is read through its document view; an owned value is inspected
/// directly.
///
/// Public because the SDK's item report carries the fact at encode time (the CLI's colour rendering reads it); a second
/// spelling here would let the two diverge.
///
/// # Errors
///
/// Returns [`DataError`] only when reading the located value's borrowed view over an otherwise valid authoritative
/// document fails — a broken internal contract the caller surfaces as such.
pub fn is_raw_text(value: &EngineResult<'_>) -> Result<bool, DataError> {
    match value {
        EngineResult::Owned(owned) => Ok(owned_is_raw_text(owned)),
        EngineResult::Located(located) => {
            let view = located.product().document().payload_view(located.node())?;
            located_is_raw_text(&view)
        }
    }
}

/// The raw-text law over an owned value: the text-family variants, tags transparent.
pub(crate) fn owned_is_raw_text(value: &Value) -> bool {
    match value {
        Value::String(_) | Value::Bytes(_) => true,
        value if value.is_temporal() => true,
        Value::Tagged { payload, .. } => owned_is_raw_text(payload),
        _ => false,
    }
}

/// The raw-text law over a located value's payload view: the text-family scalar projections. A container, `null`, a
/// bool, or a number answers false.
pub(crate) fn located_is_raw_text(view: &ValueView<'_, '_>) -> Result<bool, DataError> {
    Ok(matches!(
        view.scalar()?,
        Some(
            ScalarView::String(_)
                | ScalarView::Bytes(_)
                | ScalarView::LocalDate(_)
                | ScalarView::LocalTime(_)
                | ScalarView::LocalDateTime(_)
                | ScalarView::OffsetDateTime(_)
        )
    ))
}
