//! The containment relation: whether one value is "inside" another.
//!
//! One job: own the recursive relation `contains`/`inside` are, plus the top-level kind rule that decides whether the
//! relation applies at all. It is a value law and not a text one, which is why it lives beside
//! [`crate::semantics::order`] rather than in [`crate::semantics::text`] — strings are only one of its four cases.
//!
//! The relation, per level:
//!
//! * two OBJECTS — every key of the right operand is present in the left AND
//!   its values are in the relation, so `{}` is inside everything;
//! * two ARRAYS — every element of the right operand is in the relation with
//!   SOME element of the left, which is an existential and not a positional test: `[1,2] | contains([2,1])` is `true`,
//!   `contains([1,1,1])` is `true`, and `[]` is inside everything;
//! * two STRINGS — substring, so `""` is inside everything;
//! * anything else — value equality.
//!
//! # The kind rule is TOP-LEVEL only, and booleans are two kinds
//!
//! Two facts that look like one and are not. At the top level a kind mismatch RAISES; one level down it merely answers
//! `false`:
//!
//! ```text
//! "a" | contains(1)           → string ("a") and number (1) cannot …
//! {"a":"x"} | contains({a:1}) → false
//! [["a"]] | contains([[1]])   → false
//! ```
//!
//! And the value model has no single boolean kind — `true` and `false` are separate ones — so the top-level rule
//! refuses a boolean pair that disagrees while accepting a number pair that does:
//!
//! ```text
//! true | contains(false) → boolean (true) and boolean (false) cannot …
//! 1 | contains(2)        → false
//! null | contains(null)  → true
//! ```
//!
//! [`checkable`] is that rule and nothing else; the caller raises, because the message names both operands and
//! rendering them is [`crate::error::message`]'s job. One level down the same pairings reach [`walk`]'s equality arm
//! and answer `false`, which is why the recursion never consults [`checkable`].
//!
//! The depth ceiling answers [`TooDeep`] rather than a message for the same reason the order's does — the caller
//! names the operation.
//!
//! Negative space: it renders no message and raises no error — it answers a boolean or the depth marker; it owns no
//! argument fan-out, which is the executor's answer drive; and it owns no equality of its own, reading
//! [`crate::semantics::order`]'s so `[1] | contains([1.0])` cannot disagree with `1 == 1.0`.

use jqf_data::Value;

use super::depth::{self, Guarded, TooDeep};
use super::order::semantic_eq;
use crate::semantics::owned_kind;

/// Whether the two operands' TOP-LEVEL kinds admit a containment check.
///
/// `false` is the caller's cue to raise the `cannot have their containment checked` refusal. Booleans are compared by
/// VALUE and not by kind word, because the value model spells `true` and `false` as two distinct kinds and the rule is
/// a kind-equality test.
pub fn checkable(left: &Value, right: &Value) -> bool {
    match (left.untagged(), right.untagged()) {
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (left, right) => owned_kind(left) == owned_kind(right),
    }
}

/// Whether `right` is contained in `left`.
///
/// # Errors
///
/// Returns [`TooDeep`] once the recursion passes the containment row's ceiling.
pub fn contains(left: &Value, right: &Value) -> Result<bool, TooDeep> {
    walk(left, right, 0)
}

/// One level of the containment recursion.
///
/// `depth` is the number of CONTAINERS already entered, so the outermost check is 0 and the boundary reads as `> limit`
/// — the same accounting the total order uses, which is what makes the two ceilings coincide observably.
fn walk(left: &Value, right: &Value, depth: usize) -> Result<bool, TooDeep> {
    if depth > depth::limit(Guarded::Containment) {
        return Err(TooDeep);
    }
    match (left.untagged(), right.untagged()) {
        (Value::Object(left), Value::Object(right)) => {
            for entry in right {
                let Some(found) = left.get(entry.key()) else {
                    return Ok(false);
                };
                if !walk(found, entry.value(), depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        // An EXISTENTIAL over the left operand, so a repeated right element is satisfied by the same left element every
        // time.
        (Value::Array(left), Value::Array(right)) => {
            for wanted in right {
                if !any_contains(left.iter(), wanted, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::String(left), Value::String(right)) => Ok(left.as_str().contains(right.as_str())),
        // Every remaining pairing — two scalars, or two kinds that disagree one level down from the top — is value
        // equality, which answers `false` for a mismatch instead of raising. It cannot recurse: the three container
        // pairings are taken above, so an operand reaching here is either a scalar or a kind mismatch the order settles
        // by RANK.
        (left, right) => semantic_eq(left, right),
    }
}

/// Whether any candidate contains `wanted`.
fn any_contains<'value>(
    candidates: impl Iterator<Item = &'value Value>,
    wanted: &Value,
    depth: usize,
) -> Result<bool, TooDeep> {
    for candidate in candidates {
        if walk(candidate, wanted, depth)? {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{checkable, contains};
    use jqf_data::{Array, Number, ObjectBuilder, ObjectKey, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger: a fixture's payload is charged at its own construction, so no operand builds
    /// without an account.
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    fn number(spelling: &str) -> Value {
        Value::Number(Number::try_json_literal(spelling).expect("literal"))
    }

    fn text(value: &str) -> Value {
        Value::try_string(value).expect("fixture string")
    }

    fn array(elements: impl IntoIterator<Item = Value>) -> Value {
        let _resources = ledger();
        let mut built = Array::try_new().expect("fixture array");
        for element in elements {
            built.try_push(element).expect("fixture element");
        }
        Value::Array(built)
    }

    fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
        let mut builder = ObjectBuilder::new();
        for (key, value) in entries {
            builder
                .try_insert_last(ObjectKey::try_from_str(key).expect("key"), value)
                .expect("insert");
        }
        Value::Object(builder.try_finish().expect("fixture object"))
    }

    fn holds(left: &Value, right: &Value) -> bool {
        contains(left, right).expect("the fixtures nest nowhere near the ceiling")
    }

    /// The top-level kind rule spells booleans as TWO kinds.
    ///
    /// The value model has no single boolean kind, so `true | contains(false)` is the unpaired-kinds refusal and not
    /// `false` — the one row a kind-word comparison gets wrong. Everything else pairs by kind alone.
    #[test]
    fn the_top_level_kind_rule_spells_booleans_as_two_kinds() {
        assert!(checkable(&Value::Bool(true), &Value::Bool(true)));
        assert!(!checkable(&Value::Bool(true), &Value::Bool(false)));
        assert!(checkable(&text("a"), &text("b")));
        assert!(!checkable(&text("1"), &number("1")));
        assert!(checkable(&array([]), &array([])));
    }

    /// Containment is per-kind: subset, existential, substring, equality.
    ///
    /// The array row is the one an index-wise walk gets wrong — the relation is an EXISTENTIAL over the left operand,
    /// so `[1] | contains([1,1])` is `true`, satisfied twice by the same element. The object row recurses on matched
    /// keys; the string row is a substring; and a scalar pair is the total order's equality, which crosses spellings.
    #[test]
    fn containment_is_subset_existential_substring_and_equality() {
        assert!(holds(
            &object([("a", number("1")), ("b", number("2"))]),
            &object([("a", number("1"))])
        ));
        assert!(!holds(&object([("a", number("1"))]), &object([("a", number("2"))])));
        assert!(holds(&array([number("1")]), &array([number("1"), number("1")])));
        assert!(holds(&text("abcd"), &text("bc")));
        assert!(holds(&number("1"), &number("1.0")));
    }

    /// A kind mismatch one level down answers `false` instead of refusing.
    ///
    /// The top-level rule is TOP-LEVEL only: `[1] | contains(["1"])` is `false`, because the pair reaches the
    /// recursion's equality arm rather than [`checkable`]. A nested pairing that DOES agree still recurses, so the
    /// difference is not a blanket `false` for containers.
    #[test]
    fn a_kind_mismatch_one_level_down_answers_false() {
        assert!(!holds(&array([number("1")]), &array([text("1")])));
        assert!(!holds(
            &array([array([number("1")])]),
            &array([array([number("1"), number("2")])])
        ));
        assert!(holds(
            &array([array([number("1"), number("2")])]),
            &array([array([number("1")])])
        ));
    }
}
