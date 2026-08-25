//! What kind of value this is: null, bool, number, string, and the rest.
//!
//! [`ValueKind`] is one core category per value. A tagged value reports the category of its payload. Tag text and exact
//! number spelling stay with the decoder.

/// Core category of a semantic value.
///
/// Tagged values report the payload's category through [`crate::Value::kind`]. The tag itself is separate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ValueKind {
    /// The null value.
    Null,
    /// A boolean.
    Bool,
    /// An exact or binary numeric value.
    Number,
    /// UTF-8 text.
    String,
    /// An uninterpreted byte string.
    Bytes,
    /// A local calendar date.
    LocalDate,
    /// A local wall-clock time.
    LocalTime,
    /// A local date and time.
    LocalDateTime,
    /// A date and time with an offset.
    OffsetDateTime,
    /// An ordered sequence.
    Array,
    /// An insertion-ordered, unique-key mapping.
    Object,
}

impl ValueKind {
    /// Whether this is a local date, local time, local date-time, or offset date-time.
    ///
    /// Those four carry calendar or clock structure, not text. Use this instead of matching the four variants by hand.
    /// [`ValueKind::Bytes`] is not temporal.
    #[must_use]
    pub const fn is_temporal(self) -> bool {
        matches!(
            self,
            Self::LocalDate | Self::LocalTime | Self::LocalDateTime | Self::OffsetDateTime
        )
    }
}
