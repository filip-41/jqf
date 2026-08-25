//! The YAML byte front end and token scanner.
//!
//! This module owns the YAML 1.2.2 lexical layer:
//! - the ordered BOM/null-pattern encoding detection table (UTF-8 default, UTF-16LE/BE, UTF-32LE/BE), one encoding per
//!   stream, BOM allowed at stream start only, and an exact-replayable map from decoded byte offsets to original source
//!   bytes for UTF-16/32 sources;
//! - the token scanner: a faithful implementation of the YAML reference scanning algorithm producing the token set the
//!   event parser consumes — stream/document markers, directives, block/flow collection tokens, key/value indicators,
//!   anchors, aliases, tags, and scalars in every style (plain, single/double quoted, literal/folded block).
//!
//! The two subtle mechanisms are ported exactly:
//! - the block-collection indentation stack (roll/unroll with `BLOCK-END` tokens);
//! - the simple-key machinery: a potential simple key is saved per flow level and turned into a `KEY` token when a `:`
//!   value indicator follows it, with the single-line/1024-character staleness law and the required-key error.
//!
//! The scanner is a token producer the event parser drives. It never yields `Pending` mid-token: a state step can
//! consume several tokens, and lookahead needs a complete one. Each `poll` charges the token's byte length against the
//! work meter after the token is finished. It never recurses — YAML indentation depth is unbounded input, so the
//! indents stack is an arena, not a call stack.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_resource::{MemoryCategory, RequestAccount, ResourceContext};
use jqf_source::{Diagnostic, Label, LabelStyle, ResolvedSource, Span};

use crate::error;

/// The scalar styles the scanner distinguishes (presentation is native state, never folded into the semantic value).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScalarStyle {
    /// Unquoted plain scalar.
    Plain,
    /// Single-quoted scalar.
    SingleQuoted,
    /// Double-quoted scalar.
    DoubleQuoted,
    /// Literal block scalar (`|`).
    Literal,
    /// Folded block scalar (`>`).
    Folded,
}

/// What a scalar scan produced. The zero-copy shape: when the decoded text IS the exact source bytes at the span, the
/// scalar names the span instead of an owned string, and no arena copy is taken. Everything else (a fold that joined
/// line breaks, an escape, a non-UTF-8 source whose decoded buffer is not the source bytes) builds the owned text.
#[derive(Clone, Debug)]
pub(crate) enum ScannedScalar {
    /// The decoded text IS these exact source bytes. The span is in ORIGINAL source coordinates (the scan only returns
    /// this shape for UTF-8 sources, where the scanned text IS the source's own bytes).
    Span(Span),
    /// The decoded text differs from the source and is owned here.
    Owned(String),
}

/// One scanner token.
#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    /// Decoded start offset.
    pub(crate) start: usize,
    /// Decoded end offset (exclusive).
    pub(crate) end: usize,
}

/// Token kinds (the reference scanner's token set).
#[derive(Clone, Debug)]
pub(crate) enum TokenKind {
    StreamStart,
    StreamEnd,
    VersionDirective {
        major: u32,
        minor: u32,
    },
    TagDirective {
        handle: String,
        prefix: String,
    },
    /// A reserved (unknown-name) directive, consumed and ignored with a warning per YAML 1.2 §6.13.
    ReservedDirective,
    DocumentStart,
    DocumentEnd,
    BlockSequenceStart,
    BlockMappingStart,
    BlockEnd,
    FlowSequenceStart,
    FlowSequenceEnd,
    FlowMappingStart,
    FlowMappingEnd,
    BlockEntry,
    FlowEntry,
    Key,
    Value,
    Alias(String),
    Anchor(String),
    Tag {
        handle: String,
        suffix: String,
    },
    Scalar {
        value: ScannedScalar,
        style: ScalarStyle,
    },
}

/// A potential simple key at one flow level.
#[derive(Clone, Copy, Debug)]
struct SimpleKey {
    possible: bool,
    required: bool,
    token_number: usize,
    offset: usize,
    line: usize,
    column: usize,
}

/// One poll's outcome.
#[derive(Clone, Debug)]
pub(crate) enum ScanPoll {
    /// One token was produced.
    Token(Token),
    /// The stream end token was produced; the scanner is finished.
    Done,
}

/// The initial (stream-start) indentation level. Signed, like the reference's `-1`: the unroll loop `while indent >
/// column` must terminate when the indents stack empties, and an unsigned `usize::MAX` sentinel would never satisfy
/// `MAX > column` — an infinite `BLOCK-END` stream.
const INITIAL_INDENT: isize = -1;

/// The simple-key length limit. The specification states it in CHARACTERS; this scanner counts DECODED BYTES, so a key
/// made of multi-byte characters goes stale proportionally sooner than the spec's limit — a deliberate byte-accounting
/// narrowing, never an acceptance difference for ASCII keys.
const SIMPLE_KEY_MAX: usize = 1024;

/// The decoded source a session scans: the UTF-8 source itself, or an owned decoded buffer with an exact byte map for
/// UTF-16/32 sources.
///
/// The decoded buffers are request-retained products (they live beside the document for the whole session), so a
/// buffered descriptor carries one ledger charge booked at construction under [`MemoryCategory::Retained`] and released
/// exactly once when the buffers drop. Without that charge a UTF-16/32 transcode is memory nobody counted: the ambient
/// allocator only meters it when the host installed an account, and the default CLI run does not.
///
/// [`Self::detached_identity`] is the neutral element for sessions that hand the descriptor off: identity mapping, no
/// buffer.
pub(crate) struct DecodedSource {
    /// Owned decoded UTF-8 text, present only for UTF-16/32 sources.
    buffer: Option<Vec<u8>>,
    /// Per-decoded-byte original-offset map, present only for decoded sources. The map is monotone (decoded order ==
    /// original order) and exact (each decoded byte names the original byte that produced it).
    map: Option<Vec<u32>>,
    /// Source bytes consumed by the leading BOM (UTF-8 case).
    skip: usize,
    /// A shared handle to the account charged at construction and the exact resident bytes booked, released in `Drop`.
    /// `None` for an identity descriptor and for every charge refusal (whose buffers drop uncharged).
    charge: Option<(RequestAccount, u64)>,
}

/// Validates the UTF-8 half of a decoded source up front, so every subsequent character access in the scanner is on
/// valid UTF-8 — never a silent `null` from an ill-formed scalar (`port: \x80\x81`), a truncation at the first invalid
/// byte (`msg: hello\x80world` → `"hello"`), or a lossy replacement. The XML codec's own up-front law; an ill-formed
/// source is a REFUSAL with the offset named, never a mangled answer. The UTF-16/32 transcoders produce valid UTF-8 by
/// construction, so only the byte-sourced (UTF-8) paths validate.
fn validate_utf8(source: ResolvedSource<'_>, skip: usize) -> Result<(), CodecError> {
    if let Err(error) = core::str::from_utf8(&source.bytes()[skip..]) {
        let offset = skip + error.valid_up_to();
        return Err(crate::error::invalid_range(
            source,
            offset,
            offset.saturating_add(1),
            "invalid-utf8",
            "the source is not valid UTF-8 (YAML text is UTF-8/16/32; an ill-formed \
             scalar would otherwise be silently mangled)",
        ));
    }
    Ok(())
}

impl DecodedSource {
    /// The neutral identity descriptor (no buffer, no map, no BOM skip) for sessions that handed their real descriptor
    /// off.
    pub(crate) fn detached_identity() -> Self {
        Self {
            buffer: None,
            map: None,
            skip: 0,
            charge: None,
        }
    }

    /// Detects the stream encoding and decodes a UTF-16/32 source into UTF-8.
    pub(crate) fn try_new(source: ResolvedSource<'_>, resources: &ResourceContext<'_>) -> Result<Self, CodecError> {
        let bytes = source.bytes();
        if bytes.len() > u32::MAX as usize {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        }
        // BOM detection (the specification's ordered table).
        if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
            validate_utf8(source, 3)?;
            return Ok(Self {
                buffer: None,
                map: None,
                skip: 3,
                charge: None,
            });
        }
        if bytes.starts_with(&[0x00, 0x00, 0xFE, 0xFF]) {
            return Self::decode_utf32(bytes, 4, true, source, resources);
        }
        if bytes.starts_with(&[0xFF, 0xFE, 0x00, 0x00]) {
            return Self::decode_utf32(bytes, 4, false, source, resources);
        }
        if bytes.starts_with(&[0xFE, 0xFF]) {
            return Self::decode_utf16(bytes, 2, true, source, resources);
        }
        if bytes.starts_with(&[0xFF, 0xFE]) {
            return Self::decode_utf16(bytes, 2, false, source, resources);
        }
        // Null-pattern detection: a NUL in the first four bytes indicates UTF-16/32 (UTF-8 text carries NUL only as an
        // escape, never raw).
        let prefix = &bytes[..bytes.len().min(4)];
        if prefix.contains(&0) {
            if prefix.len() >= 4 && prefix[0] == 0 && prefix[1] == 0 {
                if prefix[2] == 0 && prefix[3] != 0 {
                    return Self::decode_utf32(bytes, 0, true, source, resources);
                }
                if prefix[2] != 0 && prefix[3] == 0 {
                    return Self::decode_utf32(bytes, 0, false, source, resources);
                }
            }
            if prefix.len() >= 2 && prefix[0] == 0 && prefix[1] != 0 {
                return Self::decode_utf16(bytes, 0, true, source, resources);
            }
            if prefix.len() >= 2 && prefix[0] != 0 && prefix[1] == 0 {
                return Self::decode_utf16(bytes, 0, false, source, resources);
            }
        }
        // UTF-8 default (BOM handled above).
        validate_utf8(source, 0)?;
        Ok(Self {
            buffer: None,
            map: None,
            skip: 0,
            charge: None,
        })
    }

    /// The decoded text for one poll: the owned buffer for UTF-16/32 sources, the source bytes themselves for UTF-8.
    pub(crate) fn text<'a>(&'a self, source: ResolvedSource<'a>) -> &'a [u8] {
        match &self.buffer {
            Some(buffer) => buffer.as_slice(),
            None => source.bytes(),
        }
    }

    /// The decoded offset the scanner starts at (after the BOM).
    #[must_use]
    pub(crate) fn start_offset(&self) -> usize {
        match &self.buffer {
            Some(_) => 0,
            None => self.skip,
        }
    }

    /// Whether the decoded text the graph spans address IS the source's own bytes (UTF-8 input, BOM skip included).
    /// Only then may a bound-source span be committed against the source: a UTF-16/32 input's decoded buffer is not the
    /// source's bytes.
    #[must_use]
    pub(crate) fn maps_to_source(&self) -> bool {
        self.map.is_none()
    }

    /// Whether this descriptor is the plain UTF-8 no-BOM shape: the shape a fresh detection produces for EVERY narrowed
    /// view of a BOM-free UTF-8 source. The adjacent-value reuse hook keeps only this shape — a BOM'd or UTF-16/32
    /// source's encoding detection depends on the view's own first bytes, which the reopen hook cannot see, so such a
    /// session declines and the caller constructs a fresh one.
    #[must_use]
    pub(crate) fn is_reusable(&self) -> bool {
        self.buffer.is_none() && self.skip == 0
    }

    /// The original source offset for a decoded offset.
    #[must_use]
    pub(crate) fn original(&self, decoded: usize, source_len: usize) -> usize {
        match &self.map {
            None => decoded.min(source_len),
            Some(map) => {
                let index = decoded.min(map.len());
                let mapped = map
                    .as_slice()
                    .get(index)
                    .copied()
                    .unwrap_or(u32::try_from(source_len).unwrap_or(u32::MAX));
                usize::try_from(mapped).unwrap_or(source_len)
            }
        }
    }

    /// Rebuilds one codec failure's diagnostic label spans from DECODED coordinates to ORIGINAL source coordinates.
    /// UTF-8 sources map identity and return the error untouched; a UTF-16/32 source's scanner, parser, and graph spans
    /// address the decoded buffer — not the source — so a diagnostic that skipped this mapping at the session boundary
    /// would name offsets that do not exist in the original bytes. A failure without a diagnostic passes through.
    pub(crate) fn translate_error(&self, error: CodecError, source_len: usize) -> CodecError {
        if self.map.is_none() {
            return error;
        }
        let Some(diagnostic) = error.diagnostic() else {
            return error;
        };
        let mut rebuilt = Diagnostic::new(diagnostic.code(), diagnostic.severity(), diagnostic.message());
        for source in diagnostic.sources() {
            rebuilt = rebuilt.with_source(source.clone());
        }
        for label in diagnostic.labels() {
            let start = self.original(label.span().start() as usize, source_len);
            let end = self.original(label.span().end() as usize, source_len);
            let span = Span::try_from_usize(start, end.max(start))
                .unwrap_or_else(|_| Span::try_from_usize(0, 0).expect("zero span"));
            let mapped = match label.style() {
                LabelStyle::Primary => Label::primary(label.source(), span, label.message()),
                LabelStyle::Secondary => Label::secondary(label.source(), span, label.message()),
            };
            rebuilt = rebuilt.with_label(mapped);
        }
        error.with_diagnostic(rebuilt)
    }

    fn decode_utf16(
        bytes: &[u8],
        skip: usize,
        big_endian: bool,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let mut out = Vec::new();
        out.try_reserve_exact(bytes.len() / 2)
            .map_err(jqf_resource::ResourceError::from)?;
        let mut map = Vec::new();
        map.try_reserve_exact(bytes.len() / 2)
            .map_err(jqf_resource::ResourceError::from)?;
        let mut units: Vec<u16> = Vec::new();
        let mut i = skip;
        while i + 1 < bytes.len() {
            let unit = if big_endian {
                u16::from_be_bytes([bytes[i], bytes[i + 1]])
            } else {
                u16::from_le_bytes([bytes[i], bytes[i + 1]])
            };
            units.push(unit);
            i += 2;
        }
        // A lone trailing byte is a truncated code unit, not content: the refuse-don't-mangle law applies to the
        // encoding itself.
        if i < bytes.len() {
            return Err(error::invalid(
                source,
                i,
                "encoding",
                "the source ends with a truncated UTF-16 code unit",
            ));
        }
        let mut k = 0;
        let mut scratch = [0u8; 4];
        while k < units.len() {
            let unit = units[k];
            let original = skip + k * 2;
            let (ch, consumed) = if (0xD800..0xDC00).contains(&unit) && k + 1 < units.len() {
                let low = units[k + 1];
                if (0xDC00..0xE000).contains(&low) {
                    let cp = 0x10000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                    let ch = char::from_u32(cp).ok_or_else(|| {
                        error::invalid(source, original, "encoding", "the source is not valid UTF-16")
                    })?;
                    (ch, 2)
                } else {
                    return Err(error::invalid(
                        source,
                        original,
                        "encoding",
                        "the source is not valid UTF-16",
                    ));
                }
            } else if (0xD800..0xE000).contains(&unit) {
                return Err(error::invalid(
                    source,
                    original,
                    "encoding",
                    "the source is not valid UTF-16",
                ));
            } else {
                let ch = char::from_u32(u32::from(unit))
                    .ok_or_else(|| error::invalid(source, original, "encoding", "the source is not valid UTF-16"))?;
                (ch, 1)
            };
            let width = ch.encode_utf8(&mut scratch).len();
            for &byte in &scratch[..width] {
                out.push(byte);
                map.push(u32::try_from(original).unwrap_or(u32::MAX));
            }
            k += consumed;
        }
        Self {
            buffer: Some(out),
            map: Some(map),
            skip: 0,
            charge: None,
        }
        .charge_decoded(resources)
    }

    fn decode_utf32(
        bytes: &[u8],
        skip: usize,
        big_endian: bool,
        source: ResolvedSource<'_>,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let mut out = Vec::new();
        out.try_reserve_exact(bytes.len() / 4)
            .map_err(jqf_resource::ResourceError::from)?;
        let mut map = Vec::new();
        map.try_reserve_exact(bytes.len() / 4)
            .map_err(jqf_resource::ResourceError::from)?;
        let mut i = skip;
        let mut scratch = [0u8; 4];
        while i + 3 < bytes.len() {
            let word = if big_endian {
                u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
            } else {
                u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]])
            };
            let ch = char::from_u32(word)
                .ok_or_else(|| error::invalid(source, i, "encoding", "the source is not valid UTF-32"))?;
            let width = ch.encode_utf8(&mut scratch).len();
            for &byte in &scratch[..width] {
                out.push(byte);
                map.push(u32::try_from(i).unwrap_or(u32::MAX));
            }
            i += 4;
        }
        // A 1-3 byte tail is a truncated code unit, not content.
        if i < bytes.len() {
            return Err(error::invalid(
                source,
                i,
                "encoding",
                "the source ends with a truncated UTF-32 code unit",
            ));
        }
        Self {
            buffer: Some(out),
            map: Some(map),
            skip: 0,
            charge: None,
        }
        .charge_decoded(resources)
    }

    /// Charges the decoded buffers' resident bytes against the request ledger once, under [`MemoryCategory::Retained`],
    /// and keeps the shared account handle for the release in `Drop`. The sibling pattern (the messagepack crate's
    /// equality matrix) charges explicitly so the ceiling refuses at the ledger even where no ambient account is
    /// installed; here the buffers are RETAINED, not scratch, so they book beside the request-accounted document rather
    /// than into `Working`.
    ///
    /// A refusal drops the freshly built buffers uncharged: the handle never entered `self`, so `Drop` releases
    /// nothing.
    fn charge_decoded(mut self, resources: &ResourceContext<'_>) -> Result<Self, CodecError> {
        let Some(buffer) = self.buffer.as_ref() else {
            return Ok(self);
        };
        let Some(map) = self.map.as_ref() else {
            return Ok(self);
        };
        let map_bytes = map
            .capacity()
            .checked_mul(core::mem::size_of::<u32>())
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        let bytes = buffer
            .capacity()
            .checked_add(map_bytes)
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        let Ok(bytes) = u64::try_from(bytes) else {
            return Err(CodecError::new(CodecFailureKind::Overflow));
        };
        let account = resources.account().try_share().map_err(CodecError::from)?;
        match account.charge_residency(bytes, MemoryCategory::Retained) {
            Ok(()) => {
                self.charge = Some((account, bytes));
                Ok(self)
            }
            Err(error) => Err(CodecError::from(error)),
        }
    }
}

impl Drop for DecodedSource {
    fn drop(&mut self) {
        // The exact bytes charged at construction, released when the buffers die — including every hand-off path
        // (`take_decoded` moves the charge with the descriptor).
        if let Some((account, bytes)) = self.charge.take() {
            account.release_residency(MemoryCategory::Retained, bytes);
        }
    }
}

/// The resumable YAML scanner. It owns no text: the session hands the decoded slice in on every poll.
pub(crate) struct Scanner {
    /// The cursor: decoded byte offset of the next character.
    offset: usize,
    /// Current line (zero-based).
    line: usize,
    /// Current column in characters.
    column: usize,
    /// The token queue. A plain `Vec`: the queue is bounded working state (a handful of tokens in flight), tokens are
    /// handed out strictly in FIFO order from the front, and a pending KEY token is inserted before queued ones by
    /// [`Self::insert_token`] — so no head index is needed.
    tokens: Vec<Token>,
    /// How many tokens have been handed out (for simple-key numbering).
    tokens_parsed: usize,
    /// Stream start/end produced flags.
    stream_start_produced: bool,
    stream_end_produced: bool,
    /// Whether a simple key may start at the current position.
    simple_key_allowed: bool,
    /// The indentation stack: current level at the top.
    indents: Vec<isize>,
    indent: isize,
    /// The flow nesting level.
    flow_level: usize,
    /// One potential simple key per flow level.
    simple_keys: Vec<SimpleKey>,
    /// The collection kind of each flow level (for the simple-key line-break law: a key inside a flow MAPPING survives
    /// a line break, a key inside a flow SEQUENCE does not).
    flow_kinds: Vec<FlowKind>,
    /// Trivia comment spans skipped since the last `take_pending_comments`: the parser drains them into the graph after
    /// each token, in source order, so the walker can attach them as leading-comment facts.
    pending_comments: Vec<Span>,
}

/// The collection kind of one flow nesting level.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlowKind {
    Sequence,
    Mapping,
}

impl Scanner {
    /// Creates a scanner; the cursor starts after any leading BOM.
    pub(crate) fn try_new(start_offset: usize) -> Self {
        let tokens = Vec::with_capacity(16);
        let mut indents = Vec::with_capacity(16);
        indents.push(INITIAL_INDENT);
        let mut simple_keys = Vec::with_capacity(4);
        simple_keys.push(SimpleKey {
            possible: false,
            required: false,
            token_number: 0,
            offset: 0,
            line: 0,
            column: 0,
        });
        let mut flow_kinds = Vec::with_capacity(4);
        flow_kinds.push(FlowKind::Sequence);
        Self {
            offset: start_offset,
            line: 0,
            column: 0,
            tokens,
            tokens_parsed: 0,
            stream_start_produced: false,
            stream_end_produced: false,
            simple_key_allowed: true,
            indents,
            indent: INITIAL_INDENT,
            flow_level: 0,
            simple_keys,
            flow_kinds,
            pending_comments: Vec::with_capacity(4),
        }
    }

    /// Takes the trivia comment spans skipped since the last call, in source order. The parser drains this after each
    /// token fetch so the spans land in the graph beside the nodes they precede.
    #[must_use]
    pub(crate) fn take_pending_comments(&mut self) -> Vec<Span> {
        core::mem::take(&mut self.pending_comments)
    }

    /// Reads the character at a decoded offset without advancing, or `None` at end of input.
    fn peek_at(text: &[u8], at: usize) -> Option<Char> {
        let byte = *text.get(at)?;
        // An ASCII byte is a complete single-byte scalar: no validation, no `from_utf8` call (the scanner is dominated
        // by ASCII source text).
        if byte < 0x80 {
            return Some(Char {
                value: byte as char,
                width: 1,
            });
        }
        let width = utf8_width(byte);
        let end = at + width;
        let slice = text.get(at..end)?;
        let value = core::str::from_utf8(slice).ok()?.chars().next()?;
        Some(Char { value, width })
    }

    /// Reads the character at the cursor without advancing.
    fn peek(&self, text: &[u8]) -> Option<Char> {
        Self::peek_at(text, self.offset)
    }

    /// Reads the character `n` decoded bytes past the cursor.
    fn peek_n(&self, text: &[u8], n: usize) -> Option<Char> {
        Self::peek_at(text, self.offset + n)
    }

    /// Reads the character `n` decoded bytes past the cursor, treating end-of-input as NUL (`z`), exactly like the
    /// reference scanner's NUL-padded buffer.
    fn peek_z(&self, text: &[u8], n: usize) -> char {
        Self::peek_at(text, self.offset + n).map_or('\0', |c| c.value)
    }

    /// Advances the cursor past `width` decoded bytes, updating line/column.
    fn advance(&mut self, text: &[u8], width: usize) {
        let mut remaining = width;
        while remaining > 0 {
            let Some(Char { value, width: w }) = Self::peek_at(text, self.offset) else {
                break;
            };
            if is_line_break(value) {
                self.line += 1;
                self.column = 0;
            } else {
                self.column += 1;
            }
            self.offset += w;
            remaining = remaining.saturating_sub(w);
        }
    }

    /// Advances one character, returning it. The character is decoded exactly once: `peek_at` returns it and the
    /// line/column update and offset advance use that same `Char` instead of re-decoding the byte through `advance`
    /// (every consumed character was previously UTF-8-decoded twice).
    fn skip(&mut self, text: &[u8]) -> Option<Char> {
        let c = Self::peek_at(text, self.offset)?;
        if is_line_break(c.value) {
            self.line += 1;
            self.column = 0;
        } else {
            self.column += 1;
        }
        self.offset += c.width;
        Some(c)
    }

    /// Consumes one line break and appends a single LF to `into`.
    ///
    /// YAML §5.4: CR, LF, CRLF, NEL, LS, and PS each fold to one LF in scalar content. [`Self::skip_line`] already
    /// consumes the full break; this is the only writer of those bytes into a scalar value, so a fourth scalar site
    /// cannot forget the fold.
    fn take_content_break(&mut self, text: &[u8], into: &mut String) {
        self.skip_line(text);
        into.push('\n');
    }

    /// Advances one full line break (LF, CR, CRLF, NEL, LS, PS).
    fn skip_line(&mut self, text: &[u8]) {
        let Some(Char { value, width }) = Self::peek_at(text, self.offset) else {
            return;
        };
        if value == '\r' && Self::peek_at(text, self.offset + width).is_some_and(|c| c.value == '\n') {
            self.offset += width + 1;
            self.line += 1;
            self.column = 0;
            return;
        }
        self.advance(text, width);
    }

    /// Whether a tab at the cursor, in block context, is followed (after any further whitespace) by a block indicator
    /// `-`, `?`, or `:` — that is, one of those characters in a position where it starts a block node (followed by a
    /// blank), not one that begins a plain scalar such as `-1`. A tab before such an indicator is not valid separation
    /// (the corpus's reading of the `s-separate-in-line` law); a tab before a plain scalar or flow start is.
    fn tab_precedes_block_indicator(&self, text: &[u8]) -> bool {
        let mut at = self.offset;
        loop {
            let Some(c) = Self::peek_at(text, at) else {
                return false;
            };
            if c.value == ' ' || c.value == '\t' {
                at += c.width;
                continue;
            }
            if !matches!(c.value, '-' | '?' | ':') {
                return false;
            }
            // The indicator must actually start a block node: `-`/`?`/`:` followed by a blank (or end of input).
            return Self::peek_at(text, at + c.width).is_none_or(|n| is_blank_or_z(n.value));
        }
    }

    /// The cooperative poll: fetch one token or report Done.
    ///
    /// The scanner does NOT admit cooperative work itself: it is a pure token producer driven by the event parser, and
    /// the parser admits per EVENT (the TOML precedent — a statement runs in one quantum, never yielding mid-step).
    /// This is what makes the parser's cross-token lookahead sound: a state step can consume several tokens, and the
    /// scanner never goes Pending in the middle of one.
    pub(crate) fn poll(
        &mut self,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ScanPoll, CodecError> {
        let _ = resources;
        let mut scans = 0usize;
        loop {
            scans += 1;
            if scans > 1_000_000 {
                return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "YAML scanner made no progress",
                }));
            }
            self.fetch_more_tokens(text, source, resources)?;
            if !self.tokens.is_empty() {
                let token = self.tokens.remove(0);
                self.tokens_parsed += 1;
                let width = token.end.saturating_sub(token.start).max(1);
                let _ = resources.admit_work_bytes(width)?;
                return Ok(ScanPoll::Token(token));
            }
            if self.stream_end_produced {
                return Ok(ScanPoll::Done);
            }
        }
    }

    /// Fetches tokens until at least one is available or the stream ends. A potential simple key occupying the head
    /// position forces another fetch, exactly like the reference implementation.
    fn fetch_more_tokens(
        &mut self,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        loop {
            let need_more = if self.stream_end_produced {
                // The stream ended: nothing more can be fetched. Any queued token is handed out by the caller; an empty
                // queue is Done.
                false
            } else if self.tokens.is_empty() {
                true
            } else {
                self.stale_simple_keys(source)?;
                self.simple_keys
                    .iter()
                    .any(|key| key.possible && key.token_number == self.tokens_parsed)
            };
            if !need_more {
                return Ok(());
            }
            self.fetch_next_token(text, source, resources)?;
        }
    }

    /// The original source offset for a decoded offset. The scanner works on the decoded text; for a UTF-8 source that
    /// is the source itself, so this is a clamp — a UTF-16/32 source's diagnostics are translated through the byte map
    /// when they leave the session.
    fn original(decoded: usize, source_len: usize) -> usize {
        decoded.min(source_len)
    }

    /// Removes stale potential simple keys (crossed a line or the length limit). Two passes over the bounded
    /// `simple_keys` stack — one to find a stale REQUIRED key, one to clear the rest — so the hottest loop (one call
    /// per fetched token) allocates nothing.
    fn stale_simple_keys(&mut self, source: ResolvedSource<'_>) -> Result<(), CodecError> {
        let (offset, line) = (self.offset, self.line);
        let mut stale_required: Option<usize> = None;
        for (index, key) in self.simple_keys.iter().enumerate() {
            if key.possible && Self::simple_key_stale(key, index, offset, line, &self.flow_kinds) && key.required {
                stale_required = Some(index);
                break;
            }
        }
        if let Some(index) = stale_required {
            let key = self.simple_keys[index];
            return Err(error::invalid(
                source,
                key.offset,
                "simple-key",
                "could not find expected ':'",
            ));
        }
        for (index, key) in self.simple_keys.iter_mut().enumerate() {
            if key.possible && Self::simple_key_stale(key, index, offset, line, &self.flow_kinds) {
                key.possible = false;
            }
        }
        Ok(())
    }

    /// Whether a potential simple key went stale: it crossed the length limit, or crossed a line break outside a flow
    /// MAPPING (a block-context or flow-SEQUENCE key does not span a line; a flow-mapping key does, the corpus's `{
    /// "foo"\n :bar }` reading).
    fn simple_key_stale(key: &SimpleKey, index: usize, offset: usize, line: usize, flow_kinds: &[FlowKind]) -> bool {
        if key.offset + SIMPLE_KEY_MAX < offset {
            return true;
        }
        key.line < line && !matches!(flow_kinds.get(index), Some(FlowKind::Mapping))
    }

    /// Whether the innermost flow level has a pending (possible) simple key, i.e. a completed node that a `:`/`?` can
    /// pair with as a key.
    fn simple_key_possible(&self) -> bool {
        self.simple_keys.last().is_some_and(|key| key.possible)
    }

    /// Saves a potential simple key at the current position.
    fn save_simple_key(&mut self) {
        let required = self.flow_level == 0 && isize::try_from(self.column).is_ok_and(|column| self.indent == column);
        if self.simple_key_allowed {
            let top = self.simple_keys.len() - 1;
            let token_number = self.tokens_parsed + self.tokens.len();
            self.simple_keys[top] = SimpleKey {
                possible: true,
                required,
                token_number,
                offset: self.offset,
                line: self.line,
                column: self.column,
            };
        }
    }

    /// Removes the potential simple key at the current flow level; a required key that never found its `:` is an error.
    fn remove_simple_key(&mut self, source: ResolvedSource<'_>) -> Result<(), CodecError> {
        let top = self.simple_keys.len() - 1;
        let key = self.simple_keys[top];
        if key.possible && key.required {
            return Err(error::invalid(
                source,
                key.offset,
                "simple-key",
                "could not find expected ':'",
            ));
        }
        self.simple_keys[top].possible = false;
        Ok(())
    }

    fn increase_flow_level(
        &mut self,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.flow_level >= usize::try_from(resources.limits().max_nesting_depth()).unwrap_or(usize::MAX) {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "nesting",
                "exceeded maximum nesting depth",
            ));
        }
        self.simple_keys.push(SimpleKey {
            possible: false,
            required: false,
            token_number: 0,
            offset: 0,
            line: 0,
            column: 0,
        });
        self.flow_kinds.push(FlowKind::Sequence);
        self.flow_level += 1;
        Ok(())
    }

    fn decrease_flow_level(&mut self) {
        if self.flow_level > 0 {
            self.flow_level -= 1;
            self.simple_keys.pop();
            self.flow_kinds.pop();
        }
    }

    /// Rolls the indentation up to `column` (block context only), emitting a `BLOCK-*` start token — appended, or
    /// inserted before `token_number` when the caller names one.
    fn roll_indent(
        &mut self,
        column: usize,
        token_number: Option<usize>,
        kind: TokenKind,
        offset: usize,
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.flow_level > 0 {
            return Ok(());
        }
        let column = isize::try_from(column).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
        if self.indent < column {
            if self.indents.len() >= usize::try_from(resources.limits().max_nesting_depth()).unwrap_or(usize::MAX) {
                return Err(error::invalid(
                    source,
                    Self::original(offset, source.bytes().len()),
                    "nesting",
                    "exceeded maximum nesting depth",
                ));
            }
            self.indents.push(self.indent);
            self.indent = column;
            let token = Token {
                kind,
                start: offset,
                end: offset,
            };
            match token_number {
                None => self.tokens.push(token),
                Some(number) => self.insert_token(number, token),
            }
        }
        Ok(())
    }

    /// Inserts a token at a queue position (relative to `tokens_parsed`).
    fn insert_token(&mut self, number: usize, token: Token) {
        let position = number.saturating_sub(self.tokens_parsed);
        let index = position.min(self.tokens.len());
        let tail = self.tokens.len();
        // Insert by shifting the tail up one slot.
        self.tokens.push(Token {
            kind: TokenKind::StreamEnd,
            start: 0,
            end: 0,
        });
        for shift in (index..tail).rev() {
            self.tokens.swap(shift, shift + 1);
        }
        self.tokens[index] = token;
    }

    /// Unrolls indentation levels above `column`, emitting `BLOCK-END`s.
    fn unroll_indent(&mut self, column: isize) {
        if self.flow_level > 0 {
            return;
        }
        while self.indent > column {
            self.tokens.push(Token {
                kind: TokenKind::BlockEnd,
                start: self.offset,
                end: self.offset,
            });
            self.indent = self.indents.pop().unwrap_or(INITIAL_INDENT);
        }
    }

    /// The token-fetch dispatcher (the reference scanner's `yaml_parser_fetch_next_token`).
    fn fetch_next_token(
        &mut self,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        // Progress guard: every step must advance the cursor or push a token (or produce the terminal stream end).
        let cursor_before = self.offset;
        let tokens_before = self.tokens.len();
        let result = self.fetch_next_token_inner(text, source, resources);
        if result.is_ok()
            && self.stream_start_produced
            && !self.stream_end_produced
            && self.offset == cursor_before
            && self.tokens.len() == tokens_before
        {
            return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "YAML scanner step made no progress",
            }));
        }
        result
    }

    /// The token-fetch dispatcher body (the reference scanner's `yaml_parser_fetch_next_token`).
    fn fetch_next_token_inner(
        &mut self,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if !self.stream_start_produced {
            self.fetch_stream_start();
            return Ok(());
        }
        if self.stream_end_produced {
            return Ok(());
        }
        self.scan_to_next_token(text, source)?;
        self.stale_simple_keys(source)?;
        let column = isize::try_from(self.column).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
        self.unroll_indent(column);
        let Some(Char { value, .. }) = self.peek(text) else {
            self.fetch_stream_end(source)?;
            return Ok(());
        };
        let col = self.column;
        if col == 0 && value == '%' {
            return self.fetch_directive(text, source);
        }
        if col == 0
            && value == '-'
            && self.peek_z(text, 1) == '-'
            && self.peek_z(text, 2) == '-'
            && is_blank_or_z(self.peek_z(text, 3))
        {
            return self.fetch_document_indicator(TokenKind::DocumentStart, text, source);
        }
        if col == 0
            && value == '.'
            && self.peek_z(text, 1) == '.'
            && self.peek_z(text, 2) == '.'
            && is_blank_or_z(self.peek_z(text, 3))
        {
            return self.fetch_document_indicator(TokenKind::DocumentEnd, text, source);
        }
        match value {
            '[' => self.fetch_flow_collection_start(TokenKind::FlowSequenceStart, text, source, resources),
            '{' => self.fetch_flow_collection_start(TokenKind::FlowMappingStart, text, source, resources),
            ']' => self.fetch_flow_collection_end(TokenKind::FlowSequenceEnd, text, source),
            '}' => self.fetch_flow_collection_end(TokenKind::FlowMappingEnd, text, source),
            ',' => self.fetch_flow_entry(text, source),
            '-' if is_blank_or_z(self.peek_z(text, 1)) => self.fetch_block_entry(text, source, resources),
            '?' if self.simple_key_possible() || is_blank_or_z(self.peek_z(text, 1)) => {
                self.fetch_key(text, source, resources)
            }
            ':' if self.simple_key_possible()
                || is_blank_or_z(self.peek_z(text, 1))
                || (self.flow_level > 0 && matches!(self.peek_z(text, 1), ',' | ']' | '}')) =>
            {
                self.fetch_value(text, source, resources)
            }
            '*' => self.fetch_anchor(text, source, false),
            '&' => self.fetch_anchor(text, source, true),
            '!' => self.fetch_tag(text, source),
            '|' if self.flow_level == 0 => self.fetch_block_scalar(text, source, true),
            '>' if self.flow_level == 0 => self.fetch_block_scalar(text, source, false),
            '\'' => self.fetch_flow_scalar(text, source, true),
            '"' => self.fetch_flow_scalar(text, source, false),
            _ if can_start_plain(value, || {
                // A plain scalar's first char must be followed by a char that can continue it: not blank, and (for
                // `-`/`?`/`:` in flow) not a flow indicator. `[-]` is a bare `-` before `]` and must not start a plain
                // scalar (the corpus's fail reading); `-1` is fine.
                let next = self.peek_z(text, 1);
                !(is_blank(next) || self.flow_level > 0 && matches!(next, ',' | '[' | ']' | '{' | '}'))
            }) =>
            {
                self.fetch_plain_scalar(text, source)
            }
            _ => Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "token",
                "found character that cannot start any token",
            )),
        }
    }

    fn fetch_stream_start(&mut self) {
        self.stream_start_produced = true;
        self.simple_key_allowed = true;
        self.tokens.push(Token {
            kind: TokenKind::StreamStart,
            start: 0,
            end: 0,
        });
    }

    fn fetch_stream_end(&mut self, source: ResolvedSource<'_>) -> Result<(), CodecError> {
        // Force a new line, reset the indentation, and remove any remaining simple key (a required key that never found
        // its ':' is an error).
        self.unroll_indent(INITIAL_INDENT);
        self.remove_simple_key(source)?;
        self.stream_end_produced = true;
        self.simple_key_allowed = false;
        self.tokens.push(Token {
            kind: TokenKind::StreamEnd,
            start: self.offset,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_document_indicator(
        &mut self,
        kind: TokenKind,
        text: &[u8],
        source: ResolvedSource<'_>,
    ) -> Result<(), CodecError> {
        let start = self.offset;
        self.unroll_indent(INITIAL_INDENT);
        self.remove_simple_key(source)?;
        self.simple_key_allowed = false;
        self.advance(text, 3);
        // A `...` document-end marker must be followed by a comment or a line break on the same line; any other
        // same-line content is invalid trailing data (the corpus's `... invalid` fail reading).
        if matches!(kind, TokenKind::DocumentEnd) {
            let mut at = self.offset;
            while let Some(c) = Self::peek_at(text, at) {
                if is_blank(c.value) {
                    at += c.width;
                } else {
                    break;
                }
            }
            if let Some(c) = Self::peek_at(text, at)
                && c.value != '#'
                && !is_line_break(c.value)
            {
                return Err(error::invalid(
                    source,
                    Self::original(at, source.bytes().len()),
                    "document",
                    "did not find expected <comment or line break>",
                ));
            }
        }
        self.tokens.push(Token {
            kind,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_directive(&mut self, text: &[u8], source: ResolvedSource<'_>) -> Result<(), CodecError> {
        self.unroll_indent(INITIAL_INDENT);
        self.remove_simple_key(source)?;
        self.simple_key_allowed = false;
        let start = self.offset;
        let kind = self.scan_directive(text, start, source)?;
        self.tokens.push(Token {
            kind,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_flow_collection_start(
        &mut self,
        kind: TokenKind,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        self.save_simple_key();
        self.increase_flow_level(source, resources)?;
        self.simple_key_allowed = true;
        let flow_kind = if matches!(kind, TokenKind::FlowMappingStart) {
            FlowKind::Mapping
        } else {
            FlowKind::Sequence
        };
        if let Some(top) = self.flow_kinds.last_mut() {
            *top = flow_kind;
        }
        let start = self.offset;
        self.advance(text, 1);
        self.tokens.push(Token {
            kind,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_flow_collection_end(
        &mut self,
        kind: TokenKind,
        text: &[u8],
        source: ResolvedSource<'_>,
    ) -> Result<(), CodecError> {
        self.remove_simple_key(source)?;
        self.decrease_flow_level();
        self.simple_key_allowed = false;
        let start = self.offset;
        self.advance(text, 1);
        self.tokens.push(Token {
            kind,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_flow_entry(&mut self, text: &[u8], source: ResolvedSource<'_>) -> Result<(), CodecError> {
        self.remove_simple_key(source)?;
        self.simple_key_allowed = true;
        let start = self.offset;
        self.advance(text, 1);
        self.tokens.push(Token {
            kind: TokenKind::FlowEntry,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_block_entry(
        &mut self,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.flow_level == 0 {
            if !self.simple_key_allowed {
                return Err(error::invalid(
                    source,
                    Self::original(self.offset, source.bytes().len()),
                    "block-entry",
                    "block sequence entries are not allowed in this context",
                ));
            }
            self.roll_indent(
                self.column,
                None,
                TokenKind::BlockSequenceStart,
                self.offset,
                source,
                resources,
            )?;
        }
        self.remove_simple_key(source)?;
        self.simple_key_allowed = true;
        let start = self.offset;
        self.skip(text);
        self.tokens.push(Token {
            kind: TokenKind::BlockEntry,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_key(
        &mut self,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        if self.flow_level == 0 {
            if !self.simple_key_allowed {
                return Err(error::invalid(
                    source,
                    Self::original(self.offset, source.bytes().len()),
                    "mapping-key",
                    "mapping keys are not allowed in this context",
                ));
            }
            self.roll_indent(
                self.column,
                None,
                TokenKind::BlockMappingStart,
                self.offset,
                source,
                resources,
            )?;
        }
        self.remove_simple_key(source)?;
        self.simple_key_allowed = self.flow_level == 0;
        let start = self.offset;
        self.skip(text);
        self.tokens.push(Token {
            kind: TokenKind::Key,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_value(
        &mut self,
        text: &[u8],
        source: ResolvedSource<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<(), CodecError> {
        let simple_key = self.simple_keys[self.simple_keys.len() - 1];
        if simple_key.possible {
            // The saved simple key becomes a KEY token inserted before the queued tokens, and the block mapping starts
            // at its column.
            let token = Token {
                kind: TokenKind::Key,
                start: simple_key.offset,
                end: simple_key.offset,
            };
            self.insert_token(simple_key.token_number, token);
            self.roll_indent(
                simple_key.column,
                Some(simple_key.token_number),
                TokenKind::BlockMappingStart,
                simple_key.offset,
                source,
                resources,
            )?;
            let top = self.simple_keys.len() - 1;
            self.simple_keys[top].possible = false;
            self.simple_key_allowed = false;
        } else {
            if self.flow_level == 0 {
                if !self.simple_key_allowed {
                    return Err(error::invalid(
                        source,
                        Self::original(self.offset, source.bytes().len()),
                        "mapping-value",
                        "mapping values are not allowed in this context",
                    ));
                }
                self.roll_indent(
                    self.column,
                    None,
                    TokenKind::BlockMappingStart,
                    self.offset,
                    source,
                    resources,
                )?;
            }
            self.simple_key_allowed = self.flow_level == 0;
        }
        let start = self.offset;
        self.skip(text);
        self.tokens.push(Token {
            kind: TokenKind::Value,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_anchor(&mut self, text: &[u8], source: ResolvedSource<'_>, anchor: bool) -> Result<(), CodecError> {
        self.save_simple_key();
        self.simple_key_allowed = false;
        let start = self.offset;
        let name = self.scan_anchor_name(text, start, source, anchor)?;
        let kind = if anchor {
            TokenKind::Anchor(name)
        } else {
            TokenKind::Alias(name)
        };
        self.tokens.push(Token {
            kind,
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_tag(&mut self, text: &[u8], source: ResolvedSource<'_>) -> Result<(), CodecError> {
        self.save_simple_key();
        self.simple_key_allowed = false;
        let start = self.offset;
        let (handle, suffix) = self.scan_tag(text, start, source)?;
        self.tokens.push(Token {
            kind: TokenKind::Tag { handle, suffix },
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_block_scalar(&mut self, text: &[u8], source: ResolvedSource<'_>, literal: bool) -> Result<(), CodecError> {
        self.remove_simple_key(source)?;
        self.simple_key_allowed = true;
        let start = self.offset;
        let value = self.scan_block_scalar(text, start, source, literal)?;
        let style = if literal {
            ScalarStyle::Literal
        } else {
            ScalarStyle::Folded
        };
        self.tokens.push(Token {
            kind: TokenKind::Scalar {
                value: ScannedScalar::Owned(value),
                style,
            },
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_flow_scalar(&mut self, text: &[u8], source: ResolvedSource<'_>, single: bool) -> Result<(), CodecError> {
        self.save_simple_key();
        self.simple_key_allowed = false;
        let start = self.offset;
        // See fetch_plain_scalar: a quoted scalar's inner span can name the source only for UTF-8 input.
        let maps_to_source = text.as_ptr() == source.bytes().as_ptr() && text.len() == source.bytes().len();
        let scanned = self.scan_flow_scalar(text, start, source, single, maps_to_source)?;
        let style = if single {
            ScalarStyle::SingleQuoted
        } else {
            ScalarStyle::DoubleQuoted
        };
        self.tokens.push(Token {
            kind: TokenKind::Scalar { value: scanned, style },
            start,
            end: self.offset,
        });
        Ok(())
    }

    fn fetch_plain_scalar(&mut self, text: &[u8], source: ResolvedSource<'_>) -> Result<(), CodecError> {
        self.save_simple_key();
        self.simple_key_allowed = false;
        let start = self.offset;
        // The span this scan would hand back is in DECODED offsets; it can name the SOURCE only when the scanned text
        // IS the source's own bytes (UTF-8 input). A UTF-16/32 source's decoded buffer is a different allocation, so
        // those scalars stay owned.
        let maps_to_source = text.as_ptr() == source.bytes().as_ptr() && text.len() == source.bytes().len();
        let (value, end) = self.scan_plain_scalar(text, start, source, maps_to_source)?;
        self.tokens.push(Token {
            kind: TokenKind::Scalar {
                value,
                style: ScalarStyle::Plain,
            },
            start,
            // The scalar's span ends at its last content byte, NEVER at the trailing separation/line break the scan
            // consumed while looking for a continuation line: a span that swallows the break (or the next line's
            // leading whitespace, when the break run ran past it) is a wrong span, and the parse-level byte-compare
            // that promotes a single-line plain scalar to the source-span zero-copy route would fail for every value
            // whose trailing break the span covered. The folded multi-line case keeps its full span (the bytes differ
            // from the decoded value anyway, so it stays owned).
            end,
        });
        Ok(())
    }

    /// Eats whitespace and comments until the next token.
    fn scan_to_next_token(&mut self, text: &[u8], source: ResolvedSource<'_>) -> Result<(), CodecError> {
        let mut flow_continuation = false;
        loop {
            // BOM at the start of a line is allowed. RECORDED NARROWING: this accepts a BOM at ANY column-0 line, not
            // only the stream/document start the spec reserves it for — a mid-stream BOM is skipped as trivia rather
            // than rejected.
            if self.column == 0 && self.peek(text).is_some_and(|c| c.value == '\u{FEFF}') {
                self.skip(text);
            }
            // Whitespace: spaces always; tabs in flow context or when a simple key is not allowed (never at
            // indentation). In block context a tab is separation whitespace only when it does NOT precede a block
            // indicator (`-`, `?`, `:`): `-\t-1` is a block entry then a scalar `-1`, while `-\t-` would be a tab
            // before another block entry, which is an error. A tab at the indentation position of a fresh block line is
            // consumed only when it precedes a FLOW collection start (`[`, `{`), which the corpus accepts at the top
            // level (`\t[`, `\t{}`).
            let mut separated = false;
            let mut line_start_tab = false;
            // The tab lookahead below depends only on where this blank RUN ends, and a whitespace pass consumes blanks
            // only — so the verdict cannot change between tabs of one pass. Computing it once per pass keeps a line
            // mixing N spaces and N tabs linear; re-evaluating per tab rescans the run's remaining suffix each time,
            // quadratic decodes inside one token fetch. Reset on every outer iteration because a new pass may begin
            // inside a different run.
            let mut tab_run_verdict: Option<bool> = None;
            while let Some(c) = self.peek(text) {
                if c.value == ' '
                    || (c.value == '\t'
                        && (self.flow_level > 0
                            || !self.simple_key_allowed
                            || (self.column > 0
                                && !*tab_run_verdict.get_or_insert_with(|| self.tab_precedes_block_indicator(text)))
                            || (self.column == 0 && matches!(self.peek_z(text, 1), '[' | '{'))))
                {
                    if c.value == '\t' && self.flow_level > 0 && self.column == 0 {
                        line_start_tab = true;
                    }
                    separated = true;
                    self.skip(text);
                } else {
                    break;
                }
            }
            // Inside a flow collection, a tab at the start of a continuation line (before any spaces) is valid only
            // before a flow separator/terminator (`,` `]` `}`) or on an otherwise-blank line: the corpus accepts `\t]`
            // and a tab-only line but rejects `\tfoo` in `- [\n\tfoo...`. After spaces set the line's indentation a tab
            // is content.
            if line_start_tab {
                let mut at = self.offset;
                while let Some(n) = Self::peek_at(text, at) {
                    if n.value == ' ' || n.value == '\t' {
                        at += n.width;
                    } else {
                        break;
                    }
                }
                if let Some(n) = Self::peek_at(text, at)
                    && !matches!(n.value, ',' | ']' | '}')
                    && !is_line_break(n.value)
                {
                    return Err(error::invalid(
                        source,
                        Self::original(self.offset, source.bytes().len()),
                        "flow",
                        "found a tab character that violates indentation",
                    ));
                }
            }
            // Comments: a `#` starts a comment only when it follows separation whitespace or stands at the start of a
            // line (the spec's `s-separate-in-line` law). `foo#bar` must stay a plain scalar token and `]#` without the
            // space is an error. The separation may have been consumed by a PREVIOUS token scan (a plain scalar's
            // trailing whitespace, a block scalar's indent scan), so look at the source byte before the cursor as well.
            let prev_separated = self.offset > 0 && matches!(text.get(self.offset - 1), Some(b' ' | b'\t'));
            if self.peek(text).is_some_and(|c| c.value == '#') && (separated || prev_separated || self.column == 0) {
                let start = self.offset;
                while let Some(c) = self.peek(text) {
                    if is_line_break(c.value) {
                        break;
                    }
                    self.skip(text);
                }
                // The span covers `# ...` up to the line break. Decoded-text offsets; v1 always runs identity (source
                // == decoded), so the walker slices `source.bytes()` with it directly.
                self.pending_comments.push(Span::from_usize(start, self.offset));
            }
            // Line break?
            if let Some(c) = self.peek(text)
                && is_line_break(c.value)
            {
                if self.flow_level > 0 {
                    flow_continuation = true;
                }
                self.skip_line(text);
                if self.flow_level == 0 {
                    self.simple_key_allowed = true;
                }
                continue;
            }
            // A flow collection continuation line must be indented strictly more than the enclosing block's indentation
            // (the corpus's fail reading for `flow: [a,\nb,\nc]` at column 0); the line's first content character
            // determines the column. A separator or terminator (`,` `]` `}`) at the indent is allowed.
            if flow_continuation
                && self.flow_level > 0
                && self.indent >= 0
                && self.column <= usize::try_from(self.indent).unwrap_or(usize::MAX)
                && !matches!(self.peek_z(text, 0), ',' | ']' | '}')
            {
                return Err(error::invalid(
                    source,
                    Self::original(self.offset, source.bytes().len()),
                    "flow",
                    "found an unindented continuation line in a flow collection",
                ));
            }
            return Ok(());
        }
    }

    /// Scans a `%YAML` or `%TAG` directive (the reference's `yaml_parser_scan_directive`). The cursor is on `%`.
    fn scan_directive(
        &mut self,
        text: &[u8],
        start: usize,
        source: ResolvedSource<'_>,
    ) -> Result<TokenKind, CodecError> {
        let _ = start;
        self.skip(text); // '%'
        // Scan the directive name (alpha+).
        let name_start = self.offset;
        while self.peek(text).is_some_and(|c| c.value.is_ascii_alphabetic()) {
            self.skip(text);
        }
        let name = core::str::from_utf8(&text[name_start..self.offset])
            .map_err(|_| CodecError::new(CodecFailureKind::InvalidInput))?;
        if name.is_empty() {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "directive",
                "could not find expected directive name",
            ));
        }
        if !self.peek(text).is_some_and(|c| is_blank_or_z(c.value)) {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "directive",
                "found unexpected non-alphabetical character",
            ));
        }
        let kind = match name {
            "YAML" => {
                let (major, minor) = self.scan_version_directive(text, source)?;
                TokenKind::VersionDirective { major, minor }
            }
            "TAG" => {
                let (handle, prefix) = self.scan_tag_directive(text, source)?;
                TokenKind::TagDirective { handle, prefix }
            }
            _ => {
                // A reserved directive's whole content is `ns-char+` and is ignored (YAML 1.2 §6.13); consume to end of
                // line.
                while let Some(c) = self.peek(text) {
                    if is_line_break(c.value) {
                        break;
                    }
                    self.skip(text);
                }
                if let Some(c) = self.peek(text)
                    && is_line_break(c.value)
                {
                    self.skip_line(text);
                }
                return Ok(TokenKind::ReservedDirective);
            }
        };
        // Eat the rest of the line including any comments.
        let mut separated = false;
        while self.peek(text).is_some_and(|c| is_blank(c.value)) {
            separated = true;
            self.skip(text);
        }
        if self.peek(text).is_some_and(|c| c.value == '#') && separated {
            while let Some(c) = self.peek(text) {
                if is_line_break(c.value) {
                    break;
                }
                self.skip(text);
            }
        }
        // End of input counts as a line break here (the reference's IS_BREAKZ check): `%TAG` as the last bytes of the
        // stream is a valid final directive.
        if !self
            .peek(text)
            .is_none_or(|c| is_line_break(c.value) || c.value == '\0')
        {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "directive",
                "did not find expected comment or line break",
            ));
        }
        if let Some(c) = self.peek(text)
            && is_line_break(c.value)
        {
            self.skip_line(text);
        }
        Ok(kind)
    }

    fn scan_version_directive(&mut self, text: &[u8], source: ResolvedSource<'_>) -> Result<(u32, u32), CodecError> {
        let start = self.offset;
        while self.peek(text).is_some_and(|c| is_blank(c.value)) {
            self.skip(text);
        }
        let major = self.scan_version_number(text, source, start)?;
        if self.peek(text).is_none_or(|c| c.value != '.') {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "version-directive",
                "did not find expected digit or '.' character",
            ));
        }
        self.skip(text);
        let minor = self.scan_version_number(text, source, start)?;
        Ok((major, minor))
    }

    fn scan_version_number(
        &mut self,
        text: &[u8],
        source: ResolvedSource<'_>,

        start: usize,
    ) -> Result<u32, CodecError> {
        let mut value: u32 = 0;
        let mut length = 0usize;
        while self.peek(text).is_some_and(|c| c.value.is_ascii_digit()) {
            length += 1;
            if length > 9 {
                return Err(error::invalid(
                    source,
                    Self::original(start, source.bytes().len()),
                    "version-directive",
                    "found extremely long version number",
                ));
            }
            let c = self.skip(text).expect("peeked digit");
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u32::from(c.value as u8 - b'0')))
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
        }
        if length == 0 {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "version-directive",
                "did not find expected version number",
            ));
        }
        Ok(value)
    }

    fn scan_tag_directive(&mut self, text: &[u8], source: ResolvedSource<'_>) -> Result<(String, String), CodecError> {
        while self.peek(text).is_some_and(|c| is_blank(c.value)) {
            self.skip(text);
        }
        let handle = self.scan_tag_handle(text, true, source)?;
        if !self.peek(text).is_some_and(|c| is_blank(c.value)) {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "tag-directive",
                "did not find expected whitespace",
            ));
        }
        while self.peek(text).is_some_and(|c| is_blank(c.value)) {
            self.skip(text);
        }
        let prefix = self.scan_tag_uri(text, true, source)?;
        // End of input counts as the break here too (IS_BLANKZ): a `%TAG` directive as the stream's last line is valid.
        if !self.peek(text).is_none_or(|c| is_blank_or_z(c.value)) {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "tag-directive",
                "did not find expected whitespace or line break",
            ));
        }
        Ok((handle, prefix))
    }

    /// Scans an anchor or alias name. The cursor is on `&`/`*`.
    fn scan_anchor_name(
        &mut self,
        text: &[u8],
        start: usize,
        source: ResolvedSource<'_>,

        anchor: bool,
    ) -> Result<String, CodecError> {
        let mut name = String::new();
        self.skip(text); // '&' or '*'
        let mut length = 0usize;
        // ns-anchor-char is any printable non-flow-indicator character (§4.8: anchor names are exact text; the corpus
        // carries Unicode anchors like `&😁`). The reference's IS_ALPHA restriction is a deliberate narrowing; the spec
        // allows ns-char minus ,[]{}, so this scanner follows the spec.
        while let Some(c) = self.peek(text) {
            if is_anchor_char(c.value) {
                name.push(c.value);
                self.skip(text);
                length += 1;
            } else {
                break;
            }
        }
        let ok = length > 0
            && self.peek(text).is_some_and(|c| {
                is_blank_or_z(c.value) || matches!(c.value, '?' | ':' | ',' | ']' | '}' | '%' | '@' | '`')
            });
        if !ok {
            let code = if anchor { "anchor" } else { "alias" };
            return Err(error::invalid(
                source,
                Self::original(start, source.bytes().len()),
                code,
                "did not find expected alphabetic or numeric character",
            ));
        }
        Ok(name)
    }

    /// Scans a tag token. The cursor is on `!`.
    fn scan_tag(
        &mut self,
        text: &[u8],
        start: usize,
        source: ResolvedSource<'_>,
    ) -> Result<(String, String), CodecError> {
        if self.peek_n(text, 1).is_some_and(|c| c.value == '<') {
            // Canonical form `!<...>`: handle is empty, suffix is the URI.
            self.skip(text); // '!'
            self.skip(text); // '<'
            let suffix = self.scan_tag_uri_verbatim(text, source)?;
            if self.peek(text).is_none_or(|c| c.value != '>') {
                return Err(error::invalid(
                    source,
                    Self::original(start, source.bytes().len()),
                    "tag",
                    "did not find the expected '>'",
                ));
            }
            self.skip(text);
            Ok((String::new(), suffix))
        } else {
            // `!suffix` or `!handle!suffix`: try a handle first.
            let handle = self.scan_tag_handle(text, false, source)?;
            let handle_bytes = handle.as_bytes();
            let is_handle =
                handle_bytes.first() == Some(&b'!') && handle_bytes.len() > 1 && handle_bytes.last() == Some(&b'!');
            if is_handle {
                let suffix = self.scan_tag_uri(text, false, source)?;
                Ok((handle, suffix))
            } else {
                // Not a handle: the whole rest is the suffix, with handle `!`. The already-scanned text is the URI's
                // HEAD (the reference's `scan_tag_uri(parser, 0, 0, handle, ...)` copies the head in, so a blank tail
                // is NOT an error).
                let mut suffix = handle;
                suffix.remove(0); // drop the leading '!'
                // A bare `!` with a blank tail is the bare `!` tag (handle `!`, suffix `!` in the reference's special
                // case). When the head is non-empty the URI scanner may continue; a blank tail after a non-empty head
                // is fine because the head itself is the suffix.
                if suffix.is_empty() && self.peek(text).is_some_and(|c| !is_blank_or_z(c.value)) {
                    // `!` directly followed by URI characters: scan them.
                    let rest = self.scan_tag_uri_with_head(text, false, "", source)?;
                    suffix = rest;
                } else if !suffix.is_empty() {
                    let rest = self.scan_tag_uri_with_head(text, false, &suffix, source)?;
                    suffix = rest;
                }
                let (handle, suffix) = if suffix.is_empty() {
                    // The bare '!' tag: handle empty, suffix '!'.
                    (String::new(), "!".to_owned())
                } else {
                    ("!".to_owned(), suffix)
                };
                Ok((handle, suffix))
            }
        }
    }

    /// Scans a tag handle starting at `!`. With `directive`, a bare `!` is the only single-`!` handle allowed.
    fn scan_tag_handle(
        &mut self,
        text: &[u8],
        directive: bool,
        source: ResolvedSource<'_>,
    ) -> Result<String, CodecError> {
        let start = self.offset;
        if self.peek(text).is_none_or(|c| c.value != '!') {
            return Err(error::invalid(
                source,
                Self::original(start, source.bytes().len()),
                if directive { "tag-directive" } else { "tag" },
                "did not find expected '!'",
            ));
        }
        let mut handle = String::new();
        handle.push('!');
        self.skip(text);
        while let Some(c) = self.peek(text) {
            if c.value.is_ascii_alphanumeric() {
                handle.push(c.value);
                self.skip(text);
            } else {
                break;
            }
        }
        if self.peek(text).is_some_and(|c| c.value == '!') {
            handle.push('!');
            self.skip(text);
        } else if directive && (handle != "!") {
            return Err(error::invalid(
                source,
                Self::original(start, source.bytes().len()),
                "tag-directive",
                "did not find expected '!'",
            ));
        }
        Ok(handle)
    }

    /// Scans a tag URI. `%XX` triplets are validated and copied raw, never decoded.
    fn scan_tag_uri(&mut self, text: &[u8], directive: bool, source: ResolvedSource<'_>) -> Result<String, CodecError> {
        self.scan_tag_uri_with_head(text, directive, "", source)
    }

    /// Scans a tag URI with a pre-scanned head (the reference's `head` parameter: the already-consumed handle text,
    /// minus its leading `!`).
    fn scan_tag_uri_with_head(
        &mut self,
        text: &[u8],
        directive: bool,
        head: &str,
        source: ResolvedSource<'_>,
    ) -> Result<String, CodecError> {
        let start = self.offset;
        let mut uri = head.to_owned();
        let mut length = head.len();
        while let Some(c) = self.peek(text) {
            // The URI char set (the reference's list). A `%TAG` DIRECTIVE prefix additionally admits `,` — the 1.2.2
            // spec's own example `tag:example.com,2000:app/` uses it, and the reference Ruby binding accepts it, even
            // though the spec's ns-tag-char grammar text excludes c-flow-indicators. The spec's example wins.
            let uri_char = c.value.is_ascii_alphanumeric()
                || matches!(
                    c.value,
                    ';' | '/'
                        | '?'
                        | ':'
                        | '@'
                        | '&'
                        | '='
                        | '+'
                        | '$'
                        | '.'
                        | '%'
                        | '!'
                        | '~'
                        | '*'
                        | '\''
                        | '('
                        | ')'
                        | '-'
                        | '_'
                        | '#'
                )
                || (directive && c.value == ',');
            if !uri_char {
                break;
            }
            if c.value == '%' {
                self.scan_uri_escapes(text, directive, start, source, &mut uri)?;
            } else {
                uri.push(c.value);
                self.skip(text);
            }
            length += 1;
        }
        if length == 0 {
            return Err(error::invalid(
                source,
                Self::original(start, source.bytes().len()),
                if directive { "tag-directive" } else { "tag" },
                "did not find expected tag URI",
            ));
        }
        Ok(uri)
    }

    /// The verbatim `!<...>` URI: flow indicators are also allowed.
    fn scan_tag_uri_verbatim(&mut self, text: &[u8], source: ResolvedSource<'_>) -> Result<String, CodecError> {
        let start = self.offset;
        let mut uri = String::new();
        let mut length = 0usize;
        while let Some(c) = self.peek(text) {
            let uri_char = c.value.is_ascii_alphanumeric()
                || matches!(
                    c.value,
                    ';' | '/'
                        | '?'
                        | ':'
                        | '@'
                        | '&'
                        | '='
                        | '+'
                        | '$'
                        | '.'
                        | '%'
                        | '!'
                        | '~'
                        | '*'
                        | '\''
                        | '('
                        | ')'
                        | ','
                        | '['
                        | ']'
                );
            if !uri_char {
                break;
            }
            if c.value == '%' {
                self.scan_uri_escapes(text, false, start, source, &mut uri)?;
            } else {
                uri.push(c.value);
                self.skip(text);
            }
            length += 1;
        }
        if length == 0 {
            return Err(error::invalid(
                source,
                Self::original(start, source.bytes().len()),
                "tag",
                "did not find expected tag URI",
            ));
        }
        Ok(uri)
    }

    /// Validates one `%XX` escape sequence and copies the RAW triplet into the tag text. §4.8: YAML percent triplets
    /// remain IDENTITY characters — `!foo%62ar` and `!foobar` are distinct `.@tag` facts — so the escapes are validated
    /// (exact hex digits) but never decoded. RECORDED NARROWING: each triplet is validated in isolation, not as a run
    /// of triplets decoding to one UTF-8 sequence, so an encoding that never forms valid UTF-8 is still accepted here
    /// (the reference rejects it).
    fn scan_uri_escapes(
        &mut self,
        text: &[u8],
        directive: bool,
        _start: usize,
        source: ResolvedSource<'_>,

        uri: &mut String,
    ) -> Result<(), CodecError> {
        let c0 = self.peek(text).ok_or_else(|| {
            error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "tag",
                "did not find URI escaped octet",
            )
        })?;
        let c1 = self.peek_n(text, 1).ok_or_else(|| {
            error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "tag",
                "did not find URI escaped octet",
            )
        })?;
        let c2 = self.peek_n(text, 2).ok_or_else(|| {
            error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                "tag",
                "did not find URI escaped octet",
            )
        })?;
        if !(c0.value == '%' && c1.value.is_ascii_hexdigit() && c2.value.is_ascii_hexdigit()) {
            return Err(error::invalid(
                source,
                Self::original(self.offset, source.bytes().len()),
                if directive { "tag-directive" } else { "tag" },
                "did not find URI escaped octet",
            ));
        }
        // The raw triplet is the identity: `%62` stays `%62`, never `b`.
        uri.push('%');
        uri.push(c1.value);
        uri.push(c2.value);
        self.skip(text);
        self.skip(text);
        self.skip(text);
        Ok(())
    }

    fn scan_block_scalar(
        &mut self,
        text: &[u8],
        start: usize,
        source: ResolvedSource<'_>,

        literal: bool,
    ) -> Result<String, CodecError> {
        let mut value = String::new();
        let mut leading_break = String::new();
        let mut trailing_breaks = String::new();
        let mut chomping = 0i8;
        let mut increment = 0usize;
        self.skip(text); // '|' or '>'
        // Chomping and indentation indicators, in either order.
        if self.peek(text).is_some_and(|c| c.value == '+' || c.value == '-') {
            chomping = if self.peek(text).is_some_and(|c| c.value == '+') {
                1
            } else {
                -1
            };
            self.skip(text);
            if self.peek(text).is_some_and(|c| c.value.is_ascii_digit()) {
                if self.peek(text).is_some_and(|c| c.value == '0') {
                    return Err(error::invalid(
                        source,
                        Self::original(start, source.bytes().len()),
                        "block-scalar",
                        "found an indentation indicator equal to 0",
                    ));
                }
                increment = usize::from(self.skip(text).expect("digit").value as u8 - b'0');
            }
        } else if self.peek(text).is_some_and(|c| c.value.is_ascii_digit()) {
            if self.peek(text).is_some_and(|c| c.value == '0') {
                return Err(error::invalid(
                    source,
                    Self::original(start, source.bytes().len()),
                    "block-scalar",
                    "found an indentation indicator equal to 0",
                ));
            }
            increment = usize::from(self.skip(text).expect("digit").value as u8 - b'0');
            if self.peek(text).is_some_and(|c| c.value == '+' || c.value == '-') {
                chomping = if self.peek(text).is_some_and(|c| c.value == '+') {
                    1
                } else {
                    -1
                };
                self.skip(text);
            }
        }
        // Eat whitespace and comments to the end of the line.
        let mut separated = false;
        while self.peek(text).is_some_and(|c| is_blank(c.value)) {
            separated = true;
            self.skip(text);
        }
        if self.peek(text).is_some_and(|c| c.value == '#') && separated {
            while let Some(c) = self.peek(text) {
                if is_line_break(c.value) {
                    break;
                }
                self.skip(text);
            }
        }
        // End of input counts as a line break here (the reference's IS_BREAKZ check): `--- |1-` at EOF is a valid empty
        // block scalar.
        if !self
            .peek(text)
            .is_none_or(|c| is_line_break(c.value) || c.value == '\0')
        {
            return Err(error::invalid(
                source,
                Self::original(start, source.bytes().len()),
                "block-scalar",
                "did not find expected comment or line break",
            ));
        }
        if let Some(c) = self.peek(text)
            && is_line_break(c.value)
        {
            self.skip_line(text);
        }
        // The content indentation: the explicit increment, else auto-detected.
        let mut indent = if increment > 0 {
            if self.indent == INITIAL_INDENT {
                increment
            } else {
                usize::try_from(self.indent)
                    .map_err(|_| CodecError::new(CodecFailureKind::Overflow))?
                    .checked_add(increment)
                    .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?
            }
        } else {
            0
        };
        // Scan the leading breaks and determine indentation if needed.
        self.scan_block_scalar_breaks(text, &mut indent, &mut trailing_breaks, source)?;
        // Scan the content lines.
        let mut leading_blank = false;
        while self
            .peek(text)
            .is_some_and(|c| self.column == indent && c.value != '\0')
        {
            // A document indicator (`---`/`...`) at column 0 ends the block scalar even when the scalar's content
            // indent is 0 (a top-level block scalar), so a following document marker is not consumed as content.
            if self.column == 0 {
                let dash = self.peek_z(text, 0) == '-' && self.peek_z(text, 1) == '-' && self.peek_z(text, 2) == '-';
                let dot = self.peek_z(text, 0) == '.' && self.peek_z(text, 1) == '.' && self.peek_z(text, 2) == '.';
                if (dash || dot) && is_blank_or_z(self.peek_z(text, 3)) {
                    break;
                }
            }
            // We are at the beginning of a non-empty line.
            let trailing_blank = self.peek(text).is_some_and(|c| is_blank(c.value));
            // Check if we need to fold the leading line break.
            if !literal && leading_break.starts_with('\n') && !leading_blank && !trailing_blank {
                // Do we need to join the lines by space?
                if trailing_breaks.is_empty() {
                    value.push(' ');
                }
                leading_break.clear();
            } else {
                value.push_str(&leading_break);
                leading_break.clear();
            }
            // Append the remaining line breaks.
            value.push_str(&trailing_breaks);
            trailing_breaks.clear();
            // Is it a leading whitespace?
            leading_blank = self.peek(text).is_some_and(|c| is_blank(c.value));
            // Consume the current line.
            while let Some(c) = self.peek(text) {
                if is_line_break(c.value) || c.value == '\0' {
                    break;
                }
                value.push(c.value);
                self.skip(text);
            }
            // Consume the line break.
            if let Some(c) = self.peek(text)
                && is_line_break(c.value)
            {
                leading_break.clear();
                self.take_content_break(text, &mut leading_break);
            }
            // Eat the following indentation spaces and line breaks.
            self.scan_block_scalar_breaks(text, &mut indent, &mut trailing_breaks, source)?;
        }
        // Chomp the tail.
        if chomping != -1 {
            value.push_str(&leading_break);
        }
        if chomping == 1 {
            value.push_str(&trailing_breaks);
        }
        Ok(value)
    }

    /// Eats indentation spaces and line breaks for a block scalar, determining the content indentation when it was not
    /// explicit.
    fn scan_block_scalar_breaks(
        &mut self,
        text: &[u8],
        indent: &mut usize,
        breaks: &mut String,
        source: ResolvedSource<'_>,
    ) -> Result<(), CodecError> {
        let mut max_indent = 0usize;
        loop {
            while let Some(c) = self.peek(text) {
                if (*indent == 0 || self.column < *indent) && c.value == ' ' {
                    self.skip(text);
                } else {
                    break;
                }
            }
            if self.column > max_indent {
                max_indent = self.column;
            }
            // A tab inside an EXPLICIT indentation region is an error; while auto-detecting (`*indent == 0`) a tab is a
            // content boundary ONLY when a leading space established the indentation — the corpus reads ` \tbar` as
            // indent 1 plus `\tbar` content, but a bare `\t` at the indent position (`foo: |\n\t\nbar`) is a
            // tab-as-indentation error.
            if let Some(c) = self.peek(text)
                && c.value == '\t'
                && (*indent != 0 || max_indent == 0)
                && (*indent == 0 || self.column < *indent)
            {
                return Err(error::invalid(
                    source,
                    Self::original(self.offset, source.bytes().len()),
                    "block-scalar",
                    "found a tab character where an indentation space is expected",
                ));
            }
            if !self.peek(text).is_some_and(|c| is_line_break(c.value)) {
                break;
            }
            self.take_content_break(text, breaks);
        }
        if *indent == 0 {
            *indent = max_indent;
            if self.indent != INITIAL_INDENT {
                let floor = usize::try_from(self.indent).unwrap_or(usize::MAX).saturating_add(1);
                if *indent < floor {
                    *indent = floor;
                }
                // The reference implementation floors the detected indent at
                // 1. The corpus's spec reading allows a TOP-LEVEL block scalar to take content at column 0 (`--- >`
                //    then a bare line), so the floor applies only inside a container.
                if *indent < 1 {
                    *indent = 1;
                }
            }
        }
        Ok(())
    }

    /// Scans a single- or double-quoted scalar.
    fn scan_flow_scalar(
        &mut self,
        text: &[u8],
        start: usize,
        source: ResolvedSource<'_>,

        single: bool,
        maps_to_source: bool,
    ) -> Result<ScannedScalar, CodecError> {
        // The decoded value. `None` while the scan is still byte-exact with the source: an escape-free, fold-free
        // quoted scalar IS its inner content span, so nothing is owned until the first escape or fold diverges the
        // bytes.
        let mut value: Option<String> = None;
        // An escape was decoded (`''`, `\`, `\<break>`): the decoded bytes differ from the source, so the scalar cannot
        // name its span.
        let mut escaped = false;
        // A line break was joined into the value (a fold): same consequence.
        let mut folded = false;
        let mut leading_break = String::new();
        let mut trailing_breaks = String::new();
        let mut whitespaces = String::new();
        self.skip(text); // the left quote
        let inner_start = self.offset;
        // The decoded offset just past the last CONTENT byte consumed (a fold's materialization owns the accumulated
        // span from `inner_start` to here; trailing same-line whitespace lives beyond it and is handled by the joins).
        let mut content_end = inner_start;
        loop {
            // Document indicators at the start of a line are errors.
            if self.column == 0 {
                let dash = self.peek_z(text, 0) == '-' && self.peek_z(text, 1) == '-' && self.peek_z(text, 2) == '-';
                let dot = self.peek_z(text, 0) == '.' && self.peek_z(text, 1) == '.' && self.peek_z(text, 2) == '.';
                if (dash || dot) && is_blank_or_z(self.peek_z(text, 3)) {
                    return Err(error::invalid(
                        source,
                        Self::original(start, source.bytes().len()),
                        "quoted-scalar",
                        "found unexpected document indicator",
                    ));
                }
            }
            if self.peek(text).is_none() {
                return Err(error::invalid(
                    source,
                    Self::original(start, source.bytes().len()),
                    "quoted-scalar",
                    "found unexpected end of stream",
                ));
            }
            let mut leading_blanks = false;
            // Consume non-blank characters.
            while let Some(c) = self.peek(text) {
                if is_blank_or_z(c.value) {
                    break;
                }
                if single && c.value == '\'' && self.peek_n(text, 1).is_some_and(|c| c.value == '\'') {
                    escaped = true;
                    // The value is still span-backed (no break has been consumed), so the cursor covers every
                    // accumulated content byte INCLUDING the pending same-line whitespace the joins skipped; own
                    // exactly that span.
                    let v = value.get_or_insert_with(|| owned_text(text, inner_start, self.offset));
                    v.push('\'');
                    self.skip(text);
                    self.skip(text);
                } else if c.value == (if single { '\'' } else { '"' }) {
                    break;
                } else if !single && c.value == '\\' && self.peek_n(text, 1).is_some_and(|c| is_line_break(c.value)) {
                    // A `\<break>` continuation contributes NO character to the value; own the accumulated span
                    // (cursor-inclusive, pending same-line whitespace included) now so the tail (the blank loop's
                    // trailing breaks, or nothing) joins into the right prefix.
                    escaped = true;
                    let _ = value.get_or_insert_with(|| owned_text(text, inner_start, self.offset));
                    self.skip(text);
                    self.skip_line(text);
                    leading_blanks = true;
                    break;
                } else if !single && c.value == '\\' {
                    escaped = true;
                    let v = value.get_or_insert_with(|| owned_text(text, inner_start, self.offset));
                    self.scan_escape(text, start, source, v)?;
                } else {
                    if let Some(v) = value.as_mut() {
                        v.push(c.value);
                    }
                    self.skip(text);
                    content_end = self.offset;
                }
            }
            // End of the scalar?
            if self
                .peek(text)
                .is_some_and(|c| c.value == (if single { '\'' } else { '"' }))
            {
                break;
            }
            // Consume blank characters.
            while let Some(c) = self.peek(text) {
                // A real NUL byte (distinct from the end-of-input sentinel, which is `None`) is illegal YAML; the
                // content loop above classifies it blank-or-z and the blank loop used to leave it to neither loop — an
                // infinite spin on `'\0`. Reject it outright; the other scalar paths treat NUL as a terminator, so
                // every path must at least terminate.
                if c.value == '\0' {
                    return Err(error::invalid(
                        source,
                        Self::original(self.offset, source.bytes().len()),
                        "quoted-scalar",
                        "found an illegal NUL byte in a quoted scalar",
                    ));
                }
                if is_blank(c.value) || is_line_break(c.value) {
                    if is_blank(c.value) {
                        // A tab at the start of a continuation line inside a block context is the tab-as-indentation
                        // error (the corpus rejects `foo: "bar\n\tbaz"`); a top-level flow scalar's continuation line
                        // has no block indent floor, so its leading tab is content (`"\n\t3rd"`).
                        if leading_blanks && self.column == 0 && c.value == '\t' && self.indent >= 0 {
                            return Err(error::invalid(
                                source,
                                Self::original(self.offset, source.bytes().len()),
                                "quoted-scalar",
                                "found a tab character that violates indentation",
                            ));
                        }
                        if leading_blanks {
                            self.skip(text);
                        } else {
                            whitespaces.push(c.value);
                            self.skip(text);
                        }
                    } else if !leading_blanks {
                        whitespaces.clear();
                        leading_break.clear();
                        self.take_content_break(text, &mut leading_break);
                        leading_blanks = true;
                    } else {
                        self.take_content_break(text, &mut trailing_breaks);
                    }
                } else {
                    break;
                }
            }
            // A quoted-scalar continuation line must be indented strictly more than the enclosing block's indentation
            // (the corpus's fail reading for `quoted: "a\nb\nc"` at column 0); a blank line or the closing quote at the
            // indent is allowed.
            if leading_blanks
                && self.indent >= 0
                && self.column <= usize::try_from(self.indent).unwrap_or(usize::MAX)
                && !is_line_break(self.peek_z(text, 0))
                && self.peek_z(text, 0) != (if single { '\'' } else { '"' })
            {
                return Err(error::invalid(
                    source,
                    Self::original(self.offset, source.bytes().len()),
                    "quoted-scalar",
                    "found an unindented continuation line in a quoted scalar",
                ));
            }
            // Join whitespaces or fold line breaks.
            if leading_blanks {
                // A fold: the decoded value diverges from the source bytes from here on. Own the accumulated content
                // span, then apply the fold.
                folded = true;
                let v = value.get_or_insert_with(|| owned_text(text, inner_start, content_end));
                if leading_break.starts_with('\n') {
                    if trailing_breaks.is_empty() {
                        v.push(' ');
                    } else {
                        v.push_str(&trailing_breaks);
                        trailing_breaks.clear();
                    }
                    leading_break.clear();
                } else {
                    v.push_str(&leading_break);
                    v.push_str(&trailing_breaks);
                    leading_break.clear();
                    trailing_breaks.clear();
                }
            } else if let Some(v) = value.as_mut() {
                // Same-line whitespace: byte-preserving, so it is only pushed into an already-owned value; while the
                // value is still span-backed the bytes already live in the span (this includes whitespace before the
                // closing quote, which IS part of a quoted scalar's value and of its inner span).
                v.push_str(&whitespaces);
                whitespaces.clear();
            } else {
                whitespaces.clear();
            }
        }
        self.skip(text); // the right quote
        let inner_end = self.offset.saturating_sub(1);
        if !escaped && !folded && maps_to_source {
            let span = Span::try_from_usize(inner_start, inner_end)
                .map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
            Ok(ScannedScalar::Span(span))
        } else {
            // With no escape and no fold the decoded value IS the full inner content (whitespace joins included) —
            // materialize the span; with an escape/fold the owned value was built as it happened.
            let owned = value.unwrap_or_else(|| owned_text(text, inner_start, inner_end));
            Ok(ScannedScalar::Owned(owned))
        }
    }

    /// Scans one double-quote escape sequence. The cursor is on `\`.
    fn scan_escape(
        &mut self,
        text: &[u8],
        start: usize,
        source: ResolvedSource<'_>,

        value: &mut String,
    ) -> Result<(), CodecError> {
        let esc = self.peek_n(text, 1).map_or('\0', |c| c.value);
        let simple = match esc {
            '0' => Some('\0'),
            'a' => Some('\u{07}'),
            'b' => Some('\u{08}'),
            't' | '\t' => Some('\u{09}'),
            'n' => Some('\n'),
            'v' => Some('\u{0B}'),
            'f' => Some('\u{0C}'),
            'r' => Some('\r'),
            'e' => Some('\u{1B}'),
            ' ' => Some(' '),
            '"' => Some('"'),
            '/' => Some('/'),
            '\\' => Some('\\'),
            'N' => Some('\u{85}'),
            '_' => Some('\u{A0}'),
            'L' => Some('\u{2028}'),
            'P' => Some('\u{2029}'),
            'x' | 'u' | 'U' => None,
            _ => {
                return Err(error::invalid(
                    source,
                    Self::original(start, source.bytes().len()),
                    "quoted-scalar",
                    "found unknown escape character",
                ));
            }
        };
        if let Some(ch) = simple {
            value.push(ch);
            self.skip(text);
            self.skip(text);
            return Ok(());
        }
        let code_length = match esc {
            'x' => 2usize,
            'u' => 4,
            _ => 8,
        };
        // Consume the escape code: the digits sit at offsets +2 .. +2+n.
        let mut code: u32 = 0;
        for k in 0..code_length {
            let c = self.peek_n(text, 2 + k).map(|c| c.value);
            let Some(c) = c else {
                return Err(error::invalid(
                    source,
                    Self::original(start, source.bytes().len()),
                    "quoted-scalar",
                    "did not find expected hexadecimal number",
                ));
            };
            let Some(digit) = c.to_digit(16) else {
                return Err(error::invalid(
                    source,
                    Self::original(start, source.bytes().len()),
                    "quoted-scalar",
                    "did not find expected hexadecimal number",
                ));
            };
            code = code * 16 + digit;
        }
        self.skip(text); // '\'
        self.skip(text); // 'x'/'u'/'U'
        for _ in 0..code_length {
            self.skip(text);
        }
        if (0xD800..=0xDFFF).contains(&code) || code > 0x0010_FFFF {
            return Err(error::invalid(
                source,
                Self::original(start, source.bytes().len()),
                "quoted-scalar",
                "found invalid Unicode character escape code",
            ));
        }
        let ch = char::from_u32(code).ok_or_else(|| CodecError::new(CodecFailureKind::InvalidInput))?;
        value.push(ch);
        Ok(())
    }

    /// Scans a plain scalar. Returns the decoded value and the CONTENT end: the decoded offset just past the scalar's
    /// last content byte. The content end is what the token's span must end at — the scan consumes trailing separation,
    /// line breaks (while probing for a continuation line), and inline comments past the value, and none of that is
    /// part of the scalar.
    fn scan_plain_scalar(
        &mut self,
        text: &[u8],
        start: usize,
        source: ResolvedSource<'_>,

        maps_to_source: bool,
    ) -> Result<(ScannedScalar, usize), CodecError> {
        // The decoded value. `None` while the scan is still byte-exact with the source: a plain scalar that never folds
        // IS its content span, so nothing is owned until the first fold diverges the bytes.
        let mut value: Option<String> = None;
        // A line break was joined into the value (a fold): the decoded bytes no longer equal the source, so the scalar
        // cannot name its span.
        let mut folded = false;
        let mut content_end = start;
        let mut leading_break = String::new();
        let mut trailing_breaks = String::new();
        let mut whitespaces = String::new();
        let mut leading_blanks = false;
        // The reference scanner's effective continuation indent: the container indent + 1, and ZERO at the top level
        // (`indent == -1`). A top-level plain scalar therefore continues on a following line even at column 0 — `a\nb`
        // is one scalar `"a b"` — while any deeper scalar requires its continuation lines to be indented strictly more
        // than the container.
        let indent = if self.indent == INITIAL_INDENT {
            0
        } else {
            usize::try_from(self.indent).unwrap_or(usize::MAX).saturating_add(1)
        };
        loop {
            // Document indicators at the start of a line end the scalar.
            if self.column == 0 {
                let dash = self.peek_z(text, 0) == '-' && self.peek_z(text, 1) == '-' && self.peek_z(text, 2) == '-';
                let dot = self.peek_z(text, 0) == '.' && self.peek_z(text, 1) == '.' && self.peek_z(text, 2) == '.';
                if (dash || dot) && is_blank_or_z(self.peek_z(text, 3)) {
                    break;
                }
            }
            // A comment ends the scalar. The `#` here always follows separation whitespace or a line break (a `#`
            // directly after a non-blank would have been consumed above as part of the scalar), so it is a valid
            // comment; consume it to end of line so the next token scan does not mis-read it as unseparated.
            if self.peek(text).is_some_and(|c| c.value == '#') {
                let start = self.offset;
                while let Some(c) = self.peek(text) {
                    if is_line_break(c.value) {
                        break;
                    }
                    self.skip(text);
                }
                self.pending_comments.push(Span::from_usize(start, self.offset));
                break;
            }
            // Consume non-blank characters.
            while let Some(c) = self.peek(text) {
                if is_blank_or_z(c.value) {
                    break;
                }
                // "x:" + one of '?[]{}' in the flow context is an error (the reference's IS_FLOW_INDICATOR check). A
                // ':' followed by ',' or ']' or '}' is a VALUE indicator with an empty value (the corpus's `omitted
                // value:,` reading), so it ends the scalar as a key and lets the parser pair the empty value.
                if self.flow_level > 0
                    && c.value == ':'
                    && self.peek_n(text, 1).is_some_and(|c| matches!(c.value, '?' | '[' | '{'))
                {
                    return Err(error::invalid(
                        source,
                        Self::original(start, source.bytes().len()),
                        "plain-scalar",
                        "found unexpected ':'",
                    ));
                }
                // Indicators that may end a plain scalar. A ':' followed by a blank, or by a flow separator/terminator
                // in flow context, ends the scalar so it can serve as a mapping key with an empty value (`omitted
                // value:,` -> key "omitted value").
                if (c.value == ':' && is_blank_or_z(self.peek_z(text, 1)))
                    || (self.flow_level > 0
                        && (matches!(c.value, ',' | '[' | ']' | '{' | '}')
                            || (c.value == ':' && matches!(self.peek_z(text, 1), ',' | ']' | '}'))))
                {
                    break;
                }
                // Join whitespaces and breaks.
                if leading_blanks || !whitespaces.is_empty() {
                    if leading_blanks {
                        // A fold: the decoded value diverges from the source bytes from here on. Own the accumulated
                        // content span, then apply the fold.
                        folded = true;
                        let v = value.get_or_insert_with(|| owned_text(text, start, content_end));
                        if leading_break.starts_with('\n') {
                            if trailing_breaks.is_empty() {
                                v.push(' ');
                            } else {
                                v.push_str(&trailing_breaks);
                                trailing_breaks.clear();
                            }
                            leading_break.clear();
                        } else {
                            v.push_str(&leading_break);
                            v.push_str(&trailing_breaks);
                            leading_break.clear();
                            trailing_breaks.clear();
                        }
                        leading_blanks = false;
                    } else if let Some(v) = value.as_mut() {
                        // Same-line whitespace: byte-preserving, so it is only pushed into an already-owned value;
                        // while the value is still span-backed the bytes already live in the span.
                        v.push_str(&whitespaces);
                        whitespaces.clear();
                    } else {
                        whitespaces.clear();
                    }
                }
                if let Some(v) = value.as_mut() {
                    v.push(c.value);
                }
                self.skip(text);
                // The content end advances ONLY with content: a later loop iteration may consume nothing (EOF or an
                // indicator right after a break run), and its cursor is past the trailing break, not past content.
                content_end = self.offset;
            }
            // Is it the end?
            if !self
                .peek(text)
                .is_some_and(|c| is_blank(c.value) || is_line_break(c.value))
            {
                break;
            }
            // Consume blank characters.
            while let Some(c) = self.peek(text) {
                if is_blank(c.value) || is_line_break(c.value) {
                    if is_blank(c.value) {
                        // Tabs that violate indentation are an error, except on an otherwise-blank line (a blank line
                        // between block entries may contain tabs — the corpus accepts `foo: 1\n\t\nbar: 2`).
                        if leading_blanks
                            && self.column < indent
                            && c.value == '\t'
                            && self.peek_n(text, 1).is_some_and(|n| !is_line_break(n.value))
                        {
                            return Err(error::invalid(
                                source,
                                Self::original(start, source.bytes().len()),
                                "plain-scalar",
                                "found a tab character that violates indentation",
                            ));
                        }
                        if leading_blanks {
                            self.skip(text);
                        } else {
                            whitespaces.push(c.value);
                            self.skip(text);
                        }
                    } else if !leading_blanks {
                        whitespaces.clear();
                        leading_break.clear();
                        self.take_content_break(text, &mut leading_break);
                        leading_blanks = true;
                    } else {
                        self.take_content_break(text, &mut trailing_breaks);
                    }
                } else {
                    break;
                }
            }
            // Check the indentation level.
            if self.flow_level == 0 && self.column < indent {
                // The indentation whitespace of this continuation line was already consumed above, so a `#` at this
                // point follows separation whitespace: it is a valid comment. Consume it so the next token scan does
                // not mis-read it as unseparated (the same law as the scalar-body `#` above), and record it as the
                // document's comment.
                if self.peek(text).is_some_and(|c| c.value == '#') {
                    let start = self.offset;
                    while let Some(c) = self.peek(text) {
                        if is_line_break(c.value) {
                            break;
                        }
                        self.skip(text);
                    }
                    self.pending_comments.push(Span::from_usize(start, self.offset));
                }
                break;
            }
        }
        if leading_blanks {
            self.simple_key_allowed = true;
        }
        if !folded && maps_to_source {
            let span =
                Span::try_from_usize(start, content_end).map_err(|_| CodecError::new(CodecFailureKind::Overflow))?;
            Ok((ScannedScalar::Span(span), content_end))
        } else {
            let owned = value.unwrap_or_else(|| owned_text(text, start, content_end));
            Ok((ScannedScalar::Owned(owned), content_end))
        }
    }
}

/// A decoded character plus its width in decoded bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Char {
    /// The Unicode scalar value.
    pub(crate) value: char,
    /// Width in decoded bytes (1..=4).
    pub(crate) width: usize,
}

/// Owns the decoded text at a span. The scanned text is validated UTF-8 up front and every span end here is a character
/// boundary (the cursor only moves past whole characters), so the slice is always valid UTF-8.
fn owned_text(text: &[u8], start: usize, end: usize) -> String {
    String::from(core::str::from_utf8(&text[start..end]).expect("scanned text is valid UTF-8"))
}

// --------------------------------------------------------------------------- Character classification
// ---------------------------------------------------------------------------

#[inline]
fn utf8_width(byte: u8) -> usize {
    if byte < 0x80 {
        1
    } else if byte >> 5 == 0b110 {
        2
    } else if byte >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[inline]
fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

#[inline]
fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// The reference scanner's `IS_BLANKZ`: blank, line break, or NUL. A plain scalar ends at any of the three.
#[inline]
fn is_blank_or_z(c: char) -> bool {
    is_blank(c) || is_line_break(c) || c == '\0'
}

/// Whether a character is a legal anchor/alias name character (`ns-anchor-char`: any non-flow-indicator printable).
#[inline]
fn is_anchor_char(c: char) -> bool {
    !matches!(c, ',' | '[' | ']' | '{' | '}') && !is_blank_or_z(c) && !is_line_break(c)
}

/// Whether a character can start a plain scalar at the current position.
fn can_start_plain(value: char, next_not_blank: impl Fn() -> bool) -> bool {
    if is_blank_or_z(value) {
        return false;
    }
    if matches!(
        value,
        ',' | '[' | ']' | '{' | '}' | '#' | '&' | '*' | '!' | '|' | '>' | '\'' | '"' | '%' | '@' | '`'
    ) {
        return false;
    }
    if value == '-' && next_not_blank() {
        return true;
    }
    if (value == '?' || value == ':') && next_not_blank() {
        // A plain scalar may start with `?` or `:` when the next char is a non-blank (the corpus's `?foo` / `:x`
        // reading in both flow and block context).
        return true;
    }
    !matches!(value, '-' | '?' | ':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    fn resources<'a>() -> ResourceContext<'a> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("context")
    }

    fn source(bytes: &'static [u8]) -> ResolvedSource<'static> {
        ResolvedSource::new(SourceRef::new(SourceId::new(1), SourceKind::Input), "t", bytes, 0)
    }

    #[test]
    fn scanner_terminates_on_tiny_input() {
        let src = source(b"42\n");
        let mut resources = resources();
        let decoded = crate::scan::DecodedSource::try_new(src, &resources).expect("decoded");
        let start = decoded.start_offset();
        let mut scanner = crate::scan::Scanner::try_new(start);
        let text = decoded.text(src);
        let mut count = 0usize;
        while let ScanPoll::Token(token) = scanner.poll(text, src, &mut resources).expect("poll") {
            count += 1;
            assert!(count <= 100, "scanner spin: more than 100 tokens");
            let _ = token;
        }
        assert!(count >= 3, "expected stream tokens, got {count}");
    }

    #[test]
    fn a_nul_byte_in_a_quoted_scalar_is_rejected_not_spun_on() {
        // `'\0` (a lone single quote followed by a NUL byte) used to spin forever — the content loop classified NUL
        // blank-or-z, the blank loop classified it neither blank nor line break, and neither loop advanced the cursor.
        // The scanner must terminate: reject the NUL outright.
        let src = source(b"'\0");
        let mut resources = resources();
        let decoded = crate::scan::DecodedSource::try_new(src, &resources).expect("decoded");
        let start = decoded.start_offset();
        let mut scanner = crate::scan::Scanner::try_new(start);
        let text = decoded.text(src);
        let mut polls = 0usize;
        loop {
            polls += 1;
            assert!(polls <= 1000, "scanner spins on a NUL inside a quoted scalar");
            match scanner.poll(text, src, &mut resources) {
                // A rejection is the valid outcome; Done terminates the loop or the poll bound above does.
                Err(_) | Ok(ScanPoll::Done) => break,
                Ok(ScanPoll::Token(_)) => {}
            }
        }
    }

    #[test]
    fn scanner_terminates_on_mapping() {
        let src = source(b"name: Ada\nage: 37\n");
        let mut resources = resources();
        let decoded = crate::scan::DecodedSource::try_new(src, &resources).expect("decoded");
        let start = decoded.start_offset();
        let mut scanner = crate::scan::Scanner::try_new(start);
        let text = decoded.text(src);
        let mut count = 0usize;
        while let ScanPoll::Token(token) = scanner.poll(text, src, &mut resources).expect("poll") {
            count += 1;
            assert!(count < 40, "scanner spin: more than 40 tokens");
            let _ = token;
        }
        assert!(count >= 5, "expected many tokens, got {count}");
    }

    /// The decoded-buffer charge law: a UTF-16/32 transcode books its resident bytes once at construction under
    /// `Retained` and releases exactly those bytes when the descriptor drops; a UTF-8 (identity) descriptor books
    /// nothing.
    #[test]
    fn decoded_buffers_are_charged_at_construction_and_released_on_drop() {
        let resources = resources();
        let account = resources.account().try_share().expect("share");
        let retained = || account.snapshot().memory(MemoryCategory::Retained).current();
        let before = retained();

        // UTF-16LE with BOM: every ASCII unit decodes to one byte, so the buffers hold one byte and one u32 per payload
        // character.
        let utf16: Vec<u8> = {
            let mut bytes = vec![0xFF, 0xFE];
            for unit in "a: 1\nb: 2\n".encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes
        };
        let leaked: &'static [u8] = Box::leak(utf16.into_boxed_slice());
        let src = source(leaked);
        let charged = {
            let decoded = crate::scan::DecodedSource::try_new(src, &resources).expect("decoded");
            assert!(!decoded.maps_to_source(), "a transcode is not identity");
            let delta = retained() - before;
            assert!(delta > 0, "a transcode must book residency, got {delta}");
            delta
        };
        // The drop gives back exactly what construction booked.
        assert_eq!(retained(), before, "drop releases the {charged} charged bytes");

        // A UTF-8 source is an identity descriptor: no buffer, no charge.
        let decoded = crate::scan::DecodedSource::try_new(source(b"a: 1\n"), &resources).expect("decoded");
        assert!(decoded.maps_to_source(), "utf-8 maps to the source");
        assert_eq!(retained(), before, "an identity descriptor must not charge");
    }

    /// A transcode that would cross the memory ceiling refuses at the ledger instead of handing back an uncharged
    /// buffer.
    #[test]
    fn a_transcode_past_the_memory_ceiling_refuses_at_the_ledger() {
        // A 1 KiB ceiling admits the ledger allocation itself (a few hundred counter bytes) but no transcode of a 4
        // Ki-character source, whose buffer+map reservation is ~20 KiB.
        let resources = ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 1024, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("context");
        let utf16: Vec<u8> = {
            let mut bytes = vec![0xFF, 0xFE];
            for unit in "k".repeat(4096).encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes
        };
        let leaked: &'static [u8] = Box::leak(utf16.into_boxed_slice());
        let src = source(leaked);
        let Err(error) = crate::scan::DecodedSource::try_new(src, &resources) else {
            panic!("the ceiling cannot admit this transcode");
        };
        assert!(
            matches!(
                error.kind(),
                CodecFailureKind::Resource(jqf_resource::ResourceError::LimitExceeded { .. })
            ),
            "expected the memory-ceiling refusal, got {error:?}"
        );
    }
}
