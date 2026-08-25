//! Parser frames, string/number automata, and grammar dials.
//!
//! Each open container holds an [`OwnedDepthGuard`] so nesting survives across poll calls. [`JsonGrammar`] is the
//! per-session RFC 8259 / JSONC / JSON5 feature set.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::{AccountedDocumentBuilder, AccountedTextStage, NodeId};
use jqf_resource::OwnedDepthGuard;
use jqf_source::Span;

use crate::byte_scan::Utf8Carry;

/// The top-of-stack dispatch state of one container frame.
///
/// `Clone` + `Copy` exist so the token-loop batching in `parse::value_step` can hoist the top frame's state into a
/// local for the admitted batch and write it back at batch boundaries — every variant's payload (`ObjectKey`) is a
/// copyable span.
#[derive(Clone, Copy)]
pub(crate) enum FrameState {
    ArrayValueOrEnd { may_end: bool },
    ArrayCommaOrEnd,
    ObjectKeyOrEnd { may_end: bool },
    ObjectColon(ObjectKey),
    ObjectValue(ObjectKey),
    ObjectCommaOrEnd,
}

#[derive(Clone, Copy)]
pub(crate) enum ObjectKey {
    Stored(Span),
    Source(Span),
}

/// The JSON-family grammar one decode session accepts: the extensions over strict RFC 8259 a grammar dial arms, plus
/// the leniency dial. Stored on the parse state and fed from the provider's options, so every grammar decision reads a
/// field instead of doing a resource lookup per token — the shape `resources.decode_lenient()` already had, now
/// carried per session.
///
/// Strict JSON's session carries `STRICT` with `lenient` copied from the resource dial at provider construction; a
/// JSONC session arms `comments` and/or `trailing_commas` (the dialect's one `bool` in `Options`). The difference
/// between the two JSONC dialects is exactly the `trailing_commas` bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one independent grammar bit per JSON-family extension; a bitflag would hide which feature each dial arm reads"
)]
pub(crate) struct JsonGrammar {
    /// `//` line comments and `/* */` block comments accepted as trivia.
    pub comments: bool,
    /// A trailing comma before a container's closing delimiter.
    pub trailing_commas: bool,
    /// The leniency dial: the reference's `+1`, `.5`, `1.`, `01` spellings.
    pub lenient: bool,
    /// The JSON5 grammar: single-quoted strings, ASCII `IdentifierName` object keys, hex integer literals, the
    /// `\x`/`\0`/line-continuation escapes, and the JSON5 whitespace set (U+2028/U+2029/U+FEFF). The json5 provider
    /// arms this together with `comments` and `trailing_commas` (JSON5 permits both).
    pub json5: bool,
}

impl JsonGrammar {
    /// The strict RFC 8259 grammar: no comments, no trailing commas, no leniency. With `lenient` copied from the
    /// resource dial this IS the strict-JSON session's grammar.
    pub(crate) const STRICT: Self = Self {
        comments: false,
        trailing_commas: false,
        lenient: false,
        json5: false,
    };
}

pub(crate) struct Frame {
    pub(crate) node: NodeId,
    pub(crate) state: FrameState,
    pub(crate) _depth: OwnedDepthGuard,
    /// The S4 canonicality duplicate-key probe: up to eight key fingerprints of the OPEN OBJECT's members, inline so
    /// the common small objects pay no allocation. `None` for array frames and once the session's canonicality flag has
    /// cleared (the probe is a decline-only detector: a match clears the flag, and a ninth key clears it too — the
    /// safe direction either way).
    pub(crate) seen_key_fingerprints: Option<([u64; 8], u8)>,
    /// The prune-map node governing THIS container's children: `PRUNE_ALL` when everything below is kept (the default
    /// and the only value when no prune map is armed), `PRUNE_OMIT` inside an omitted subtree, else an index into the
    /// session's prune map.
    pub(crate) prune: u32,
}

#[derive(Clone, Copy)]
pub(crate) enum StringTarget {
    Key,
    Value,
}

pub(crate) enum EscapeState {
    Plain,
    Escape,
    Unicode {
        value: u16,
        digits: u8,
    },
    LowBackslash {
        high: u16,
    },
    LowU {
        high: u16,
    },
    LowUnicode {
        high: u16,
        value: u16,
        digits: u8,
    },
    /// A JSON5 `\xHH` escape: two hex digits, one byte.
    Hex {
        value: u8,
        digits: u8,
    },
}

pub(crate) struct StringState {
    pub(crate) target: StringTarget,
    pub(crate) start: usize,
    pub(crate) cursor: usize,
    /// The string's opening quote byte: `b'"'` (JSON/JSONC) or `b'\''` (JSON5's single-quoted strings).
    pub(crate) quote: u8,
    pub(crate) text: Option<AccountedTextStage>,
    pub(crate) escape: EscapeState,
    /// Whether the string contains an escape (`\`). A string without one appends its source bytes DIRECTLY to the
    /// builder arena at the closing quote — one copy, never through the scratch — which is what makes the plain
    /// string the decode path's zero-copy case.
    pub(crate) had_escape: bool,
    /// Truncated UTF-8 sequence pending across a work-grant cut.
    #[allow(dead_code)]
    pub(crate) utf8_carry: Utf8Carry,
}

#[derive(Clone, Copy)]
pub(crate) enum NumberLex {
    Start,
    IntegerStart,
    Zero,
    Integer,
    FractionStart,
    Fraction,
    ExponentStart,
    ExponentSign,
    Exponent,
    /// A JSON5 hex integer (`0x…`): `0x` consumed, now inside the hex-digit run. `digit_count` counts hex digits; the
    /// value materializes as an exact `Integer`.
    Hex,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent lexical and magnitude facts that must survive poll boundaries; a bitflag would hide which fact each poll sets"
)]
pub(crate) struct NumberState {
    pub(crate) start: u32,
    pub(crate) cursor: u32,
    pub(crate) lex: NumberLex,
    pub(crate) negative: bool,
    pub(crate) digit_count: u32,
    pub(crate) first_nonzero: Option<u32>,
    pub(crate) last_nonzero: Option<u32>,
    pub(crate) first_nonzero_offset: Option<u32>,
    pub(crate) last_nonzero_offset: Option<u32>,
    pub(crate) last_digit_offset: Option<u32>,
    pub(crate) fraction_digits: u32,
    pub(crate) exponent_negative: bool,
    pub(crate) exponent: u128,
    pub(crate) exponent_overflow: bool,
    pub(crate) has_fraction_or_exponent: bool,
    /// True when a byte the strict grammar refuses was consumed under decode leniency: a leading `+`, a leading `.` or
    /// a point after the sign, a digit after a leading zero, or an exponent marker right after a bare point (`1.e5`).
    /// Such a number is never its own render and never names a source span; the flag is the signal both facts live on.
    pub(crate) lenient_spelling: bool,
}

#[cfg(test)]
mod number_state_layout {
    use super::NumberState;

    #[test]
    fn number_state_fits_in_the_u32_offset_budget() {
        // ~144 with usize offsets; u32 offsets target ~48-80 depending on the exponent field. Stay well under the old
        // size.
        assert!(
            core::mem::size_of::<NumberState>() <= 80,
            "NumberState is {} bytes",
            core::mem::size_of::<NumberState>()
        );
    }
}

impl NumberState {
    /// A number token that begins at `start`. Offsets that do not fit `u32` refuse with Overflow — the same ceiling
    /// the source span type uses.
    ///
    /// `already_lenient` seeds `lenient_spelling`: pass `true` only when a lenient-only byte was ALREADY CONSUMED
    /// before `start` (a leading `+` or a bare point), never as a dial read — `start_at(cursor, grammar.lenient)`
    /// would silently mark every strict integer non-verbatim.
    pub(crate) fn start_at(start: usize, already_lenient: bool) -> Result<Self, CodecError> {
        let start = offset_u32(start)?;
        Ok(Self {
            start,
            cursor: start,
            lex: NumberLex::Start,
            negative: false,
            digit_count: 0,
            first_nonzero: None,
            last_nonzero: None,
            first_nonzero_offset: None,
            last_nonzero_offset: None,
            last_digit_offset: None,
            fraction_digits: 0,
            exponent_negative: false,
            exponent: 0,
            exponent_overflow: false,
            has_fraction_or_exponent: false,
            lenient_spelling: already_lenient,
        })
    }

    pub(crate) const fn start_usize(&self) -> usize {
        self.start as usize
    }

    pub(crate) const fn cursor_usize(&self) -> usize {
        self.cursor as usize
    }
}

pub(crate) fn offset_u32(offset: usize) -> Result<u32, CodecError> {
    u32::try_from(offset).map_err(|_| CodecError::new(CodecFailureKind::Overflow))
}

pub(crate) struct NumberNormalizeState {
    pub(crate) source: NumberState,
    pub(crate) canonical: AccountedTextStage,
    pub(crate) copy_end: usize,
    pub(crate) copy_cursor: usize,
    pub(crate) scale: Option<i64>,
    pub(crate) prefix: Option<&'static str>,
}

/// What one parse session is decoding, which decides both where its root value may end and whether its text may name
/// the source instead of copying it.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ParseMode {
    /// One complete document occupying the whole source: anything after the root value is trailing content, and text
    /// may be a zero-copy span of the caller-retained bytes.
    Document,
    /// One value of an adjacent-value stream, opened over everything from its first byte to end of input. Whatever
    /// follows the root value belongs to the next value and is not this session's concern; text may still be a
    /// zero-copy span, of the extent this value alone occupies.
    AdjacentValue,
    /// One value over a buffer the session owns and recycles, which the published document must outlive — so every
    /// byte is copied into the builder arenas and no source span is ever committed.
    OwnedRun,
}

impl ParseMode {
    /// Whether the root value may end before the source does, leaving the rest to a caller who framed it.
    pub(crate) const fn ends_before_source(self) -> bool {
        !matches!(self, Self::Document)
    }

    /// Whether committed text may be a span of the source rather than a copy in the builder arenas.
    pub(crate) const fn retains_source_spans(self) -> bool {
        !matches!(self, Self::OwnedRun)
    }
}

pub(crate) enum ParsePhase {
    Value,
    String(StringState),
    /// Cooperatively hashing the exact source segment the completed root value occupies, which authenticates every
    /// zero-copy span committed into it.
    SealSource,
    Number(NumberState),
    NumberNormalize(NumberNormalizeState),
    Trailing,
    Finalize,
    Publish,
}

pub(crate) type Builder = AccountedDocumentBuilder<'static>;
pub(crate) type Finalizer = jqf_data::AccountedDocumentFinalizer<'static>;

#[cfg(all(test, target_pointer_width = "64"))]
mod layout_tests {
    use core::mem::{align_of, size_of};

    use super::{Frame, FrameState, ObjectKey};

    #[test]
    fn parser_frame_layout_is_pinned() {
        assert_eq!((size_of::<ObjectKey>(), align_of::<ObjectKey>()), (12, 4));
        assert_eq!((size_of::<FrameState>(), align_of::<FrameState>()), (16, 4));
        // The S4 canonicality probe (`seen_key_fingerprints`, an inline eight-fingerprint array) grows the frame; the
        // pin keeps the hot parse structure's layout deliberate.
        assert_eq!((size_of::<Frame>(), align_of::<Frame>()), (112, 8));
    }
}
