//! The attached-fact helpers: role matching and payload materialization.
//!
//! The `facts` builtin in the registry is their only consumer beside the executor's accessor step: a fact role is a
//! string law and a payload is a value law, so their home is semantics.

use jqf_data::{Array, FactPayloadView, Integer, Number, ObjectBuilder, Value};

use crate::error::EngineRunError;

/// Whether a fact role serves an accessor selector.
///
/// A fact role is either exactly the selector (a plain semantic role such as `name`) or the codec's namespaced form
/// `<format>.<semantic>@<revision>`
/// (such as `xml.name@1`), which serves the selector named by its semantic segment. A document carries one codec's
/// facts, so namespaced roles from different formats cannot collide on the same node.
/// The semantic-segment fact-role law: a fact whose role names a semantic segment equal to `selector` is that
/// selector's fact (`xml.name@1` serves `.@name`, a plain `name` role does too).
pub fn accessor_matches_fact(role: &str, selector: &str) -> bool {
    if role == selector {
        return true;
    }
    let core = role.rsplit_once('.').map_or(role, |(_, rest)| rest);
    let semantic = core.split_once('@').map_or(core, |(semantic, _)| semantic);
    semantic == selector
}

/// Materializes one attached-fact payload into an owned semantic value.
///
/// The payload is jqf's portable, format-neutral fact vocabulary ([`jqf_data::FactPayloadView`]), which maps directly
/// onto [`Value`]: scalars become their value, `List`/`Map` build `Array`/`Object`, and opaque bytes project as
/// uninterpreted `Bytes`.
pub fn materialize_fact_payload(payload: FactPayloadView<'_>) -> Result<Value, EngineRunError> {
    match payload {
        FactPayloadView::Null => Ok(Value::Null),
        FactPayloadView::Bool(value) => Ok(Value::Bool(value)),
        FactPayloadView::Integer(text) => {
            let integer =
                Integer::parse(text).map_err(|_| EngineRunError::internal_contract("fact integer payload"))?;
            Number::try_integer_unaccounted(integer)
                .map(Value::Number)
                .map_err(|_| EngineRunError::allocation_failure())
        }
        FactPayloadView::Decimal { coefficient, scale } => {
            let integer =
                Integer::parse(coefficient).map_err(|_| EngineRunError::internal_contract("fact decimal payload"))?;
            let decimal = jqf_data::Decimal::from_parts(integer, scale)
                .map_err(|_| EngineRunError::internal_contract("fact decimal payload"))?;
            Number::try_decimal_unaccounted(decimal)
                .map(Value::Number)
                .map_err(|_| EngineRunError::allocation_failure())
        }
        FactPayloadView::Text(text) => Value::try_string(text).map_err(|_| EngineRunError::allocation_failure()),
        FactPayloadView::Bytes(bytes) => Value::try_bytes(bytes).map_err(|_| EngineRunError::allocation_failure()),
        FactPayloadView::OpaqueBytes(bytes) => {
            Value::try_bytes(bytes).map_err(|_| EngineRunError::allocation_failure())
        }
        FactPayloadView::List(list) => {
            let mut out = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
            for item in list.iter() {
                let value = materialize_fact_payload(item)?;
                out.try_push(value).map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Value::Array(out))
        }
        FactPayloadView::Map(map) => {
            let mut builder = ObjectBuilder::new();
            for (key, value) in map.iter() {
                let value = materialize_fact_payload(value)?;
                let key = jqf_data::ObjectKey::try_from_str(key).map_err(|_| EngineRunError::allocation_failure())?;
                builder
                    .try_insert_last(key, value)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            builder
                .try_finish()
                .map(Value::Object)
                .map_err(|_| EngineRunError::allocation_failure())
        }
    }
}
