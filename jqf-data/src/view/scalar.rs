//! Borrowed views of scalar nodes: numbers, dates, and other atoms.
//!
//! [`ScalarView`] is the borrowed atom form of an owned [`crate::Value`] or a document scalar. [`NumberView`] is
//! `Number | Integer(&str) | Decimal | Float`. There is no `Atom` type. The temporal views borrow storage so an encoder
//! can write canonical text without building an owned value.

use alloc::string::String;

use crate::document::{NodeSemantic, WidePayload};
use crate::{DataError, Decimal, Document, Float, Integer, LocalDate, Number, NumericError, UtcOffset};

/// Borrowed local wall-clock time.
#[derive(Clone, Copy, Debug)]
pub struct LocalTimeView<'value> {
    /// Hour in `0..=23`.
    pub hour: u8,
    /// Minute in `0..=59`.
    pub minute: u8,
    /// Second in `0..=60`.
    pub second: u8,
    /// Canonical fractional-second digits.
    pub fraction: &'value str,
}

/// Borrowed local date and time.
#[derive(Clone, Copy, Debug)]
pub struct LocalDateTimeView<'value> {
    /// Calendar date.
    pub date: LocalDate,
    /// Wall-clock time.
    pub time: LocalTimeView<'value>,
}

/// Borrowed date and time with a UTC offset.
#[derive(Clone, Copy, Debug)]
pub struct OffsetDateTimeView<'value> {
    /// Local date and time.
    pub local: LocalDateTimeView<'value>,
    /// Offset semantics.
    pub offset: UtcOffset,
}

impl LocalTimeView<'_> {
    /// Append `HH:MM:SS[.fraction]` to `out`.
    ///
    /// Same text as [`crate::LocalTime::write_text`]. An encoder can write this view without building an owned value.
    pub fn write_text(&self, out: &mut String) -> Result<(), crate::TemporalError> {
        crate::temporal::write_time_text(out, self.hour, self.minute, self.second, self.fraction)
    }
}

impl LocalDateTimeView<'_> {
    /// Append `YYYY-MM-DDTHH:MM:SS[.fraction]` to `out`.
    pub fn write_text(&self, out: &mut String) -> Result<(), crate::TemporalError> {
        self.date.write_text(out)?;
        crate::temporal::push(out, "T")?;
        self.time.write_text(out)
    }
}

impl OffsetDateTimeView<'_> {
    /// Append RFC 3339 text to `out`.
    pub fn write_text(&self, out: &mut String) -> Result<(), crate::TemporalError> {
        self.local.write_text(out)?;
        self.offset.write_text(out)
    }
}

fn local_time_view(time: &crate::LocalTime) -> LocalTimeView<'_> {
    LocalTimeView {
        hour: time.hour(),
        minute: time.minute(),
        second: time.second(),
        fraction: time.fraction().digits(),
    }
}

fn local_date_time_view(datetime: &crate::LocalDateTime) -> LocalDateTimeView<'_> {
    LocalDateTimeView {
        date: datetime.date,
        time: local_time_view(&datetime.time),
    }
}

/// Borrowed number. Does not own the document.
#[derive(Clone, Copy, Debug)]
pub enum NumberView<'value> {
    /// Existing canonical owned number.
    Number(&'value Number),
    /// Canonical signed integer text.
    Integer(&'value str),
    /// Exact decimal coefficient and scale.
    Decimal {
        /// Canonical signed coefficient text.
        coefficient: &'value str,
        /// Base-ten scale.
        scale: i64,
    },
    /// Exact binary64 payload.
    Float(Float),
}

impl NumberView<'_> {
    /// Whether this number is negative.
    ///
    /// Same answer as [`Number::is_negative`] on the owned arms; the float arm reads the sign bit, so negative zero
    /// counts as negative. A non-negative number can stay borrowed; a negative one needs an owned copy to drop the
    /// sign.
    fn is_negative(&self) -> bool {
        match self {
            Self::Number(number) => number.is_negative(),
            Self::Integer(text) => text.starts_with('-'),
            Self::Decimal { coefficient, .. } => coefficient.starts_with('-'),
            Self::Float(value) => value.get().is_sign_negative(),
        }
    }

    /// Same as [`Number::try_sign_stripped`]: drop the minus, keep category and spelling (`-1.000` → `1.000`).
    ///
    /// # Errors
    ///
    /// Returns [`NumericError`] when allocating the stripped storage fails or the retained coefficient/scale is
    /// invalid.
    pub fn try_sign_stripped(&self) -> Result<Number, NumericError> {
        match self {
            Self::Number(number) => number.try_sign_stripped(),
            Self::Integer(text) => {
                Number::try_integer_unaccounted(Integer::parse(text.strip_prefix('-').unwrap_or(text))?)
            }
            Self::Decimal { coefficient, scale } => {
                let stripped = coefficient.strip_prefix('-').unwrap_or(coefficient);
                let mut owned = String::new();
                owned
                    .try_reserve_exact(stripped.len())
                    .map_err(|_| NumericError::Allocation)?;
                owned.push_str(stripped);
                Number::try_decimal_unaccounted(Decimal::from_literal_parts(Integer::from_canonical(owned)?, *scale)?)
            }
            Self::Float(value) => Ok(Number::float(Float::new(value.get().abs()))),
        }
    }

    /// Same as [`Number::try_negated`]: flip the sign. Zero of either sign becomes positive zero. Keeps a retained
    /// spelling (`1.500` → `-1.500`).
    ///
    /// # Errors
    ///
    /// Returns [`NumericError`] when allocating the re-signed storage fails or the retained coefficient/scale is
    /// invalid.
    pub fn try_negated(&self) -> Result<Number, NumericError> {
        if self.is_negative() {
            return self.try_sign_stripped();
        }
        match self {
            Self::Number(number) => number.try_negated(),
            Self::Integer(text) => {
                if *text == "0" {
                    return Number::try_integer_unaccounted(Integer::parse(text)?);
                }
                Number::try_integer_unaccounted(Integer::parse(&crate::number::prefix_minus(text)?)?)
            }
            Self::Decimal { coefficient, scale } => {
                let digits = if *coefficient == "0" {
                    let mut owned = String::new();
                    owned.try_reserve_exact(1).map_err(|_| NumericError::Allocation)?;
                    owned.push('0');
                    owned
                } else {
                    crate::number::prefix_minus(coefficient)?
                };
                Number::try_decimal_unaccounted(Decimal::from_literal_parts(Integer::from_canonical(digits)?, *scale)?)
            }
            Self::Float(value) => {
                let magnitude = value.get();
                if magnitude == 0.0 {
                    return Ok(Number::float(Float::new(0.0)));
                }
                Ok(Number::float(Float::new(-magnitude)))
            }
        }
    }
}

/// One borrowed scalar.
#[derive(Clone, Copy, Debug)]
pub enum ScalarView<'value> {
    /// Null scalar.
    Null,
    /// Boolean scalar.
    Bool(bool),
    /// Numeric scalar.
    Number(NumberView<'value>),
    /// UTF-8 text scalar.
    String(&'value str),
    /// Byte-string scalar.
    Bytes(&'value [u8]),
    /// Local date scalar.
    LocalDate(&'value LocalDate),
    /// Local time scalar.
    LocalTime(LocalTimeView<'value>),
    /// Local date-time scalar.
    LocalDateTime(LocalDateTimeView<'value>),
    /// Offset date-time scalar.
    OffsetDateTime(OffsetDateTimeView<'value>),
}

impl<'value> ScalarView<'value> {
    /// Same as [`crate::ValueKind::is_temporal`] for this view.
    #[must_use]
    pub const fn is_temporal(&self) -> bool {
        matches!(
            self,
            Self::LocalDate(_) | Self::LocalTime(_) | Self::LocalDateTime(_) | Self::OffsetDateTime(_)
        )
    }

    /// Borrowed scalar view of an owned value. `None` for an array, object, or tag wrapper.
    ///
    /// Twin of [`crate::ValueView::scalar`]. A tagged value returns `None`; unwrap it with [`crate::Value::untagged`]
    /// first if you want the payload.
    #[must_use]
    pub fn from_value(value: &'value crate::Value) -> Option<Self> {
        Some(match value {
            crate::Value::Null => Self::Null,
            crate::Value::Bool(value) => Self::Bool(*value),
            crate::Value::Number(value) => Self::Number(NumberView::Number(value)),
            crate::Value::String(value) => Self::String(value),
            crate::Value::Bytes(value) => Self::Bytes(value),
            crate::Value::LocalDate(value) => Self::LocalDate(value),
            crate::Value::LocalTime(value) => Self::LocalTime(local_time_view(value)),
            crate::Value::LocalDateTime(value) => Self::LocalDateTime(local_date_time_view(value)),
            crate::Value::OffsetDateTime(value) => Self::OffsetDateTime(OffsetDateTimeView {
                local: local_date_time_view(&value.local),
                offset: value.offset,
            }),
            crate::Value::Array(_) | crate::Value::Object(_) | crate::Value::Tagged { .. } => {
                return None;
            }
        })
    }

    pub(crate) fn from_semantic(
        document: &'value Document<'_>,
        semantic: &'value NodeSemantic,
    ) -> Result<Option<Self>, DataError> {
        Ok(Some(match semantic {
            NodeSemantic::Null => Self::Null,
            NodeSemantic::Bool(value) => Self::Bool(*value),
            NodeSemantic::StoredInteger(value) => Self::Number(NumberView::Integer(
                document.text(*value).ok_or(DataError::InvalidDocument)?,
            )),
            NodeSemantic::AccountedFloat(value) => Self::Number(NumberView::Float(*value)),
            NodeSemantic::Text(_) => Self::String(document.semantic_text(semantic).ok_or(DataError::InvalidDocument)?),
            NodeSemantic::LocalDate(value) => Self::LocalDate(value),
            // A span-backed container is a CONTAINER, not a scalar: reporting "no scalar view" is the same answer a
            // built container gives.
            NodeSemantic::Array { .. } | NodeSemantic::Object { .. } | NodeSemantic::ContainerSpan { .. } => {
                return Ok(None);
            }
            NodeSemantic::Unrepresentable => return Err(DataError::UnrepresentableSemantic),
            NodeSemantic::Wide { id, .. } => Self::from_wide(document, document.wide_payload(*id)?)?,
        }))
    }

    fn from_wide(document: &'value Document<'_>, payload: &'value WidePayload) -> Result<Self, DataError> {
        Ok(match payload {
            WidePayload::StoredDecimal { coefficient, scale } => Self::Number(NumberView::Decimal {
                coefficient: document.text(*coefficient).ok_or(DataError::InvalidDocument)?,
                scale: *scale,
            }),
            WidePayload::AccountedBytes(value) => Self::Bytes(value.as_slice()),
            WidePayload::AccountedLocalTime(value) => Self::LocalTime(LocalTimeView {
                hour: value.hour,
                minute: value.minute,
                second: value.second,
                fraction: value.fraction.as_str(),
            }),
            WidePayload::AccountedLocalDateTime(value) => Self::LocalDateTime(LocalDateTimeView {
                date: value.date,
                time: LocalTimeView {
                    hour: value.time.hour,
                    minute: value.time.minute,
                    second: value.time.second,
                    fraction: value.time.fraction.as_str(),
                },
            }),
            WidePayload::AccountedOffsetDateTime(value) => Self::OffsetDateTime(OffsetDateTimeView {
                local: LocalDateTimeView {
                    date: value.local.date,
                    time: LocalTimeView {
                        hour: value.local.time.hour,
                        minute: value.local.time.minute,
                        second: value.local.time.second,
                        fraction: value.local.time.fraction.as_str(),
                    },
                },
                offset: value.offset,
            }),
        })
    }
}
