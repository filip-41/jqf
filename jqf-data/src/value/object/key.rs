//! Object keys: short text inline, longer text shared.

use crate::Value;
use crate::value::ValueAllocationError;
use crate::value::shared::Shared;

/// Keys of at most this many bytes live inline in the [`ObjectKey`] struct, which costs no allocation at all; longer
/// keys stay refcount-shared text.
///
/// 22 is the classic small-string cap: `[u8; 22]` plus the length byte packs the inline arm into the 24 bytes the
/// two-arm enum costs anyway, where a 23-byte inline buffer would round the struct up to 32.
const INLINE_KEY_CAP: usize = 22;

/// One object key.
///
/// Keys of at most 22 bytes live inline. Longer keys share text, so the same field name across many objects is one
/// allocation.
///
/// Do not turn a key into a [`Value::String`](crate::Value::String) by sharing its allocation — that string could
/// outlive the object. Copy it with [`Self::try_to_value_string`].
///
/// The test-only `shares_text_with` is identity, not equality: two inline keys with the same text return `false`.
#[derive(Debug)]
pub struct ObjectKey(ObjectKeyRepr);

#[derive(Debug)]
#[repr(u8)]
enum ObjectKeyRepr {
    Inline { len: u8, bytes: [u8; INLINE_KEY_CAP] },
    Boxed(Shared<str>),
}

impl ObjectKey {
    /// Copy `text` into a key. Short keys stay inline; longer keys share.
    pub fn try_from_str(text: &str) -> Result<Self, ValueAllocationError> {
        if text.len() <= INLINE_KEY_CAP {
            let mut bytes = [0u8; INLINE_KEY_CAP];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            Ok(Self(ObjectKeyRepr::Inline {
                // SAFETY for the cast: the inline arm is taken only when
                // `text.len() <= INLINE_KEY_CAP` (22), which fits a `u8`.
                #[expect(clippy::cast_possible_truncation, reason = "guarded by INLINE_KEY_CAP")]
                len: text.len() as u8,
                bytes,
            }))
        } else {
            Shared::try_from_str(text).map(|shared| Self(ObjectKeyRepr::Boxed(shared)))
        }
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            ObjectKeyRepr::Inline { len, bytes } => {
                // SAFETY: the inline arm's bytes were copied from a valid
                // `&str` at construction (`try_from_str`), so the first `len`
                // bytes are valid UTF-8 of exactly that length.
                unsafe { core::str::from_utf8_unchecked(&bytes[..*len as usize]) }
            }
            ObjectKeyRepr::Boxed(shared) => shared.as_str(),
        }
    }

    /// Charged copy of this key as a string value.
    ///
    /// Never shares the key allocation — that string could outlive the object.
    pub fn try_to_value_string(&self) -> Result<Value, ValueAllocationError> {
        Value::try_string(self.as_str())
    }

    /// Another handle on the same key. Inline keys copy; long keys share.
    #[must_use]
    pub fn clone_shared(&self) -> Self {
        match &self.0 {
            ObjectKeyRepr::Inline { len, bytes } => Self(ObjectKeyRepr::Inline {
                len: *len,
                bytes: *bytes,
            }),
            ObjectKeyRepr::Boxed(shared) => Self(ObjectKeyRepr::Boxed(shared.clone_shared())),
        }
    }

    /// Whether these two handles name the same long-key allocation.
    ///
    /// Identity, not equality. Two separately built twins return `false`. Inline keys never share. Compare
    /// [`Self::as_str`] for equality.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn shares_text_with(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (ObjectKeyRepr::Boxed(a), ObjectKeyRepr::Boxed(b)) => a.shares_allocation_with(b),
            _ => false,
        }
    }

    /// Reports whether the key holds its text inline (no allocation).
    ///
    /// Test-only companion to [`Self::shares_text_with`]: sharing alone cannot tell the two arms apart, because two
    /// independently parsed keys answer `false` either way — so the object suite's key-sharing witness reads the
    /// representation directly. Production code never sees it.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn is_inline(&self) -> bool {
        matches!(self.0, ObjectKeyRepr::Inline { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::{INLINE_KEY_CAP, ObjectKey, ObjectKeyRepr};
    use alloc::format;

    /// The inline arm must not grow the struct past the two-arm budget: the whole point of the 22-byte cap is that the
    /// inline buffer plus the length byte pack into the same 24 bytes the boxed arm's alignment demands anyway.
    #[test]
    fn inline_arm_does_not_grow_the_key() {
        assert_eq!(core::mem::size_of::<ObjectKey>(), 24);
        assert_eq!(core::mem::size_of::<ObjectKeyRepr>(), 24);
        assert!(core::mem::size_of::<[u8; INLINE_KEY_CAP]>() < 24);
    }

    /// A key at the cap round-trips through the inline arm byte-exactly; one byte past it takes the boxed arm; both
    /// read identically.
    #[test]
    fn cap_boundary_round_trips() {
        let at_cap = "k".repeat(INLINE_KEY_CAP);
        let inline = ObjectKey::try_from_str(&at_cap).expect("inline key");
        assert_eq!(inline.as_str(), at_cap);
        // The inline arm shares nothing, not even with its own clone — it names no allocation — while the boxed
        // arm's clone shares.
        assert!(!inline.shares_text_with(&inline.clone_shared()));

        let past_cap = "k".repeat(INLINE_KEY_CAP + 1);
        let boxed = ObjectKey::try_from_str(&past_cap).expect("boxed key");
        assert_eq!(boxed.as_str(), past_cap);
        assert!(boxed.shares_text_with(&boxed.clone_shared()));

        // Equal text reads equal regardless of arm.
        assert_eq!(
            inline.as_str(),
            ObjectKey::try_from_str(&at_cap).expect("copy").as_str()
        );
        assert!(!inline.shares_text_with(&ObjectKey::try_from_str(&at_cap).expect("copy")));
        assert!(!boxed.shares_text_with(&inline));
    }

    /// The cap counts BYTES, and it never splits a character: a multibyte key at and across the byte cap must land on
    /// the right arm and round-trip its exact text.
    #[test]
    fn the_byte_cap_never_splits_a_multibyte_character() {
        let at_cap = "é".repeat(INLINE_KEY_CAP / 2); // 11 × 2 bytes
        let inline = ObjectKey::try_from_str(&at_cap).expect("inline key");
        assert!(inline.is_inline());
        assert_eq!(inline.as_str(), at_cap);

        let past_cap = "é".repeat(INLINE_KEY_CAP / 2 + 1); // 12 × 2 bytes
        let boxed = ObjectKey::try_from_str(&past_cap).expect("boxed key");
        assert!(!boxed.is_inline());
        assert_eq!(boxed.as_str(), past_cap);

        // A mixed multibyte/ASCII key crosses the cap on a whole byte too.
        let prefix = "é".repeat(10); // 20 bytes
        let at_cap = format!("{prefix}ab"); // 22 bytes
        let inline = ObjectKey::try_from_str(&at_cap).expect("inline key");
        assert!(inline.is_inline());
        assert_eq!(inline.as_str(), at_cap);
        let past_cap = format!("{prefix}abc"); // 23 bytes
        let boxed = ObjectKey::try_from_str(&past_cap).expect("boxed key");
        assert!(!boxed.is_inline());
        assert_eq!(boxed.as_str(), past_cap);
    }

    #[test]
    fn try_to_value_string_copies_and_does_not_share_a_boxed_key() {
        use crate::Value;

        let inline = ObjectKey::try_from_str("short").expect("inline key");
        let Value::String(text) = inline.try_to_value_string().expect("inline copy") else {
            panic!("expected a string value");
        };
        assert_eq!(text.as_str(), "short");

        let boxed = ObjectKey::try_from_str(&"k".repeat(INLINE_KEY_CAP + 1)).expect("boxed key");
        let value = boxed.try_to_value_string().expect("boxed copy");
        let Value::String(text) = &value else {
            panic!("expected a string value");
        };
        assert_eq!(text.as_str(), boxed.as_str());
        let ObjectKeyRepr::Boxed(key_text) = &boxed.0 else {
            panic!("expected a boxed key");
        };
        assert!(!key_text.shares_allocation_with(text));
    }
}
