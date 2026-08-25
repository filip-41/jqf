//! The jqf truth law: `false` and `null` are falsy, everything else truthy.
//!
//! One job: answer whether one engine result is truthy under the exact rule — only `false` and `null` are falsy;
//! every other value, including `0`, `""`, `[]`, and `{}`, is truthy. The check reads a borrowed value: a located node
//! through its authoritative document view, or an owned value directly. It is payload-transparent, matching the rest of
//! the model — a tagged value's truth is its payload's truth.
//!
//! Negative space: it produces no value, drives no cardinality, and charges no ledger. The engine's `select` reads this
//! once that engine crate lands.

use jqf_data::{DataError, ScalarView, Value, ValueKind, ValueView};

use crate::codec_result::EngineResult;

/// The payload kind and the truth law from one view open.
///
/// A walk that both classifies and selects the same child must not open the located view twice.
pub fn kind_and_truthy(value: &EngineResult<'_>) -> Result<(ValueKind, bool), DataError> {
    match value {
        EngineResult::Owned(owned) => {
            let kind = owned.kind();
            Ok((kind, owned_is_truthy(owned)))
        }
        EngineResult::Located(located) => {
            let view = located.product().document().payload_view(located.node())?;
            let kind = view.kind()?;
            let truthy = located_is_truthy(&view)?;
            Ok((kind, truthy))
        }
    }
}

/// Whether `value` is truthy under the one jqf truth law.
///
/// Only `false` and `null` are falsy. A located value is read through its document view (payload-transparent: a located
/// tagged bool reports the payload's bool); an owned value is inspected directly.
///
/// Intended for the SDK's exit-status publication (`-e`) once the engine crate lands: it reads the same law at encode
/// time, and a second spelling would let the two diverge.
///
/// # Errors
///
/// Returns [`DataError`] only when reading the located value's borrowed view over an otherwise valid authoritative
/// document fails — a broken internal contract the caller surfaces as such.
pub fn is_truthy(value: &EngineResult<'_>) -> Result<bool, DataError> {
    match value {
        EngineResult::Owned(owned) => Ok(owned_is_truthy(owned)),
        EngineResult::Located(located) => {
            // The payload-transparent view: a tag LAYER (CBOR's kindless wrapper node) is descended before the node is
            // asked what it is, so a tagged bool/null reports its payload's truth here exactly as the owned arm reports
            // `Value::Tagged`'s.
            let view = located.product().document().payload_view(located.node())?;
            located_is_truthy(&view)
        }
    }
}

/// The truth of an owned value: `false`/`null` falsy, tags transparent.
fn owned_is_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Tagged { payload, .. } => owned_is_truthy(payload),
        _ => true,
    }
}

/// Truth, empty-array, and raw-text facts opened from one view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicationFacts {
    /// [`is_truthy`]
    pub truthy: bool,
    /// [`is_empty_array`]
    pub empty_array: bool,
    /// [`super::rawtext::is_raw_text`]
    pub raw_text: bool,
}

/// One located-view walk for the three encode-report facts.
pub fn publication_facts(value: &EngineResult<'_>) -> Result<PublicationFacts, DataError> {
    match value {
        EngineResult::Owned(owned) => Ok(PublicationFacts {
            truthy: owned_is_truthy(owned),
            empty_array: owned_is_empty_array(owned),
            raw_text: super::rawtext::owned_is_raw_text(owned),
        }),
        EngineResult::Located(located) => {
            let view = located.product().document().payload_view(located.node())?;
            Ok(PublicationFacts {
                truthy: located_is_truthy(&view)?,
                empty_array: located_is_empty_array(&view)?,
                raw_text: super::rawtext::located_is_raw_text(&view)?,
            })
        }
    }
}

/// Whether `value` is an EMPTY ARRAY under the payload-transparent view.
///
/// The intended consumer is the CLI's `--diff` exit law once the engine crate lands: the fixed diff program emits ONE
/// array of change records, and an empty array is the "the two documents are semantically equal" verdict.
/// Mirrors [`is_truthy`]'s borrowed read with the same tag transparency; see that function's doc for the full
/// statement.
///
/// # Errors
///
/// Returns [`DataError`] only when reading the located value's borrowed view over an otherwise valid authoritative
/// document fails — a broken internal contract the caller surfaces as such.
pub fn is_empty_array(value: &EngineResult<'_>) -> Result<bool, DataError> {
    match value {
        EngineResult::Owned(owned) => Ok(owned_is_empty_array(owned)),
        EngineResult::Located(located) => {
            let view = located.product().document().payload_view(located.node())?;
            located_is_empty_array(&view)
        }
    }
}

/// The emptiness of an owned value: only an empty array is an empty array, tags transparent.
fn owned_is_empty_array(value: &Value) -> bool {
    match value {
        Value::Array(array) => array.is_empty(),
        Value::Tagged { payload, .. } => owned_is_empty_array(payload),
        _ => false,
    }
}

/// The emptiness of a located value through its document view.
fn located_is_empty_array(view: &ValueView<'_, '_>) -> Result<bool, DataError> {
    if view.kind()? != ValueKind::Array {
        return Ok(false);
    }
    match view.array()? {
        Some(array) => Ok(array.is_empty()),
        // An array-kind node always projects an array view; a divergence is an invalid document, reported as such.
        None => Err(DataError::InvalidDocument),
    }
}

/// The truth of a located value through its document view. `kind()` and `scalar()` are payload-transparent over the
/// node's own semantic, so the caller must hand in the PAYLOAD view (a tag layer descended) — a tagged bool/null then
/// reports its payload's category here.
fn located_is_truthy(view: &ValueView<'_, '_>) -> Result<bool, DataError> {
    match view.kind()? {
        ValueKind::Null => Ok(false),
        ValueKind::Bool => match view.scalar()? {
            Some(ScalarView::Bool(value)) => Ok(value),
            // A node whose kind is Bool always projects a bool scalar; a divergence is an invalid document, reported as
            // such.
            _ => Err(DataError::InvalidDocument),
        },
        _ => Ok(true),
    }
}
