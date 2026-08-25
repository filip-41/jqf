//! Small request knobs that are not money.
//!
//! They live on [`crate::ResourceContext`] so a decode or a query can ask "what should happen on a missing key?"
//! without talking to a flag parser.

/// What to do where a missing key, a bad index, or a null operand would otherwise become a value instead of an error.
///
/// `Lenient` is the default and returns the caller-provided fallback value. `Warn` returns the same value, then prints
/// one aggregated report. `Strict` turns the event into a raise. A `?`, a `//`, or a `try` body around the site fires
/// no event.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MismatchPolicy {
    /// Substitute a value, no report. This is the default.
    #[default]
    Lenient,
    /// Same answer as `Lenient`, plus one count per kind on stderr.
    Warn,
    /// Raise instead of substituting a value.
    Strict,
}

/// What a warning-severity diagnostic does to the run.
///
/// `Error` is the default: warnings stay warnings. `Warn` prints them. `Strict` promotes any warning to a failed run.
/// `Lenient` also accepts relaxed number spellings and huge exponents that strict JSON refuses.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StrictnessPolicy {
    /// Warnings stay warnings. This is the default.
    #[default]
    Error,
    /// Print the warnings; the run can still succeed.
    Warn,
    /// Any warning fails the run.
    Strict,
    /// Same warning behavior as `Error`, plus looser JSON number parsing. Do not send these requests to worker threads
    /// — the dial does not travel with them.
    Lenient,
}

/// Names for the mismatch kinds, in the same order as the counters.
///
/// A slice, not a fixed array: the count is single-sourced in [`MISMATCH_CELL_COUNT`] and a new kind cannot desync the
/// length.
pub static MISMATCH_CELL_NAMES: &[&str] = &[
    "missing-object-key",
    "index-out-of-range",
    "field-on-null",
    "index-or-slice-on-null",
    "null-additive-identity",
    "path-miss",
    "cross-kind-ordering",
    "assignment-vivifies",
    "delete-path-miss",
    "slice-clamped",
    "null-as-empty-container",
];

/// How many mismatch kinds the context counts.
pub const MISMATCH_CELL_COUNT: usize = MISMATCH_CELL_NAMES.len();

/// A value the encoder could not write as-is, so it rewrote it.
///
/// The warning names what came out, not which input format it came from.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(usize)]
pub enum ProjectionKind {
    /// A date, time, or date-time rendered as RFC 3339 text.
    Temporal,
    /// A tagged value published as its bare payload.
    Tag,
    /// A byte string rendered as base64url text.
    Bytes,
}

impl ProjectionKind {
    /// Every kind, in the order the host reports them.
    pub const ALL: [Self; 3] = [Self::Temporal, Self::Tag, Self::Bytes];

    /// How many kinds exist — the counter array's width.
    pub const COUNT: usize = Self::ALL.len();

    /// This kind's slot in the counter array.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The diagnostic code recorded once per projected value.
    #[must_use]
    pub const fn diagnostic_code(self) -> u16 {
        match self {
            Self::Temporal => crate::diag::codes::PROJECTION_TEMPORAL,
            Self::Tag => crate::diag::codes::PROJECTION_TAG,
            Self::Bytes => crate::diag::codes::PROJECTION_BYTES,
        }
    }

    /// The noun phrase a host warning uses after the count.
    #[must_use]
    pub const fn summary(self, count: u64) -> &'static str {
        match (self, count) {
            (Self::Temporal, 1) => "datetime rendered as an RFC 3339 string",
            (Self::Temporal, _) => "datetimes rendered as RFC 3339 strings",
            (Self::Tag, 1) => "tagged value published as its bare payload",
            (Self::Tag, _) => "tagged values published as their bare payload",
            (Self::Bytes, 1) => "byte string rendered as base64url text",
            (Self::Bytes, _) => "byte strings rendered as base64url text",
        }
    }
}
