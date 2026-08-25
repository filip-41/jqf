//! Format and dialect names: nonempty strings with no ASCII whitespace or control byte.
//!
//! [`FormatId`] and [`DialectId`] are owned. [`FormatIdRef`] and [`DialectIdRef`] are borrowed views of the same text.
//! Owned and borrowed forms compare equal in either direction.

use core::fmt;

use crate::identity::{IdentityError, IdentityText};

macro_rules! identity_name {
    (
        owned $owned:ident, $owned_doc:literal;
        reference $reference:ident, $reference_doc:literal;
    ) => {
        identity_name!(@owned $owned, $owned_doc);
        identity_name!(@reference $reference, $reference_doc, $owned);
    };
    (@owned $name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(IdentityText);

        impl $name {
            /// Copy `value` into an owned identity.
            ///
            /// # Errors
            ///
            /// Empty, or an ASCII whitespace/control byte, is [`FormatIdError`]; so is allocation refusal.
            pub fn try_new(value: &str) -> Result<Self, FormatIdError> {
                crate::identity::validate(value).map_err(FormatIdError::from)?;
                IdentityText::try_new(value)
                    .map(Self)
                    .map_err(|_| FormatIdError::Allocation)
            }

            /// The name as text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }

            /// Wrap a name the schema already interned and checked.
            pub(crate) fn from_accounted(value: IdentityText) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
    (@reference $name:ident, $description:literal, $owned:ident) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name<'identity>(&'identity str);

        impl $name<'static> {
            /// Borrow a `'static` identity.
            ///
            /// # Panics
            ///
            /// Panics if `value` is empty or has an ASCII whitespace or control byte.
            #[must_use]
            pub const fn from_static(value: &'static str) -> Self {
                assert!(
                    crate::identity::validate(value).is_ok(),
                    "invalid static identity"
                );
                Self(value)
            }
        }

        impl<'identity> $name<'identity> {
            /// The name as text.
            #[must_use]
            pub const fn as_str(self) -> &'identity str {
                self.0
            }
        }

        impl PartialEq<$owned> for $name<'_> {
            fn eq(&self, other: &$owned) -> bool {
                self.0 == other.as_str()
            }
        }

        // The mirror of the impl above: without it `owned == ref` fails to compile while `ref == owned` answers, and
        // comparison direction is not a property callers should have to know.
        impl PartialEq<$name<'_>> for $owned {
            fn eq(&self, other: &$name<'_>) -> bool {
                self.as_str() == other.as_str()
            }
        }
    };
}

identity_name! {
    owned FormatId, "Owned format name, such as `json` or `yaml`.";
    reference FormatIdRef, "Borrowed format name. Copyable; no allocation.";
}

identity_name! {
    owned DialectId, "Owned dialect name, such as `json.jqf@1`.";
    reference DialectIdRef, "Borrowed dialect name. Copyable; no allocation.";
}

/// Why a format or dialect name was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatIdError {
    /// The name was empty.
    Empty,
    /// The name had an ASCII control or whitespace byte.
    InvalidCharacter,
    /// The allocator refused the copy.
    Allocation,
}

impl fmt::Display for FormatIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "format or dialect identity must not be empty",
            Self::InvalidCharacter => "format or dialect identity must not contain ASCII whitespace or control bytes",
            Self::Allocation => "format or dialect identity allocation failed",
        })
    }
}

impl core::error::Error for FormatIdError {}

impl From<IdentityError> for FormatIdError {
    fn from(value: IdentityError) -> Self {
        match value {
            IdentityError::Empty => Self::Empty,
            IdentityError::InvalidCharacter => Self::InvalidCharacter,
        }
    }
}
