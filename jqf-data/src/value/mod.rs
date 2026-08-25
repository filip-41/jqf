//! Owned semantic values: null, bool, number, string, array, object, and the rest.
//!
//! [`Value`] has no `PartialEq`, `Hash`, or `Ord`. The caller that compares values owns that comparison — a
//! substitute here would disagree on floats, tags, objects, and dates.
//!
//! Heap payloads allocate through `shared.rs`. Construction can fail if the allocator refuses; it does not charge a
//! request ledger. Clone is a refcount bump. A later write copies first, so a shared clone looks the same as a
//! separately built twin. Sharing shows only through the identity witnesses: [`Value::shares_allocation_with`], or the
//! containers' [`Array::allocation_key`] / [`Object::allocation_key`].

mod array;
pub(crate) mod object;
mod shared;
mod tagged;

pub use array::{Array, resolve_index};
pub use object::{Object, ObjectBuilder, ObjectEntry, ObjectKey};
pub use shared::Shared;
pub use tagged::{TagError, TagId};

use core::fmt;

use crate::{LocalDate, LocalDateTime, LocalTime, Number, OffsetDateTime, ValueKind};

/// Owned semantic value.
///
/// The caller that compares values owns the comparison — see the module documentation for why there is no
/// `PartialEq`, `Hash`, or `Ord`. Clone is a refcount bump on each heap payload.
#[derive(Debug)]
pub enum Value {
    /// The null value.
    Null,
    /// A boolean.
    Bool(bool),
    /// An exact or binary numeric value.
    Number(Number),
    /// UTF-8 text.
    String(Shared<str>),
    /// An uninterpreted byte string.
    Bytes(Shared<[u8]>),
    /// A local calendar date.
    LocalDate(LocalDate),
    /// A local wall-clock time.
    LocalTime(LocalTime),
    /// A local date and time.
    LocalDateTime(LocalDateTime),
    /// A date and time with an offset.
    OffsetDateTime(OffsetDateTime),
    /// A non-core tag wrapping its payload.
    Tagged {
        /// Exact tag text, such as `!money`.
        tag: TagId,
        /// The wrapped value.
        payload: Shared<Value>,
    },
    /// An ordered sequence.
    Array(Array),
    /// Unique keys, first-insertion order.
    Object(Object),
}

impl Value {
    /// Category of the payload. Looks through tag wrappers.
    #[must_use]
    pub fn kind(&self) -> ValueKind {
        match self.untagged() {
            Self::Null => ValueKind::Null,
            Self::Bool(_) => ValueKind::Bool,
            Self::Number(_) => ValueKind::Number,
            Self::String(_) => ValueKind::String,
            Self::Bytes(_) => ValueKind::Bytes,
            Self::LocalDate(_) => ValueKind::LocalDate,
            Self::LocalTime(_) => ValueKind::LocalTime,
            Self::LocalDateTime(_) => ValueKind::LocalDateTime,
            Self::OffsetDateTime(_) => ValueKind::OffsetDateTime,
            Self::Array(_) => ValueKind::Array,
            Self::Object(_) => ValueKind::Object,
            Self::Tagged { .. } => unreachable!("untagged removes every tag layer"),
        }
    }

    /// Same as [`ValueKind::is_temporal`] on [`Self::kind`].
    #[must_use]
    pub fn is_temporal(&self) -> bool {
        self.kind().is_temporal()
    }

    /// The value under every tag wrapper.
    #[must_use]
    pub fn untagged(&self) -> &Self {
        let mut value = self;
        while let Self::Tagged { payload, .. } = value {
            value = payload;
        }
        value
    }

    /// Whether these two handles name the same heap allocation.
    ///
    /// This is identity, not equality. Two separately built twins return `false`. The check is O(1) at any depth.
    ///
    /// Tags are stripped first, so two wrappers around the same payload return `true` even if the tags differ. Compare
    /// the tags yourself if that matters.
    ///
    /// Numbers, strings, and byte strings share allocations the same way arrays and objects do. Null, bool, and dates
    /// have no allocation to share.
    #[must_use]
    pub fn shares_allocation_with(&self, other: &Self) -> bool {
        match (self.untagged(), other.untagged()) {
            (Self::Array(left), Self::Array(right)) => left.shares_storage_with(right),
            (Self::Object(left), Self::Object(right)) => left.shares_storage_with(right),
            (Self::Number(left), Self::Number(right)) => left.shares_allocation_with(right),
            (Self::String(left), Self::String(right)) => left.shares_allocation_with(right),
            (Self::Bytes(left), Self::Bytes(right)) => left.shares_allocation_with(right),
            _ => false,
        }
    }

    /// Outermost tag, if this value is tagged.
    #[must_use]
    pub fn tag(&self) -> Option<&TagId> {
        match self {
            Self::Tagged { tag, .. } => Some(tag),
            _ => None,
        }
    }

    /// Shared UTF-8 text. Fails if the allocator refuses.
    pub fn try_string(text: &str) -> Result<Self, ValueAllocationError> {
        Shared::try_from_str(text).map(Self::String)
    }

    /// Shared byte string. Fails if the allocator refuses.
    pub fn try_bytes(bytes: &[u8]) -> Result<Self, ValueAllocationError> {
        Shared::try_from_slice(bytes).map(Self::Bytes)
    }

    /// Wrap `payload` in a non-core tag. Fails if the allocator refuses.
    pub fn try_tagged(tag: TagId, payload: Self) -> Result<Self, ValueAllocationError> {
        Ok(Self::Tagged {
            tag,
            payload: Shared::try_new(payload)?,
        })
    }
}

impl Clone for Value {
    /// Refcount bump on each heap payload. Copies nothing.
    ///
    /// A later write copies first. Sharing is visible only through the identity witnesses named in the module
    /// documentation.
    fn clone(&self) -> Self {
        match self {
            Self::Null => Self::Null,
            Self::Bool(value) => Self::Bool(*value),
            Self::Number(value) => Self::Number(value.clone()),
            Self::String(value) => Self::String(value.clone_shared()),
            Self::Bytes(value) => Self::Bytes(value.clone_shared()),
            Self::LocalDate(value) => Self::LocalDate(*value),
            Self::LocalTime(value) => Self::LocalTime(value.clone()),
            Self::LocalDateTime(value) => Self::LocalDateTime(value.clone()),
            Self::OffsetDateTime(value) => Self::OffsetDateTime(value.clone()),
            Self::Tagged { tag, payload } => Self::Tagged {
                tag: tag.clone(),
                payload: payload.clone_shared(),
            },
            Self::Array(value) => Self::Array(value.clone_shared()),
            Self::Object(value) => Self::Object(value.clone_shared()),
        }
    }
}

/// The allocator refused a value payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ValueAllocationError;

impl fmt::Display for ValueAllocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("semantic value allocation failed")
    }
}

impl core::error::Error for ValueAllocationError {}

#[cfg(test)]
mod tests {
    use super::{Array, Object, ObjectBuilder, ObjectEntry, ObjectKey, TagId, Value};
    use crate::{FractionalSecond, LocalDate, LocalDateTime, LocalTime, Number, OffsetDateTime, UtcOffset};
    use alloc::{string::String, vec::Vec};

    #[test]
    fn value_clones_without_a_ledger() {
        let elements = alloc::vec![Value::Bool(true), Value::Null];
        let original = Value::Array(Array::try_from_vec(elements).expect("array fixture"));
        let copy = original.clone();
        assert!(
            original.shares_allocation_with(&copy),
            "clone shares, it does not deep-copy"
        );
    }

    /// The sharing rows of the number blindness matrix: every spelling class the matrix pins — spanning both integer
    /// arms, both decimal shapes, a float, and the retained negative zero the machine arm refuses.
    const SHARING_MATRIX: &[&str] = &[
        "0",
        "-0",
        "1",
        "-1",
        "9007199254740993",
        "9223372036854775807",
        "-9223372036854775808",
        "99999999999999999999",
        "12345678901234567890123456789",
        "1.5",
        "-0.0625",
        "1e3",
        "-0E5",
        "1.0e400",
    ];

    fn literal(spelling: &str) -> Number {
        Number::try_json_literal(spelling).expect("literal parses")
    }

    /// The projections the sharing matrix compares — category, integer digits, decimal coefficient/scale, float
    /// presence, and sign. A shared clone must agree with an INDEPENDENTLY parsed twin on all of them; that agreement
    /// is what "representation-blind" means for the sharing property, since no public API can see the `Arc` at all.
    fn projections(number: &Number) -> (String, Option<String>, Option<String>, bool, bool) {
        let category = alloc::format!("{:?}", number.category());
        let integer = number.as_integer().map(|value| String::from(value.as_str()));
        let decimal = number
            .as_decimal()
            .map(|value| alloc::format!("{}e{}", value.coefficient().as_str(), -value.scale()));
        let float = number.as_float().is_some();
        (category, integer, decimal, float, number.is_negative())
    }

    /// Cloning a `Value::Number` SHARES its representation instead of allocating a fresh one, and the shared clone is
    /// indistinguishable from an independently constructed twin through every projection a consumer has.
    #[test]
    fn a_cloned_number_shares_its_representation_and_stays_projection_identical() {
        for spelling in SHARING_MATRIX {
            let source = literal(spelling);
            let value = Value::Number(source.clone());
            let Value::Number(cloned) = value.clone() else {
                panic!("clone changed the variant for {spelling}");
            };
            // A boxed number shares its representation; an inline machine integer is copied by value and has no
            // representation to share — both are the arms' truthful answers, and the projection-identity assertion
            // below covers the machine arm.
            assert!(
                cloned.shares_allocation_with(&source)
                    || (source.as_machine().is_some() && cloned.as_machine().is_some()),
                "clone neither shared nor stayed inline for {spelling}"
            );
            let independent = literal(spelling);
            assert!(
                !cloned.shares_allocation_with(&independent),
                "two independent parses must NOT share for {spelling}"
            );
            assert_eq!(
                projections(&cloned),
                projections(&independent),
                "projection mismatch for {spelling}"
            );
        }
    }

    /// Clone of every heap variant shares with its source and matches a separately built twin.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the sharing matrix pins every heap-backed variant in one walk, each with its shared-with-source and twin-must-not-share pair"
    )]
    fn a_cloned_value_shares_every_heap_payload_and_stays_projection_identical() {
        // String.
        let source = Value::try_string("shared text").expect("string");
        let Value::String(cloned) = source.clone() else {
            panic!("string clone changed the variant");
        };
        let Value::String(original) = &source else {
            panic!("string fixture changed the variant");
        };
        assert!(
            cloned.shares_allocation_with(original),
            "a cloned string must retain its source allocation"
        );
        let independent = Value::try_string("shared text").expect("twin string");
        let Value::String(independent) = &independent else {
            panic!("string twin changed the variant");
        };
        assert!(
            !cloned.shares_allocation_with(independent),
            "two independent strings must NOT share"
        );
        assert_eq!(cloned.as_str(), independent.as_str());

        // Bytes.
        let source = Value::try_bytes(b"shared bytes").expect("bytes");
        let Value::Bytes(cloned) = source.clone() else {
            panic!("bytes clone changed the variant");
        };
        let Value::Bytes(original) = &source else {
            panic!("bytes fixture changed the variant");
        };
        assert!(
            cloned.shares_allocation_with(original),
            "a cloned byte string must retain its source allocation"
        );
        let independent = Value::try_bytes(b"shared bytes").expect("twin bytes");
        let Value::Bytes(independent) = &independent else {
            panic!("bytes twin changed the variant");
        };
        assert!(
            !cloned.shares_allocation_with(independent),
            "two independent byte strings must NOT share"
        );
        assert_eq!(cloned.as_slice(), independent.as_slice());

        // Array elements.
        let source = Value::Array(array_of(&["a", "b"]));
        let Value::Array(cloned) = source.clone() else {
            panic!("array clone changed the variant");
        };
        let Value::Array(original) = &source else {
            panic!("array fixture changed the variant");
        };
        assert!(
            cloned.shares_storage_with(original),
            "a cloned array must retain its source element spine"
        );
        let independent = array_of(&["a", "b"]);
        assert!(
            !cloned.shares_storage_with(&independent),
            "two independent arrays must NOT share"
        );
        assert_eq!(render_array(&cloned), render_array(&independent));

        // Object entries.
        let source = Value::Object(object_of(&["a", "b"]));
        let Value::Object(cloned) = source.clone() else {
            panic!("object clone changed the variant");
        };
        let Value::Object(original) = &source else {
            panic!("object fixture changed the variant");
        };
        assert!(
            cloned.shares_storage_with(original),
            "a cloned object must retain its source entry table"
        );
        let independent = object_of(&["a", "b"]);
        assert!(
            !cloned.shares_storage_with(&independent),
            "two independent objects must NOT share"
        );
        assert_eq!(render_object(&cloned), render_object(&independent));

        // Tag payload.
        let tag = TagId::try_new_unaccounted("!m").expect("tag");
        let inner = Value::try_string("payload").expect("payload");
        let source = Value::try_tagged(tag, inner).expect("tagged");
        let Value::Tagged {
            tag: cloned_tag,
            payload: cloned,
        } = source.clone()
        else {
            panic!("tagged clone changed the variant");
        };
        let Value::Tagged {
            tag: original_tag,
            payload: original,
        } = &source
        else {
            panic!("tagged fixture changed the variant");
        };
        assert!(
            cloned.shares_allocation_with(original),
            "a cloned tag must retain its source payload allocation"
        );
        assert_eq!(cloned_tag.as_str(), original_tag.as_str());
        let Value::String(text) = &*cloned else {
            panic!("tagged clone lost its payload");
        };
        assert_eq!(text.as_str(), "payload");
    }

    /// A number reached through a container still shares: the element and entry allocations are retained whole, so
    /// every `Number` inside them is the SAME `Number`, not a re-parsed twin.
    #[test]
    fn numbers_nested_in_shared_containers_stay_shared_through_the_clone() {
        for spelling in SHARING_MATRIX {
            let source = literal(spelling);

            let elements = alloc::vec![Value::Number(source.clone())];
            let array = Value::Array(Array::try_from_vec(elements).expect("array"));
            let Value::Array(cloned) = array.clone() else {
                panic!("array clone changed the variant for {spelling}");
            };
            let Some(Value::Number(element)) = cloned.get(0) else {
                panic!("array clone lost its element for {spelling}");
            };
            assert!(
                element.shares_allocation_with(&source)
                    || (source.as_machine().is_some() && element.as_machine().is_some()),
                "array element neither shared nor stayed inline for {spelling}"
            );

            let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("n").expect("key"),
                    Value::Number(source.clone()),
                )
                .expect("insert");
            let object = Value::Object(builder.try_finish().expect("object"));
            let Value::Object(cloned) = object.clone() else {
                panic!("object clone changed the variant for {spelling}");
            };
            let Some(Value::Number(member)) = cloned.get_index(0).map(ObjectEntry::value) else {
                panic!("object clone lost its member for {spelling}");
            };
            assert!(
                member.shares_allocation_with(&source)
                    || (source.as_machine().is_some() && member.as_machine().is_some()),
                "object member neither shared nor stayed inline for {spelling}"
            );

            let tagged = Value::try_tagged(
                TagId::try_new_unaccounted("!m").expect("tag"),
                Value::Number(source.clone()),
            )
            .expect("tagged");
            let Value::Tagged { payload, .. } = tagged.clone() else {
                panic!("tagged clone changed the variant for {spelling}");
            };
            let Value::Number(payload) = &*payload else {
                panic!("tagged clone lost its payload for {spelling}");
            };
            assert!(
                payload.shares_allocation_with(&source)
                    || (source.as_machine().is_some() && payload.as_machine().is_some()),
                "tagged payload neither shared nor stayed inline for {spelling}"
            );
        }
    }

    /// Constructors can fail, but they do not charge the request ledger. A refused allocation is tested where the
    /// counting allocator lives (`jqf-resource`). This test only checks that construction succeeds.
    #[test]
    fn allocating_constructors_succeed_without_charging() {
        assert_eq!(render(&Value::try_string("text").expect("text allocates")), "text");
    }

    /// The unshared clone arms — null, booleans, and the temporal values — reserve nothing and name no allocation
    /// `shares_allocation_with` can see: a clone is a plain copy (`LocalDate`) or a fractional-second refcount bump
    /// (`LocalTime` and its composites). The test pins what they CAN guarantee — the clone preserves the value
    /// exactly, with no ledger touched.
    #[test]
    fn the_inline_clone_arms_copy_by_value_and_preserve_the_value() {
        let date = LocalDate::new(2024, 2, 29).expect("leap day");
        let time = LocalTime::new(23, 59, 58, FractionalSecond::default()).expect("wall time");
        let local = LocalDateTime {
            date,
            time: time.clone(),
        };
        let offset = OffsetDateTime {
            local: local.clone(),
            offset: UtcOffset::UnknownLocalOffset,
        };

        let cloned = Value::Null.clone();
        assert!(matches!(cloned, Value::Null));

        let Value::Bool(flag) = Value::Bool(true).clone() else {
            panic!("bool clone changed the variant");
        };
        assert!(flag);

        let Value::LocalDate(copied) = Value::LocalDate(date).clone() else {
            panic!("date clone changed the variant");
        };
        assert_eq!(copied, date);

        let Value::LocalTime(copied) = Value::LocalTime(time.clone()).clone() else {
            panic!("time clone changed the variant");
        };
        assert_eq!(copied, time);

        let Value::LocalDateTime(copied) = Value::LocalDateTime(local.clone()).clone() else {
            panic!("date-time clone changed the variant");
        };
        assert_eq!(copied, local);

        let Value::OffsetDateTime(copied) = Value::OffsetDateTime(offset.clone()).clone() else {
            panic!("offset clone changed the variant");
        };
        assert_eq!(copied.local, offset.local);
        assert_eq!(copied.offset, offset.offset);
    }

    /// THE uniqueness invariant that makes the sharing unobservable: a mutation through a NON-UNIQUE handle detaches
    /// onto its own allocation before it writes, and the twin it shared with is left exactly as it was.
    ///
    /// This is checked on both container mutators — the array's element spine and the object's entry table —
    /// because either one writing in place through a shared allocation would make clone observable, and detach-on-write
    /// rests on it not being.
    #[test]
    fn a_mutation_through_a_shared_handle_detaches_and_leaves_its_twin_undisturbed() {
        let original = array_of(&["a", "b"]);
        let mut twin = original.clone_shared();
        assert!(twin.shares_storage_with(&original));
        twin.try_push(Value::try_string("c").expect("appended element"))
            .expect("shared array push detaches");
        assert!(
            !twin.shares_storage_with(&original),
            "a push through a shared spine must detach"
        );
        assert_eq!(render_array(&original), "a,b");
        assert_eq!(render_array(&twin), "a,b,c");

        let original = object_of(&["a", "b"]);
        let mut twin = original.clone_shared();
        assert!(twin.shares_storage_with(&original));
        *twin
            .try_get_index_mut(0)
            .expect("shared object mutation detaches")
            .expect("entry exists") = Value::Null;
        assert!(
            !twin.shares_storage_with(&original),
            "a value write through a shared entry table must detach"
        );
        assert_eq!(render_object(&original), "k0=a,k1=b");
        assert_eq!(render_object(&twin), "k0=null,k1=b");
    }

    /// The string accumulator is a THIRD mutation path beside the array spine and the object entry table, and it owes
    /// the same law: growth through a shared payload detaches onto a fresh allocation, leaving the twin exactly as it
    /// was. `Shared::try_extend` names this as its detach story; this test pins it at the value level like the
    /// container pair above.
    #[test]
    fn a_shared_string_growth_detaches_and_leaves_its_twin_undisturbed() {
        let source = Value::try_string("a").expect("string");
        let Value::String(original) = &source else {
            panic!("string fixture changed the variant");
        };
        let mut twin = original.clone_shared();
        assert!(twin.shares_allocation_with(original));
        twin.try_extend("b").expect("shared string growth detaches");
        assert!(
            !twin.shares_allocation_with(original),
            "growth through a shared payload must detach"
        );
        assert_eq!(original.as_str(), "a");
        assert_eq!(twin.as_str(), "ab");
    }

    /// Dropping a clone restores uniqueness, and the restored sole handle mutates without ceremony. In-place-ness
    /// itself is unobservable through the public surface — any surviving twin forces the detach — so this test pins
    /// what a clone's drop guarantees: the original's mutation path is untouched by the clone that came and went.
    #[test]
    fn dropping_the_last_clone_restores_a_fully_mutable_sole_handle() {
        let mut array = array_of(&["a"]);
        let before = array.clone_shared();
        drop(before);
        array
            .try_push(Value::try_string("b").expect("appended element"))
            .expect("sole-handle push");
        assert_eq!(render_array(&array), "a,b");
    }

    /// `Value` is `Send`: payloads have no request-ledger handle.
    #[test]
    fn a_value_is_send_under_the_ambient_allocator() {
        fn assert_send<T: Send>() {}
        assert_send::<Value>();
    }

    fn array_of(items: &[&str]) -> Array {
        let mut values = Vec::new();
        for item in items {
            values.push(Value::try_string(item).expect("element"));
        }
        Array::try_from_vec(values).expect("array fixture")
    }

    fn object_of(items: &[&str]) -> Object {
        let mut builder = ObjectBuilder::try_with_capacity(items.len()).expect("builder");
        for (index, item) in items.iter().enumerate() {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str(&alloc::format!("k{index}")).expect("key"),
                    Value::try_string(item).expect("entry value"),
                )
                .expect("entry");
        }
        builder.try_finish().expect("object fixture")
    }

    fn render(value: &Value) -> String {
        match value {
            Value::Null => String::from("null"),
            Value::String(text) => String::from(text.as_str()),
            Value::Bool(flag) => alloc::format!("{flag}"),
            other => alloc::format!("{other:?}"),
        }
    }

    fn render_array(array: &Array) -> String {
        let mut out = String::new();
        for (index, value) in array.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&render(value));
        }
        out
    }

    fn render_object(object: &Object) -> String {
        let mut out = String::new();
        for (index, entry) in object.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(entry.key());
            out.push('=');
            out.push_str(&render(entry.value()));
        }
        out
    }
}
