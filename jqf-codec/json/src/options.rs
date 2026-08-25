//! JSON-family encode/decode option payloads.
//!
//! Indent, NDJSON/json-seq profiles, and terminator spellings live with the JSON grammar. Codec-core keeps only the
//! opaque format/dialect id strings and the record-route slot.

use jqf_codec_core::{CodecError, CodecFailureKind, ValidationMode};

/// RFC 8259 insignificant whitespace between adjacent JSON texts.
///
/// JSON, NDJSON, and json-seq pass this on [`jqf_codec_core::DecodeRequest`]. Codec-core's skip set is empty; this
/// alphabet is not a kernel default.
pub const VALUE_SEPARATORS: &[u8] = b" \t\n\r";

/// Longest indentation run the JSON-family encoders write in one admitted write. A deeper run is written in chunks of
/// this size, so the fill is a fixed cost rather than one that scales with `max_nesting_depth`.
const SPACE_FILL: [u8; 128] = [b' '; 128];
const TAB_FILL: [u8; 64] = [b'\t'; 64];

/// Structural whitespace policy for encoded JSON.
///
/// [`Self::Compact`] writes no structural whitespace. [`Self::Spaces`] writes one line break per element, indented by
/// that many spaces per open container (the CLI default is `Spaces(2)`). [`Self::Tabs`] indents with one tab per open
/// container.
///
/// `Spaces(0)` writes every line break but no indentation, and is not the same as [`Self::Compact`]: it writes every
/// line break but no indentation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JsonIndent {
    /// No structural whitespace at all.
    #[default]
    Compact,
    /// One line break per element or member, indented by this many spaces per open container.
    Spaces(u8),
    /// One line break per element or member, indented by one tab per open container.
    Tabs,
}

impl JsonIndent {
    /// The repeating fill byte run and the number of its bytes that one open container contributes, or `None` when no
    /// structural whitespace is written at all.
    #[must_use]
    pub const fn fill(self) -> Option<(&'static [u8], usize)> {
        match self {
            Self::Compact => None,
            Self::Spaces(width) => Some((&SPACE_FILL, width as usize)),
            Self::Tabs => Some((&TAB_FILL, 1)),
        }
    }

    /// The bytes that separate an object key from its value: the reference writes a space after the colon in every
    /// indented mode, and none when compact.
    #[must_use]
    pub const fn key_separator(self) -> &'static [u8] {
        match self {
            Self::Compact => b":",
            _ => b": ",
        }
    }
}

/// Strict-JSON encoder options.
///
/// The codec's default is [`JsonIndent::Compact`], which is what every internal caller and the NDJSON framing want.
/// Matching the reference's *pretty* default is a product decision that belongs to the CLI, which passes the indent
/// explicitly.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is one CLI formatting flag (mirrors CliOutputSelection in jqf-cli, \
              which carries the same flags one layer up); grouping them would hide the surface"
)]
pub struct JsonEncodeOptions {
    /// Structural whitespace policy.
    pub indent: JsonIndent,
    /// A ROOT string item is written without quotes or escapes (its bytes verbatim); every other item, and every nested
    /// string, is written normally. The facade still owns the item newline.
    ///
    /// `ascii_output` takes precedence over this for a ROOT string: with both flags the string is written QUOTED with
    /// non-ASCII escaped (`-ja` and `-ra` both render the string `"h\u00e9llo"`), so the raw arm below is gated on
    /// `!ascii_output`.
    pub raw_strings: bool,
    /// Every object's member keys are emitted in ascending byte (UTF-8/codepoint) order, recursively.
    pub sort_keys: bool,
    /// Every character at or above `0x80` is escaped as `\uXXXX` (a supplementary character as its surrogate pair),
    /// exactly as the ascii arm writes it.
    pub ascii_output: bool,
    /// The facade terminates every item with a NUL byte instead of a newline, so a ROOT string written raw (the
    /// `raw_strings` arm below) can no longer carry a literal NUL of its own without colliding with that terminator.
    /// Gates a check, not a rewrite: the string is rejected rather than silently mangled.
    pub raw_output_nul: bool,
}

impl JsonEncodeOptions {
    /// Whether these options emit compact JSON with key order and spelling unchanged: the identity-encode dialect the
    /// canonical-source echo may publish. Indent, `-S`, `-a`, and `-r` each rewrite bytes the echo would otherwise
    /// copy.
    #[must_use]
    pub const fn emits_canonical_form(self) -> bool {
        matches!(self.indent, JsonIndent::Compact) && !self.sort_keys && !self.ascii_output && !self.raw_strings
    }
}

/// One of the two sealed NDJSON profiles.
///
/// There is no third, permissive profile. The donor's `Compatible` default is deliberately not ported: a profile that
/// silently repairs framing is exactly the thing that makes "is this NDJSON?" unanswerable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NdjsonProfile {
    /// `ndjson.strict@1`: the first framing or payload fault is terminal.
    ///
    /// An absent FINAL terminator is not a fault under either profile: JSON Lines permits it and every incumbent reader
    /// accepts it. Strictness is about what jqf refuses to repair, not about refusing valid streams.
    Strict,
    /// `ndjson.recovering@1`: framing faults become ordered issues and the stream continues after the next physical
    /// line feed.
    Recovering,
}

impl NdjsonProfile {
    /// Reconstructs the profile from the codec-neutral open envelope.
    #[must_use]
    pub const fn from_recovering(recovering: bool) -> Self {
        if recovering { Self::Recovering } else { Self::Strict }
    }

    /// The validation mode this profile requires.
    ///
    /// A mismatched dialect and validation mode is rejected during normalization, BEFORE any source byte is consumed
    /// — a recovering framer running under a strict request would silently downgrade the request's own contract.
    #[must_use]
    pub const fn validation(self) -> ValidationMode {
        match self {
            Self::Strict => ValidationMode::Strict,
            Self::Recovering => ValidationMode::Recover,
        }
    }
}

/// Shared body of [`NdjsonDecodeOptions`] and [`JsonSeqDecodeOptions`]: exactly one normalized per-record payload
/// ceiling. The two dialects' read-side payloads are byte-identical by design; the public wrappers exist only so each
/// dialect keeps its own name and type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordPayloadCeiling {
    max_record_bytes: u64,
}

impl RecordPayloadCeiling {
    /// Normalizes a per-record byte ceiling against the request's input ceiling.
    ///
    /// The default is the request's own effective input ceiling, so an omitted option never makes a legal stream
    /// illegal. An explicit value may only make the ceiling SMALLER: a per-record ceiling above the request ceiling
    /// would be an unenforceable promise.
    fn try_new(max_record_bytes: Option<u64>, input_ceiling: u64) -> Result<Self, CodecError> {
        let requested = max_record_bytes.unwrap_or(input_ceiling);
        if requested == 0 || requested > input_ceiling {
            return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
        }
        Ok(Self {
            max_record_bytes: requested,
        })
    }

    /// Effective per-record payload ceiling in bytes.
    #[must_use]
    const fn max_record_bytes(self) -> u64 {
        self.max_record_bytes
    }
}

/// Normalized NDJSON read-side options.
///
/// Byte-identical to [`JsonSeqDecodeOptions`]: both wrap the same single normalized per-record ceiling
/// ([`RecordPayloadCeiling`]) and differ only in name and type, so a caller cannot silently mix dialect option
/// payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NdjsonDecodeOptions(RecordPayloadCeiling);

impl NdjsonDecodeOptions {
    /// Normalizes a per-record byte ceiling against the request's input ceiling.
    ///
    /// The default is the request's own effective input ceiling, so an omitted option never makes a legal stream
    /// illegal. An explicit value may only make the ceiling SMALLER: a per-record ceiling above the request ceiling
    /// would be an unenforceable promise.
    pub fn try_new(max_record_bytes: Option<u64>, input_ceiling: u64) -> Result<Self, CodecError> {
        Ok(Self(RecordPayloadCeiling::try_new(max_record_bytes, input_ceiling)?))
    }

    /// Effective per-record payload ceiling in bytes.
    #[must_use]
    pub const fn max_record_bytes(self) -> u64 {
        self.0.max_record_bytes()
    }
}

/// The physical bytes an NDJSON encoder writes after every record.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NdjsonTerminator {
    /// One line feed. The default.
    #[default]
    Lf,
    /// One carriage return followed by one line feed.
    CrLf,
}

impl NdjsonTerminator {
    /// Resolves the canonical spelling to its bytes.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
        }
    }

    /// Parses a canonical spelling (`lf` or `crlf`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "lf" => Some(Self::Lf),
            "crlf" => Some(Self::CrLf),
            _ => None,
        }
    }
}

/// Normalized NDJSON write-side options.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NdjsonEncodeOptions {
    canonical_terminator: NdjsonTerminator,
}

impl NdjsonEncodeOptions {
    /// Selects the canonical terminator every encoded record receives.
    #[must_use]
    pub const fn new(canonical_terminator: NdjsonTerminator) -> Self {
        Self { canonical_terminator }
    }

    /// The canonical terminator.
    #[must_use]
    pub const fn canonical_terminator(self) -> NdjsonTerminator {
        self.canonical_terminator
    }
}

/// One of the two json-seq profiles.
///
/// `Strict` is the registered dialect. `Recovering` turns every framing fault into an ordered issue and continues after
/// the next RS; those issues never force the request's failure class. It is not a dialect (`json-seq.recover@1` stays
/// reserved).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonSeqProfile {
    /// `json-seq.strict@1`: the first framing or payload fault is terminal.
    Strict,
    /// The `--seq` flag's recovering route: framing faults become ordered issues, the stream continues after the next
    /// RS, and the exit class is left to the program's own last-record result.
    Recovering,
}

impl JsonSeqProfile {
    /// Reconstructs the profile from the codec-neutral open envelope.
    #[must_use]
    pub const fn from_recovering(recovering: bool) -> Self {
        if recovering { Self::Recovering } else { Self::Strict }
    }

    /// The validation mode this profile requires.
    ///
    /// A mismatched profile and validation mode is rejected during normalization, BEFORE any source byte is consumed.
    #[must_use]
    pub const fn validation(self) -> ValidationMode {
        match self {
            Self::Strict => ValidationMode::Strict,
            Self::Recovering => ValidationMode::Recover,
        }
    }
}

/// Normalized json-seq read-side options.
///
/// Byte-identical to [`NdjsonDecodeOptions`]: both wrap the same single normalized per-record ceiling
/// ([`RecordPayloadCeiling`]) and differ only in name and type, so a caller cannot silently mix dialect option
/// payloads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonSeqDecodeOptions(RecordPayloadCeiling);

impl JsonSeqDecodeOptions {
    /// Normalizes a per-record byte ceiling against the request's input ceiling.
    ///
    /// The default is the request's own effective input ceiling, so an omitted option never makes a legal stream
    /// illegal. An explicit value may only make the ceiling SMALLER: a per-record ceiling above the request ceiling
    /// would be an unenforceable promise.
    pub fn try_new(max_record_bytes: Option<u64>, input_ceiling: u64) -> Result<Self, CodecError> {
        Ok(Self(RecordPayloadCeiling::try_new(max_record_bytes, input_ceiling)?))
    }

    /// Effective per-record payload ceiling in bytes.
    #[must_use]
    pub const fn max_record_bytes(self) -> u64 {
        self.0.max_record_bytes()
    }
}

/// The physical bytes a json-seq encoder writes after every item.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JsonSeqSuffix {
    /// One line feed. The RFC 7464 encoder grammar's default.
    #[default]
    Lf,
    /// No suffix: the reference's `-j`/`--join-output` under `--seq`. (`NoSuffix`, not `None`: one `use
    /// JsonSeqSuffix::*;` away from an Option-looking arm.)
    NoSuffix,
    /// One NUL byte: the NUL-terminator guard under `--seq` (which implies `-j`'s no-LF law and replaces it).
    Nul,
}

/// Normalized json-seq write-side options.
///
/// The payload spelling (indent, `-r` raw strings, `-S` sort keys, `-a` ascii output, `--raw-output0` NUL guard) is the
/// strict-JSON render style, carried whole so the json-seq encoder renders every item exactly as bare JSON output would
/// and then frames it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JsonSeqEncodeOptions {
    /// The strict-JSON render style each item's payload is encoded under.
    pub json: JsonEncodeOptions,
    /// The bytes written after every item.
    pub suffix: JsonSeqSuffix,
}

impl JsonSeqEncodeOptions {
    /// Selects the render style and the item suffix.
    #[must_use]
    pub const fn new(json: JsonEncodeOptions, suffix: JsonSeqSuffix) -> Self {
        Self { json, suffix }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JsonEncodeOptions, JsonIndent, JsonSeqDecodeOptions, JsonSeqProfile, JsonSeqSuffix, NdjsonDecodeOptions,
        NdjsonProfile, NdjsonTerminator, VALUE_SEPARATORS,
    };
    use jqf_codec_core::ValidationMode;

    #[test]
    fn adjacent_value_skip_set_is_rfc_8259_whitespace() {
        assert_eq!(VALUE_SEPARATORS, b" \t\n\r");
    }

    #[test]
    fn each_profile_pins_its_validation_mode() {
        assert_eq!(NdjsonProfile::Strict.validation(), ValidationMode::Strict);
        assert_eq!(NdjsonProfile::Recovering.validation(), ValidationMode::Recover);
        assert_eq!(JsonSeqProfile::Strict.validation(), ValidationMode::Strict);
        assert_eq!(JsonSeqProfile::Recovering.validation(), ValidationMode::Recover);
    }

    #[test]
    fn only_lf_and_crlf_parse_as_ndjson_terminators() {
        assert_eq!(NdjsonTerminator::parse("lf"), Some(NdjsonTerminator::Lf));
        assert_eq!(NdjsonTerminator::parse("crlf"), Some(NdjsonTerminator::CrLf));
        assert_eq!(NdjsonTerminator::parse("cr"), None);
        assert_eq!(NdjsonTerminator::Lf.bytes(), b"\n");
        assert_eq!(NdjsonTerminator::CrLf.bytes(), b"\r\n");
    }

    #[test]
    fn an_omitted_record_ceiling_defaults_to_the_input_ceiling() {
        assert_eq!(
            NdjsonDecodeOptions::try_new(None, 1024)
                .expect("default")
                .max_record_bytes(),
            1024
        );
        assert_eq!(
            JsonSeqDecodeOptions::try_new(None, 1024)
                .expect("default")
                .max_record_bytes(),
            1024
        );
    }

    #[test]
    fn an_explicit_ceiling_may_shrink_but_never_grow() {
        assert!(NdjsonDecodeOptions::try_new(Some(2048), 1024).is_err());
        assert!(NdjsonDecodeOptions::try_new(Some(0), 1024).is_err());
        assert!(JsonSeqDecodeOptions::try_new(Some(2048), 1024).is_err());
    }

    #[test]
    fn suffixes_are_closed() {
        assert_eq!(JsonSeqSuffix::default(), JsonSeqSuffix::Lf);
        assert_ne!(JsonSeqSuffix::NoSuffix, JsonSeqSuffix::Lf);
        assert_ne!(JsonSeqSuffix::Nul, JsonSeqSuffix::NoSuffix);
    }

    #[test]
    fn json_canonical_form_is_compact_unsorted_and_unescaped() {
        assert!(JsonEncodeOptions::default().emits_canonical_form());
        assert!(
            !JsonEncodeOptions {
                indent: JsonIndent::Spaces(2),
                ..JsonEncodeOptions::default()
            }
            .emits_canonical_form()
        );
        assert!(
            !JsonEncodeOptions {
                sort_keys: true,
                ..JsonEncodeOptions::default()
            }
            .emits_canonical_form()
        );
        assert!(
            !JsonEncodeOptions {
                ascii_output: true,
                ..JsonEncodeOptions::default()
            }
            .emits_canonical_form()
        );
        assert!(
            !JsonEncodeOptions {
                raw_strings: true,
                ..JsonEncodeOptions::default()
            }
            .emits_canonical_form()
        );
    }
}
