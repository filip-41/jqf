//! The WHATWG HTML tokenizer (§13.2.5 of the HTML Standard), a faithful state-machine port.
//!
//! Every emitted token carries the exact span it consumed in the input, and the token stream covers the input
//! contiguously — the token at index `i` ends exactly where index `i + 1` begins.
//!
//! The tokenizer consumes DECODED text (the codec front end applies the WHATWG encoding determination first). For
//! identity decodes — valid UTF-8 and the single-byte encodings, which map one byte to one scalar — spans are raw-byte
//! spans. The one non-identity case is invalid UTF-8, whose U+FFFD replacement shortens the decoded text; spans then
//! record DECODED coordinates (the monotone original-byte map for that case is not served).

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use jqf_codec_core::byte_scan::{StopSet, prefix_len};

/// The per-step text-scan cap (see [`Tokenizer::scan_text_run`]): at most this many bytes of a text run are consumed in
/// one tokenizer step, so the session's one-admission-check-per-step drive stays a bounded cooperative quantum. Equals
/// the work meter's linear item bound.
const MAX_TEXT_SCAN_BYTES: usize = 256;

/// The HTML text terminators: `<`, `&`, NUL, `\r` (the WHATWG tokenizer's text-data stop set).
#[derive(Clone, Copy)]
struct Html;
impl StopSet for Html {
    const EQ: [u8; 8] = [b'<', b'&', 0x00, b'\r', 0, 0, 0, 0];
    const EQ_LEN: u8 = 4;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// One token as the tree builder sees it.
#[derive(Debug)]
pub enum TokenKind {
    /// A doctype token with the recovered identifiers and the force-quirks flag.
    Doctype {
        /// The doctype name, lowercased.
        name: Option<String>,
        /// The public identifier, when present.
        public_identifier: Option<String>,
        /// The system identifier, when present.
        system_identifier: Option<String>,
        /// The token's force-quirks flag.
        force_quirks: bool,
        /// The doctype's correctness evidence (the fifth field): false when any doctype-state parse error occurred.
        correct: bool,
    },
    /// A start tag.
    StartTag {
        /// The lowercased tag name.
        name: String,
        /// The recovered attributes (first-wins duplicates dropped).
        attributes: Vec<Attribute>,
        /// The self-closing flag.
        self_closing: bool,
    },
    /// An end tag.
    EndTag {
        /// The lowercased tag name.
        name: String,
    },
    /// A comment with its recovered data.
    Comment {
        /// The comment data.
        data: String,
    },
    /// A run of decoded characters (the raw span covers the run's source).
    Character {
        /// The decoded character data.
        data: String,
    },
    /// End of input.
    Eof,
}

/// One recovered attribute.
#[derive(Clone, Debug)]
pub struct Attribute {
    /// The lowercased attribute name.
    pub name: String,
    /// The decoded attribute value.
    pub value: String,
}

/// One token plus its exact input span.
#[derive(Debug)]
pub struct Token {
    /// The token's kind.
    pub kind: TokenKind,
    /// The token's exact input span.
    pub span: Range<usize>,
}

/// The tokenizer's entry states.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitialState {
    /// The data state.
    Data,
    /// The RCDATA state (`title`/`textarea` content).
    Rcdata,
    /// The RAWTEXT state (`style`/`xmp`/`iframe`/`noembed`/`noframes`).
    Rawtext,
    /// The script data state.
    ScriptData,
    /// The PLAINTEXT state.
    Plaintext,
    /// The CDATA section state (foreign content).
    CdataSection,
}

/// The tokenizer states (the spec's state names, abbreviated).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Data,
    Rcdata,
    Rawtext,
    ScriptData,
    Plaintext,
    TagOpen,
    EndTagOpen,
    TagName,
    RcdataLessThanSign,
    RcdataEndTagOpen,
    RcdataEndTagName,
    RawtextLessThanSign,
    RawtextEndTagOpen,
    RawtextEndTagName,
    ScriptDataLessThanSign,
    ScriptDataEndTagOpen,
    ScriptDataEndTagName,
    ScriptDataEscapeStart,
    ScriptDataEscapeStartDash,
    ScriptDataEscaped,
    ScriptDataEscapedDash,
    ScriptDataEscapedDashDash,
    ScriptDataEscapedLessThanSign,
    ScriptDataEscapedEndTagOpen,
    ScriptDataEscapedEndTagName,
    ScriptDataDoubleEscapeStart,
    ScriptDataDoubleEscaped,
    ScriptDataDoubleEscapedDash,
    ScriptDataDoubleEscapedDashDash,
    ScriptDataDoubleEscapedLessThanSign,
    ScriptDataDoubleEscapeEnd,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDoubleQuoted,
    AttributeValueSingleQuoted,
    AttributeValueUnquoted,
    AfterAttributeValueQuoted,
    SelfClosingStartTag,
    BogusComment,
    MarkupDeclarationOpen,
    CommentStart,
    CommentStartDash,
    Comment,
    CommentLessThanSign,
    CommentLessThanSignBang,
    CommentLessThanSignBangDash,
    CommentLessThanSignBangDashDash,
    CommentEndDash,
    CommentEnd,
    CommentEndBang,
    Doctype,
    BeforeDoctypeName,
    DoctypeName,
    AfterDoctypeName,
    AfterDoctypePublicKeyword,
    BeforeDoctypePublicIdentifier,
    DoctypePublicIdentifierDoubleQuoted,
    DoctypePublicIdentifierSingleQuoted,
    AfterDoctypePublicIdentifier,
    BetweenDoctypePublicAndSystemIdentifiers,
    AfterDoctypeSystemKeyword,
    BeforeDoctypeSystemIdentifier,
    DoctypeSystemIdentifierDoubleQuoted,
    DoctypeSystemIdentifierSingleQuoted,
    AfterDoctypeSystemIdentifier,
    BogusDoctype,
    CdataSection,
    CdataSectionBracket,
    CdataSectionEnd,
}

/// The tokenizer over one decoded input.
pub struct Tokenizer {
    /// The decoded input text. The tokenizer OWNS it (rather than borrowing) so a session can hold the tokenizer across
    /// cooperative polls without a self-referential borrow; the eager routes move their owned decoded `String` in and
    /// pay nothing extra.
    input: String,
    at: usize,
    state: State,
    last_start_tag: Option<String>,
    /// The temporary buffer (spec: the "temporary buffer").
    temp: String,
    /// The current attribute being collected.
    attr_name: String,
    attr_value: String,
    /// The accumulated attribute list of the tag being parsed.
    attrs: Vec<Attribute>,
    /// The collected character run, flushed at state boundaries.
    chars: String,
    /// Whether the current run consists of whitespace only (the 0.90 tokenizer's split: a run that STARTS with
    /// whitespace ends at the first non-whitespace character, so the whitespace is its own token and the modes'
    /// per-token whitespace laws see it alone).
    ws_segment: bool,
    /// The input position where the current run began (the position after the last flush or non-character token).
    segment_start: usize,
    /// The current tag token being built.
    tag_name: String,
    /// The input position of the `<` that opened the comment being parsed.
    comment_start: usize,
    tag_start: usize,
    self_closing: bool,
    /// The doctype token being built.
    doctype_name: Option<String>,
    /// The doctype's correctness evidence (false on any doctype parse error, per the law).
    doctype_correct: bool,
    doctype_public: Option<String>,
    doctype_system: Option<String>,
    force_quirks: bool,
    /// Whether a character reference is being consumed inside an attribute.
    in_attribute: bool,
    /// Whether the tag being parsed is an end tag.
    in_end_tag: bool,
    /// Whether the text-mode end-tag path already emitted the end tag and the remaining tag tail (attributes, `>`) must
    /// be consumed silently.
    end_tag_pending: bool,
    /// Whether the tokenizer stopped (EOF).
    eof: bool,
    /// Tokens a step produced beyond the one being handed out (a step can emit a character run AND the EOF token in one
    /// transition).
    pending: Vec<Token>,
    /// Whether the tree builder's adjusted current node is a foreign (SVG/MathML) element, which the CDATA-section
    /// state consults. The builder refreshes this before every token.
    foreign_content: bool,
}

/// One named-reference lookup result.
#[derive(Debug)]
struct NamedReference {
    index: usize,
    length: usize,
}

/// The longest entity name that is a prefix of `input[at..]`.
fn longest_named_reference(input: &str, at: usize) -> Option<NamedReference> {
    let rest = &input[at..];
    // Names are ASCII; the longest is 32 chars ("&CounterClockwiseContourIntegral;").
    let max = rest.len().min(34);
    let mut best: Option<NamedReference> = None;
    for length in (1..=max).rev() {
        // The candidate slice must end at a char boundary: the input is decoded text (windows-1252 bytes become
        // multi-byte UTF-8) and a length landing inside one multi-byte char would panic (the fuzz receipt's first
        // defect). Entity names are ASCII, so a non-boundary length can never match anyway.
        if !rest.is_char_boundary(length) {
            continue;
        }
        let candidate = &rest[..length];
        // Only ASCII letters/digits follow `&` in names; skip non-ASCII prefixes quickly.
        if !candidate.is_ascii() {
            continue;
        }
        if let Ok(index) = crate::entities_table::NAMES.binary_search(&candidate) {
            best = Some(NamedReference { index, length });
            break;
        }
    }
    best
}

impl Tokenizer {
    /// Scans ahead to the next byte that stops a text run — `<`, `&`, NUL, CR (the WHATWG text-data stop set, `Html`) —
    /// and appends the clean run to the current character run, verbatim: every scanned byte is an ordinary character
    /// the per-char path would push as-is, and none of the stop bytes can appear inside a multi-byte UTF-8 sequence, so
    /// the run always ends on a char boundary and the appended slice is a valid `&str`. `in_attribute` is false in
    /// every calling state (the tag states own attributes), so the run goes straight into `chars`.
    ///
    /// Returns true when a run was consumed (the caller returns); false when `self.at` sits on a stop byte or at EOF,
    /// in which case the caller's own special-byte handling applies.
    ///
    /// The run is CAPPED: the session drives one admission check per tokenizer step (the cooperative yield law — a long
    /// character run yields across steps rather than running to its end), so one step must stay a bounded quantum. 256
    /// is the work meter's documented linear item bound (256 items per credit), so a step's worst-case work equals the
    /// meter's own per-credit byte law.
    fn scan_text_run(&mut self) -> bool {
        // The slice handed to `prefix_len` is BOUNDED to the cap: the scan's scalar tail runs to the first stop byte of
        // the slice it is given, so an unbounded slice would scalar-walk the whole remaining run per step (O(run²) over
        // the parse) — the module doc's "no scalar head" law says the caller owns the head, and the cap IS that head.
        let input_len = self.input.len();
        let window_end = (self.at + MAX_TEXT_SCAN_BYTES).min(input_len);
        let window = &self.input.as_bytes()[self.at..window_end];
        let run = prefix_len::<Html>(window);
        if run == 0 {
            return false;
        }
        // The window boundary can cut inside a multi-byte UTF-8 sequence, so round the run's end down to the nearest
        // char boundary (at most three bytes back for a 4-byte char, only when the window cut).
        let mut boundary = self.at + run;
        while boundary > self.at && !self.input.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let run = boundary - self.at;
        if run == 0 {
            return false;
        }
        self.at += run;
        self.chars.push_str(&self.input[self.at - run..self.at]);
        true
    }

    /// Constructs the tokenizer in the data state.
    pub fn new(input: String) -> Self {
        Self::with_initial_state(input, InitialState::Data, None)
    }

    /// Constructs the tokenizer in one of the entry states, with an optional last start tag (the conformance harness's
    /// doors).
    pub fn with_initial_state(input: String, initial: InitialState, last_start_tag: Option<&str>) -> Self {
        let state = match initial {
            InitialState::Data => State::Data,
            InitialState::Rcdata => State::Rcdata,
            InitialState::Rawtext => State::Rawtext,
            InitialState::ScriptData => State::ScriptData,
            InitialState::Plaintext => State::Plaintext,
            InitialState::CdataSection => State::CdataSection,
        };
        Self {
            input,
            at: 0,
            state,
            last_start_tag: last_start_tag.map(alloc::string::ToString::to_string),
            temp: String::new(),
            attr_name: String::new(),
            attr_value: String::new(),
            attrs: Vec::new(),
            chars: String::new(),
            ws_segment: false,
            segment_start: 0,
            tag_name: String::new(),
            comment_start: 0,
            tag_start: 0,
            self_closing: false,
            doctype_name: None,
            doctype_correct: true,
            doctype_public: None,
            doctype_system: None,
            force_quirks: false,
            in_attribute: false,
            in_end_tag: false,
            end_tag_pending: false,
            eof: false,
            pending: Vec::new(),
            foreign_content: false,
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.at..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.at += ch.len_utf8();
        Some(ch)
    }

    fn reconsume(&mut self, ch: char) {
        self.at -= ch.len_utf8();
    }

    /// Appends a character to the current character run (or, while a character reference is being consumed inside an
    /// attribute, to the attribute's value). The run's span is the raw input between `segment_start` and the flush
    /// position, so the emitted character's decoded length is irrelevant to the origin map.
    fn emit_char(&mut self, ch: char) {
        if self.in_attribute {
            self.attr_value.push(ch);
        } else {
            self.chars.push(ch);
        }
    }

    /// The CR normalization law: a carriage return in text becomes LF, and an immediately following LF is consumed with
    /// it (both covered by the run's raw span).
    fn emit_normalized_cr(&mut self) {
        if self.input[self.at..].starts_with('\n') {
            self.at += 1;
        }
        self.emit_char('\n');
    }

    /// Consumes a character reference (§13.2.5.73) and appends its result to the current character run. The `&` has
    /// already been consumed.
    fn consume_character_reference(&mut self) {
        match self.peek() {
            None => {
                // `&` at EOF is a literal.
                self.emit_char('&');
            }
            Some('#') => {
                self.bump();
                self.consume_numeric_reference();
            }
            Some(ch) if ch.is_ascii_alphanumeric() => {
                self.consume_named_reference();
            }
            Some(_) => {
                // The ambiguous ampersand state: the `&` is literal, then ASCII alphanumerics and `;` continue as
                // literals.
                self.emit_char('&');
                loop {
                    match self.peek() {
                        Some(ch) if ch.is_ascii_alphanumeric() || ch == ';' => {
                            self.emit_char(ch);
                            self.bump();
                        }
                        _ => break,
                    }
                }
            }
        }
    }

    /// The numeric character reference start states (after `&#`).
    fn consume_numeric_reference(&mut self) {
        let mut hex_marker: Option<char> = None;
        let radix = match self.peek() {
            Some('x' | 'X') => {
                hex_marker = self.peek();
                self.bump();
                16
            }
            _ => 10,
        };
        let mut digits: Vec<char> = Vec::new();
        loop {
            match self.peek() {
                Some(ch) if ch.to_digit(radix).is_some() => {
                    digits.push(ch);
                    self.bump();
                }
                _ => break,
            }
        }
        if digits.is_empty() {
            // No digits: everything consumed is literal, the `&` included.
            self.emit_char('&');
            self.emit_char('#');
            if let Some(marker) = hex_marker {
                self.emit_char(marker);
            }
            return;
        }
        self.finish_numeric_reference(digits, radix);
    }

    /// The numeric character reference end state: consume the optional `;` and resolve the code point.
    fn finish_numeric_reference(&mut self, digits: Vec<char>, radix: u32) {
        let mut value: u32 = 0;
        for digit in &digits {
            let digit_value = digit.to_digit(radix).expect("validated digit");
            value = value.saturating_mul(radix).saturating_add(digit_value);
        }
        if self.peek() == Some(';') {
            self.bump();
        }
        // The numeric reference end state: the C1 controls map through windows-1252, and only null, surrogates, and
        // out-of-range values become U+FFFD. CR, other controls, and noncharacters are KEPT (a parse error, but the
        // code point stands) — the current WHATWG law the suite pins.
        let resolved = if (0x80..=0x9F).contains(&value) {
            WINDOWS_1252_MAP[(value - 0x80) as usize]
        } else if value == 0x00 || (0xD800..=0xDFFF).contains(&value) || value > 0x10FFFF {
            0xFFFD
        } else {
            value
        };
        let ch = char::from_u32(resolved).unwrap_or('\u{FFFD}');
        self.emit_char(ch);
    }

    /// The named character reference state: the longest name prefix, the semicolon rule, and the legacy (no-semicolon)
    /// rule.
    fn consume_named_reference(&mut self) {
        let Some(NamedReference { index, length }) = longest_named_reference(&self.input, self.at) else {
            // Not a named reference: flush the consumed characters.
            self.emit_char('&');
            return;
        };
        let name = crate::entities_table::NAMES[index];
        // The table stores names WITH their semicolon (the spec's first column); a name that does not end in `;` is a
        // legacy entry.
        let legacy = !name.ends_with(';');
        let name_end = self.at + length;
        let legacy_ok = if self.in_attribute {
            // The legacy rule: in an attribute, a no-semicolon match is discarded when the next character is `=` or an
            // ASCII alphanumeric (the ambiguous ampersand).
            !self.input[name_end..].starts_with('=')
                && !self.input[name_end..]
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphanumeric())
        } else {
            true
        };
        if !legacy || legacy_ok {
            self.at += length;
            // The spec's step 5: a legacy match followed by `;` consumes it as part of the reference (unreachable when
            // the `;` form is itself a table entry, which the longest match would have preferred; kept for the spec's
            // completeness).
            if legacy && self.input[self.at..].starts_with(';') {
                self.at += 1;
            }
            for point in crate::entities_table::CODEPOINTS[index] {
                self.emit_char(char::from_u32(*point).unwrap_or('\u{FFFD}'));
            }
        } else {
            // Ambiguous ampersand: the `&` is literal and the rest continues as ordinary text.
            self.emit_char('&');
        }
    }

    /// Emits the pending character run as one token. The span is the raw input from `segment_start` to the current
    /// position, so the run covers every byte the run consumed — literal text, character references, and replacement
    /// characters alike — which is what keeps the token stream contiguous.
    fn flush_chars(&mut self, out: &mut Vec<Token>) {
        if !self.chars.is_empty() {
            let start = self.segment_start;
            let data = core::mem::take(&mut self.chars);
            out.push(Token {
                kind: TokenKind::Character { data },
                span: start..self.at,
            });
        }
        self.segment_start = self.at;
        self.ws_segment = false;
    }

    /// Emits one non-character token and advances the segment boundary past it, so the next character run starts at
    /// this token's end.
    fn emit(&mut self, out: &mut Vec<Token>, kind: TokenKind, start: usize, end: usize) {
        out.push(Token { kind, span: start..end });
        self.segment_start = end;
    }

    /// The spec's "appropriate end tag" test: the tokenizer is parsing an end tag whose name matches the last start
    /// tag.
    fn is_appropriate_end_tag(&self) -> bool {
        self.last_start_tag.as_deref().is_some_and(|last| last == self.tag_name)
    }

    /// Refreshes the foreign-content flag the CDATA-section state consults (the tree builder sets it before every
    /// token).
    pub fn set_foreign_content(&mut self, foreign: bool) {
        self.foreign_content = foreign;
    }

    /// Switches the tokenizer into the text-content state a start tag requires (the tree builder's rule:
    /// `title`/`textarea` → RCDATA, `style`/`xmp`/`iframe`/`noembed`/`noframes` → RAWTEXT, `script` → script data,
    /// `plaintext` → plaintext; every other name is ignored).
    pub fn enter_text_mode(&mut self, name: &str) {
        self.state = match name {
            "title" | "textarea" => State::Rcdata,
            "style" | "xmp" | "iframe" | "noembed" | "noframes" => State::Rawtext,
            "script" => State::ScriptData,
            "plaintext" => State::Plaintext,
            _ => return,
        };
        self.last_start_tag = Some(name.to_ascii_lowercase());
    }

    /// Produces the next token, or `None` at EOF. The tree builder drives the tokenizer through this door so it can
    /// switch the tokenizer's state (the text-content modes) between tokens.
    pub fn next_token(&mut self) -> Option<Token> {
        loop {
            if !self.pending.is_empty() {
                return Some(self.pending.remove(0));
            }
            if self.eof {
                return None;
            }
            let mut scratch = Vec::new();
            self.step(&mut scratch);
            self.pending.extend(scratch);
        }
    }

    /// Whether the tokenizer stopped (EOF): the last step emitted the EOF token, so no further step may run.
    pub(crate) fn at_eof(&self) -> bool {
        self.eof
    }

    /// One state-machine step, appending any completed tokens to `out`.
    ///
    /// `pub(crate)` for the cooperative tree drive: the session steps the tokenizer directly (one admission check per
    /// step) so a long character run yields on the work meter instead of running to its end inside one `next_token`
    /// call.
    pub(crate) fn step(&mut self, out: &mut Vec<Token>) {
        let state = self.state;
        match state {
            State::Data => {
                // Bulk-scan the next clean run. A run may only be scanned whole when no whitespace split can occur
                // inside it: a fresh segment whose first byte is whitespace must end at the first non-whitespace
                // character (the 0.90 tokenizer's split), and an in-flight run that started with whitespace
                // (`ws_segment`) may still split — the stop-set scan cannot see the boundary, so both fall to the
                // per-char path. A run that started non-whitespace never splits, so its whole extent is clean.
                let scan_safe = if self.chars.is_empty() {
                    self.input
                        .as_bytes()
                        .get(self.at)
                        .is_some_and(|&b| !matches!(b, b'\t' | b'\n' | b'\x0C' | b'\r' | b' '))
                } else {
                    !self.ws_segment
                };
                if scan_safe && self.scan_text_run() {
                    return;
                }
                // The `&` and `<` branches FLUSH BEFORE consuming, so the character run's span ends exactly at the
                // boundary and the following token starts there (the contiguity law).
                if self.input[self.at..].starts_with('&') {
                    self.at += 1;
                    self.consume_character_reference();
                    return;
                }
                if self.input[self.at..].starts_with('<') {
                    self.flush_chars(out);
                    self.at += 1;
                    self.state = State::TagOpen;
                    self.tag_start = self.at - 1;
                    self.comment_start = self.at - 1;
                    return;
                }
                if self.input[self.at..].starts_with('\0') {
                    self.flush_chars(out);
                    self.at += 1;
                    self.emit_char('\0');
                    self.flush_chars(out);
                    return;
                }
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\r' => self.emit_normalized_cr(),
                    _ => {
                        // A run that started with whitespace ends at the first non-whitespace character (the whitespace
                        // is emitted as its own token, so the tree's per-token whitespace laws see it alone — the
                        // corpus pins it: `<html> a ` drops the leading space in before-head while the rest reaches the
                        // body).
                        if self.ws_segment && !matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' ') {
                            self.flush_chars(out);
                        }
                        if self.chars.is_empty() {
                            self.ws_segment = matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' ');
                        }
                        self.emit_char(ch);
                    }
                }
            }
            State::TagOpen => {
                let Some(ch) = self.bump() else {
                    // EOF: emit `<` as text and the EOF token.
                    self.emit_char('<');
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '!' => {
                        self.state = State::MarkupDeclarationOpen;
                    }
                    '/' => {
                        self.in_end_tag = true;
                        self.state = State::EndTagOpen;
                    }
                    c if c.is_ascii_alphabetic() => {
                        self.tag_name.clear();
                        self.tag_name.push(c.to_ascii_lowercase());
                        self.state = State::TagName;
                    }
                    '?' => {
                        self.temp.clear();
                        self.reconsume(ch);
                        self.state = State::BogusComment;
                    }
                    _ => {
                        // Anything else: parse error, emit `<` as text and reconsume.
                        self.emit_char('<');
                        self.reconsume(ch);
                        self.state = State::Data;
                    }
                }
            }
            State::EndTagOpen => {
                let Some(ch) = self.bump() else {
                    self.emit_char('<');
                    self.emit_char('/');
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    c if c.is_ascii_alphabetic() => {
                        self.tag_name.clear();
                        self.tag_name.push(c.to_ascii_lowercase());
                        self.state = State::TagName;
                    }
                    '>' => {
                        // Parse error (`</>`); nothing is emitted and the input is consumed.
                        self.in_end_tag = false;
                        self.state = State::Data;
                    }
                    _ => {
                        self.temp.clear();
                        self.reconsume(ch);
                        self.state = State::BogusComment;
                    }
                }
            }
            State::TagName => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {
                        self.state = State::BeforeAttributeName;
                    }
                    '/' => {
                        self.state = State::SelfClosingStartTag;
                    }
                    '>' => {
                        self.finish_tag(out);
                    }
                    c if c.is_ascii_uppercase() => {
                        self.tag_name.push(c.to_ascii_lowercase());
                    }
                    '\0' => {
                        self.tag_name.push('\u{FFFD}');
                    }
                    _ => self.tag_name.push(ch),
                }
            }
            State::BeforeAttributeName => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '/' | '>' => {
                        self.reconsume(ch);
                        self.state = State::AfterAttributeName;
                    }
                    '=' => {
                        // Parse error; treat as an attribute name.
                        self.push_attribute();
                        self.attr_name.clear();
                        self.attr_name.push('=');
                        self.state = State::AttributeName;
                    }
                    _ => {
                        // A new attribute starts: the pending one (reached through a solidus/self-closing detour) is
                        // pushed first — appends the new entry to the token's attribute list.
                        self.push_attribute();
                        self.attr_name.clear();
                        self.reconsume(ch);
                        self.state = State::AttributeName;
                    }
                }
            }
            State::AttributeName => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' | '/' | '>' => {
                        self.reconsume(ch);
                        self.state = State::AfterAttributeName;
                    }
                    '=' => {
                        self.state = State::BeforeAttributeValue;
                        self.attr_value.clear();
                    }
                    c if c.is_ascii_uppercase() => {
                        self.attr_name.push(c.to_ascii_lowercase());
                    }
                    '\0' => {
                        self.attr_name.push('\u{FFFD}');
                    }
                    '"' | '\'' | '<' => {
                        // Parse error; literal.
                        self.attr_name.push(ch);
                    }
                    _ => self.attr_name.push(ch),
                }
            }
            State::AfterAttributeName => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '/' => {
                        self.state = State::SelfClosingStartTag;
                    }
                    '=' => {
                        self.state = State::BeforeAttributeValue;
                        self.attr_value.clear();
                    }
                    '>' => {
                        self.finish_tag(out);
                    }
                    _ => {
                        self.push_attribute();
                        self.attr_name.clear();
                        self.reconsume(ch);
                        self.state = State::AttributeName;
                    }
                }
            }
            State::BeforeAttributeValue => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '"' => {
                        self.state = State::AttributeValueDoubleQuoted;
                    }
                    '\'' => {
                        self.state = State::AttributeValueSingleQuoted;
                    }
                    '>' => {
                        // Parse error; finish the tag with an empty value.
                        self.finish_tag(out);
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::AttributeValueUnquoted;
                    }
                }
            }
            State::AttributeValueDoubleQuoted => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '"' => {
                        self.state = State::AfterAttributeValueQuoted;
                    }
                    '&' => {
                        self.in_attribute = true;
                        self.consume_character_reference();
                        self.in_attribute = false;
                    }
                    '\0' => {
                        self.attr_value.push('\u{FFFD}');
                    }
                    _ => self.attr_value.push(ch),
                }
            }
            State::AttributeValueSingleQuoted => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\'' => {
                        self.state = State::AfterAttributeValueQuoted;
                    }
                    '&' => {
                        self.in_attribute = true;
                        self.consume_character_reference();
                        self.in_attribute = false;
                    }
                    '\0' => {
                        self.attr_value.push('\u{FFFD}');
                    }
                    _ => self.attr_value.push(ch),
                }
            }
            State::AttributeValueUnquoted => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {
                        self.push_attribute();
                        self.state = State::BeforeAttributeName;
                    }
                    '&' => {
                        self.in_attribute = true;
                        self.consume_character_reference();
                        self.in_attribute = false;
                    }
                    '>' => {
                        self.push_attribute();
                        self.finish_tag(out);
                    }
                    '\0' => {
                        self.attr_value.push('\u{FFFD}');
                    }
                    '"' | '\'' | '<' | '=' | '`' => {
                        // Parse error; literal.
                        self.attr_value.push(ch);
                    }
                    _ => self.attr_value.push(ch),
                }
            }
            State::AfterAttributeValueQuoted => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {
                        self.push_attribute();
                        self.state = State::BeforeAttributeName;
                    }
                    '/' => {
                        self.push_attribute();
                        self.state = State::SelfClosingStartTag;
                    }
                    '>' => {
                        self.push_attribute();
                        self.finish_tag(out);
                    }
                    _ => {
                        // Parse error; reconsume in before-attribute-name.
                        self.push_attribute();
                        self.reconsume(ch);
                        self.state = State::BeforeAttributeName;
                    }
                }
            }
            State::SelfClosingStartTag => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '>' => {
                        self.self_closing = true;
                        self.finish_tag(out);
                    }
                    _ => {
                        // Parse error; reconsume in before-attribute-name.
                        self.reconsume(ch);
                        self.state = State::BeforeAttributeName;
                    }
                }
            }
            State::BogusComment => {
                let start = self.comment_start;
                let mut data = String::new();
                loop {
                    match self.bump() {
                        None => break,
                        Some('>') => break,
                        Some('\0') => data.push('\u{FFFD}'),
                        Some(ch) => data.push(ch),
                    }
                }
                self.emit(out, TokenKind::Comment { data }, start, self.at);
                self.state = State::Data;
            }
            State::MarkupDeclarationOpen => {
                if self.input[self.at..].starts_with("--") {
                    self.at += 2;
                    self.state = State::CommentStart;
                    return;
                }
                if self.input[self.at..]
                    .get(.."DOCTYPE".len())
                    .is_some_and(|head| head.eq_ignore_ascii_case("DOCTYPE"))
                {
                    self.at += "DOCTYPE".len();
                    self.state = State::Doctype;
                    return;
                }
                if self.input[self.at..].starts_with("[CDATA[") && self.foreign_content {
                    self.at += "[CDATA[".len();
                    self.state = State::CdataSection;
                    return;
                }
                // Anything else: a bogus comment whose data starts AFTER the consumed `<!` (the span still covers the
                // whole region).
                self.state = State::BogusComment;
            }
            State::CommentStart => {
                let Some(ch) = self.bump() else {
                    let data = String::new();
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.state = State::CommentStartDash;
                    }
                    '>' => {
                        // Parse error; an empty comment.
                        let data = String::new();
                        self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                        self.state = State::Data;
                    }
                    _ => {
                        self.temp.clear();
                        self.reconsume(ch);
                        self.state = State::Comment;
                    }
                }
            }
            State::CommentStartDash => {
                let Some(ch) = self.bump() else {
                    let data = String::new();
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.state = State::CommentEnd;
                    }
                    '>' => {
                        // Parse error; empty comment.
                        let data = String::new();
                        self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                        self.state = State::Data;
                    }
                    _ => {
                        self.temp.push('-');
                        self.reconsume(ch);
                        self.state = State::Comment;
                    }
                }
            }
            State::Comment => {
                let Some(ch) = self.bump() else {
                    let data = core::mem::take(&mut self.temp);
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '<' => {
                        self.temp.push('<');
                        self.state = State::CommentLessThanSign;
                    }
                    '-' => {
                        self.state = State::CommentEndDash;
                    }
                    '\0' => {
                        self.temp.push('\u{FFFD}');
                    }
                    _ => self.temp.push(ch),
                }
            }
            State::CommentLessThanSign => {
                let Some(ch) = self.bump() else {
                    let data = core::mem::take(&mut self.temp);
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '!' => {
                        self.temp.push('!');
                        self.state = State::CommentLessThanSignBang;
                    }
                    '<' => {
                        self.temp.push('<');
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::Comment;
                    }
                }
            }
            State::CommentLessThanSignBang => {
                let Some(ch) = self.bump() else {
                    let data = core::mem::take(&mut self.temp);
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.state = State::CommentLessThanSignBangDash;
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::Comment;
                    }
                }
            }
            State::CommentLessThanSignBangDash => {
                let Some(ch) = self.bump() else {
                    let data = core::mem::take(&mut self.temp);
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.state = State::CommentLessThanSignBangDashDash;
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::CommentEndDash;
                    }
                }
            }
            State::CommentLessThanSignBangDashDash => {
                let Some(ch) = self.bump() else {
                    let data = core::mem::take(&mut self.temp);
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '>' | '\0' => {
                        // Parse error; the comment closes.
                        let data = core::mem::take(&mut self.temp);
                        self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                        self.state = State::Data;
                    }
                    '-' => {
                        self.temp.push('-');
                    }
                    _ => {
                        self.temp.push('-');
                        self.temp.push('-');
                        self.reconsume(ch);
                        self.state = State::Comment;
                    }
                }
            }
            State::CommentEndDash => {
                let Some(ch) = self.bump() else {
                    let data = core::mem::take(&mut self.temp);
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.state = State::CommentEnd;
                    }
                    _ => {
                        self.temp.push('-');
                        self.reconsume(ch);
                        self.state = State::Comment;
                    }
                }
            }
            State::CommentEnd => {
                let Some(ch) = self.bump() else {
                    let data = core::mem::take(&mut self.temp);
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '>' => {
                        let data = core::mem::take(&mut self.temp);
                        self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                        self.state = State::Data;
                    }
                    '!' => {
                        self.state = State::CommentEndBang;
                    }
                    '-' => {
                        self.temp.push('-');
                    }
                    _ => {
                        self.temp.push('-');
                        self.temp.push('-');
                        self.reconsume(ch);
                        self.state = State::Comment;
                    }
                }
            }
            State::CommentEndBang => {
                let Some(ch) = self.bump() else {
                    let data = core::mem::take(&mut self.temp);
                    self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.temp.push('-');
                        self.temp.push('-');
                        self.temp.push('!');
                        self.state = State::CommentEndDash;
                    }
                    '>' => {
                        // Parse error; the comment closes.
                        let data = core::mem::take(&mut self.temp);
                        self.emit(out, TokenKind::Comment { data }, self.comment_start, self.at);
                        self.state = State::Data;
                    }
                    _ => {
                        self.temp.push('-');
                        self.temp.push('-');
                        self.temp.push('!');
                        self.reconsume(ch);
                        self.state = State::Comment;
                    }
                }
            }
            State::Doctype => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {
                        self.state = State::BeforeDoctypeName;
                    }
                    '>' => {
                        self.reconsume(ch);
                        self.state = State::BeforeDoctypeName;
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::BeforeDoctypeName;
                    }
                }
            }
            State::BeforeDoctypeName => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '\0' => {
                        self.doctype_name = Some(String::from("\u{FFFD}"));
                        self.state = State::DoctypeName;
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    c if c.is_ascii_uppercase() => {
                        self.doctype_name = Some(String::from(c.to_ascii_lowercase()));
                        self.state = State::DoctypeName;
                    }
                    _ => {
                        self.doctype_name = Some(String::from(ch));
                        self.state = State::DoctypeName;
                    }
                }
            }
            State::DoctypeName => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {
                        self.state = State::AfterDoctypeName;
                    }
                    '>' => {
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    c if c.is_ascii_uppercase() => {
                        self.doctype_name
                            .get_or_insert_with(String::new)
                            .push(c.to_ascii_lowercase());
                    }
                    '\0' => {
                        self.doctype_name.get_or_insert_with(String::new).push('\u{FFFD}');
                    }
                    _ => self.doctype_name.get_or_insert_with(String::new).push(ch),
                }
            }
            State::AfterDoctypeName => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '>' => {
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ if self.input[self.at - ch.len_utf8()..]
                        .get(.."PUBLIC".len())
                        .is_some_and(|head| head.eq_ignore_ascii_case("PUBLIC")) =>
                    {
                        let after = self.at - ch.len_utf8() + "PUBLIC".len();
                        if self.input[after..].starts_with(['\t', '\n', '\x0C', '\r', ' ', '"', '\'', '>']) {
                            self.at = after;
                            self.state = State::AfterDoctypePublicKeyword;
                            return;
                        }
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                    _ if self.input[self.at - ch.len_utf8()..]
                        .get(.."SYSTEM".len())
                        .is_some_and(|head| head.eq_ignore_ascii_case("SYSTEM")) =>
                    {
                        let after = self.at - ch.len_utf8() + "SYSTEM".len();
                        if self.input[after..].starts_with(['\t', '\n', '\x0C', '\r', ' ', '"', '\'', '>']) {
                            self.at = after;
                            self.state = State::AfterDoctypeSystemKeyword;
                            return;
                        }
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                    _ => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                }
            }
            State::AfterDoctypePublicKeyword => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {
                        self.state = State::BeforeDoctypePublicIdentifier;
                    }
                    '"' => {
                        self.doctype_public = Some(String::new());
                        self.state = State::DoctypePublicIdentifierDoubleQuoted;
                    }
                    '\'' => {
                        self.doctype_public = Some(String::new());
                        self.state = State::DoctypePublicIdentifierSingleQuoted;
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                }
            }
            State::BeforeDoctypePublicIdentifier => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '"' => {
                        self.doctype_public = Some(String::new());
                        self.state = State::DoctypePublicIdentifierDoubleQuoted;
                    }
                    '\'' => {
                        self.doctype_public = Some(String::new());
                        self.state = State::DoctypePublicIdentifierSingleQuoted;
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                }
            }
            State::DoctypePublicIdentifierDoubleQuoted => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '"' => {
                        self.state = State::AfterDoctypePublicIdentifier;
                    }
                    '\0' => {
                        self.doctype_public.get_or_insert_with(String::new).push('\u{FFFD}');
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => self.doctype_public.get_or_insert_with(String::new).push(ch),
                }
            }
            State::DoctypePublicIdentifierSingleQuoted => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\'' => {
                        self.state = State::AfterDoctypePublicIdentifier;
                    }
                    '\0' => {
                        self.doctype_public.get_or_insert_with(String::new).push('\u{FFFD}');
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => self.doctype_public.get_or_insert_with(String::new).push(ch),
                }
            }
            State::AfterDoctypePublicIdentifier => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {
                        self.state = State::BetweenDoctypePublicAndSystemIdentifiers;
                    }
                    '>' => {
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    '"' => {
                        self.doctype_system = Some(String::new());
                        self.state = State::DoctypeSystemIdentifierDoubleQuoted;
                    }
                    '\'' => {
                        self.doctype_system = Some(String::new());
                        self.state = State::DoctypeSystemIdentifierSingleQuoted;
                    }
                    _ => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                }
            }
            State::BetweenDoctypePublicAndSystemIdentifiers => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '>' => {
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    '"' => {
                        self.doctype_system = Some(String::new());
                        self.state = State::DoctypeSystemIdentifierDoubleQuoted;
                    }
                    '\'' => {
                        self.doctype_system = Some(String::new());
                        self.state = State::DoctypeSystemIdentifierSingleQuoted;
                    }
                    _ => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                }
            }
            State::AfterDoctypeSystemKeyword => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {
                        self.state = State::BeforeDoctypeSystemIdentifier;
                    }
                    '"' => {
                        self.doctype_system = Some(String::new());
                        self.state = State::DoctypeSystemIdentifierDoubleQuoted;
                    }
                    '\'' => {
                        self.doctype_system = Some(String::new());
                        self.state = State::DoctypeSystemIdentifierSingleQuoted;
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                }
            }
            State::BeforeDoctypeSystemIdentifier => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '"' => {
                        self.doctype_system = Some(String::new());
                        self.state = State::DoctypeSystemIdentifierDoubleQuoted;
                    }
                    '\'' => {
                        self.doctype_system = Some(String::new());
                        self.state = State::DoctypeSystemIdentifierSingleQuoted;
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.state = State::BogusDoctype;
                    }
                }
            }
            State::DoctypeSystemIdentifierDoubleQuoted => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '"' => {
                        self.state = State::AfterDoctypeSystemIdentifier;
                    }
                    '\0' => {
                        self.doctype_system.get_or_insert_with(String::new).push('\u{FFFD}');
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => self.doctype_system.get_or_insert_with(String::new).push(ch),
                }
            }
            State::DoctypeSystemIdentifierSingleQuoted => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\'' => {
                        self.state = State::AfterDoctypeSystemIdentifier;
                    }
                    '\0' => {
                        self.doctype_system.get_or_insert_with(String::new).push('\u{FFFD}');
                    }
                    '>' => {
                        self.force_quirks = true;
                        self.doctype_correct = false;
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => self.doctype_system.get_or_insert_with(String::new).push(ch),
                }
            }
            State::AfterDoctypeSystemIdentifier => {
                let Some(ch) = self.bump() else {
                    self.eof = true;
                    self.doctype_correct = false;
                    self.force_quirks = true;
                    self.emit_doctype(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' => {}
                    '>' => {
                        self.emit_doctype(out);
                        self.state = State::Data;
                    }
                    _ => {
                        // The stray-character case: the bogus doctype does NOT flip correctness here.
                        self.force_quirks = true;
                        self.state = State::BogusDoctype;
                    }
                }
            }
            State::BogusDoctype => {
                loop {
                    match self.bump() {
                        None => break,
                        Some('>') => break,
                        Some(_) => {}
                    }
                }
                self.force_quirks = true;
                self.emit_doctype(out);
                self.state = State::Data;
            }
            State::CdataSection => {
                let Some(ch) = self.bump() else {
                    // EOF: the pending CDATA text is flushed first.
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    ']' => {
                        self.state = State::CdataSectionBracket;
                    }
                    '\0' => {
                        self.emit_char('\u{FFFD}');
                    }
                    '\r' => self.emit_normalized_cr(),
                    _ => self.emit_char(ch),
                }
            }
            State::CdataSectionBracket => {
                let Some(ch) = self.bump() else {
                    // The corpus pins the pending bracket: `<svg><![CDATA[]` keeps its `]` as text at EOF.
                    self.emit_char(']');
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    ']' => {
                        self.state = State::CdataSectionEnd;
                    }
                    _ => {
                        self.emit_char(']');
                        self.reconsume(ch);
                        self.state = State::CdataSection;
                    }
                }
            }
            State::CdataSectionEnd => {
                let Some(ch) = self.bump() else {
                    self.emit_char(']');
                    self.emit_char(']');
                    self.eof = true;
                    self.flush_chars(out);
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    ']' => {
                        self.emit_char(']');
                    }
                    '>' => {
                        self.flush_chars(out);
                        self.state = State::Data;
                    }
                    _ => {
                        self.emit_char(']');
                        self.emit_char(']');
                        self.reconsume(ch);
                        self.state = State::CdataSection;
                    }
                }
            }
            // The text-content states (RCDATA/RAWTEXT/script/plaintext) and their end-tag recognition share one
            // implementation.
            State::Rcdata => self.text_state(out, TextMode::Rcdata),
            State::Rawtext => self.text_state(out, TextMode::Rawtext),
            State::ScriptData => self.text_state(out, TextMode::ScriptData),
            State::Plaintext => {
                if self.scan_text_run() {
                    return;
                }
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\0' => self.emit_char('\u{FFFD}'),
                    '\r' => self.emit_normalized_cr(),
                    _ => self.emit_char(ch),
                }
            }
            State::RcdataLessThanSign => self.text_less_than(out, TextMode::Rcdata),
            State::RawtextLessThanSign => self.text_less_than(out, TextMode::Rawtext),
            State::ScriptDataLessThanSign => {
                // The script-data less-than-sign state has its own `!` branch (the escape start), unlike
                // RCDATA/RAWTEXT.
                let Some(ch) = self.bump() else {
                    self.emit_char('<');
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '/' => {
                        self.temp.clear();
                        self.state = State::ScriptDataEndTagOpen;
                    }
                    '!' => {
                        self.emit_char('<');
                        self.emit_char('!');
                        self.state = State::ScriptDataEscapeStart;
                    }
                    _ => {
                        self.emit_char('<');
                        self.reconsume(ch);
                        self.state = State::ScriptData;
                    }
                }
            }
            State::RcdataEndTagOpen => self.text_end_tag_open(out, TextMode::Rcdata),
            State::RawtextEndTagOpen => self.text_end_tag_open(out, TextMode::Rawtext),
            State::ScriptDataEndTagOpen => self.text_end_tag_open(out, TextMode::ScriptData),
            State::RcdataEndTagName => self.text_end_tag_name(out, TextMode::Rcdata),
            State::RawtextEndTagName => self.text_end_tag_name(out, TextMode::Rawtext),
            State::ScriptDataEndTagName => self.text_end_tag_name(out, TextMode::ScriptData),
            State::ScriptDataEscapeStart => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.emit_char('-');
                        self.state = State::ScriptDataEscapeStartDash;
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::ScriptData;
                    }
                }
            }
            State::ScriptDataEscapeStartDash => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.emit_char('-');
                        self.state = State::ScriptDataEscapedDashDash;
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::ScriptData;
                    }
                }
            }
            State::ScriptDataEscaped => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.emit_char('-');
                        self.state = State::ScriptDataEscapedDash;
                    }
                    '<' => {
                        self.flush_chars(out);
                        self.state = State::ScriptDataEscapedLessThanSign;
                    }
                    '\0' => {
                        self.emit_char('\u{FFFD}');
                    }
                    _ => self.emit_char(ch),
                }
            }
            State::ScriptDataEscapedDash => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.emit_char('-');
                        self.state = State::ScriptDataEscapedDashDash;
                    }
                    '<' => {
                        self.flush_chars(out);
                        self.state = State::ScriptDataEscapedLessThanSign;
                    }
                    '\0' => {
                        self.emit_char('\u{FFFD}');
                        self.state = State::ScriptDataEscaped;
                    }
                    _ => {
                        self.emit_char(ch);
                        self.state = State::ScriptDataEscaped;
                    }
                }
            }
            State::ScriptDataEscapedDashDash => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.emit_char('-');
                    }
                    '<' => {
                        self.flush_chars(out);
                        self.state = State::ScriptDataEscapedLessThanSign;
                    }
                    '>' => {
                        self.emit_char('>');
                        self.state = State::ScriptData;
                    }
                    '\0' => {
                        self.emit_char('\u{FFFD}');
                        self.state = State::ScriptDataEscaped;
                    }
                    _ => {
                        self.emit_char(ch);
                        self.state = State::ScriptDataEscaped;
                    }
                }
            }
            State::ScriptDataEscapedLessThanSign => {
                let Some(ch) = self.bump() else {
                    // The `<` was consumed entering this state; at EOF it is emitted as text (the law).
                    self.emit_char('<');
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '/' => {
                        self.temp.clear();
                        self.state = State::ScriptDataEscapedEndTagOpen;
                    }
                    c if c.is_ascii_alphabetic() => {
                        self.temp.clear();
                        self.emit_char('<');
                        self.reconsume(ch);
                        self.state = State::ScriptDataDoubleEscapeStart;
                    }
                    _ => {
                        self.emit_char('<');
                        self.reconsume(ch);
                        self.state = State::ScriptDataEscaped;
                    }
                }
            }
            State::ScriptDataEscapedEndTagOpen => {
                let Some(ch) = self.bump() else {
                    self.emit_char('<');
                    self.emit_char('/');
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    c if c.is_ascii_alphabetic() => {
                        self.tag_name.clear();
                        self.tag_name.push(c.to_ascii_lowercase());
                        self.temp.push(c);
                        self.state = State::ScriptDataEscapedEndTagName;
                    }
                    _ => {
                        self.emit_char('<');
                        self.emit_char('/');
                        self.reconsume(ch);
                        self.state = State::ScriptDataEscaped;
                    }
                }
            }
            State::ScriptDataEscapedEndTagName => {
                let Some(ch) = self.bump() else {
                    self.emit_char('<');
                    self.emit_char('/');
                    self.emit_chars_from_tag_name();
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' | '/' | '>' => {
                        if self.is_appropriate_end_tag() {
                            self.flush_chars(out);
                            self.emit_end_tag(out);
                            self.reconsume(ch);
                            self.state = State::BeforeAttributeName;
                        } else {
                            self.emit_char('<');
                            self.emit_char('/');
                            self.emit_chars_from_tag_name();
                            self.reconsume(ch);
                            self.state = State::ScriptDataEscaped;
                        }
                    }
                    c if c.is_ascii_uppercase() => {
                        self.tag_name.push(c.to_ascii_lowercase());
                        self.temp.push(c);
                    }
                    c if c.is_ascii_lowercase() => {
                        self.tag_name.push(c);
                        self.temp.push(c);
                    }
                    _ => {
                        self.emit_char('<');
                        self.emit_char('/');
                        self.emit_chars_from_tag_name();
                        self.reconsume(ch);
                        self.state = State::ScriptDataEscaped;
                    }
                }
            }
            State::ScriptDataDoubleEscapeStart => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' | '/' | '>' => {
                        if self.temp == "script" {
                            self.state = State::ScriptDataDoubleEscaped;
                        } else {
                            self.state = State::ScriptDataEscaped;
                        }
                        self.emit_char(ch);
                    }
                    c if c.is_ascii_uppercase() => {
                        self.temp.push(c.to_ascii_lowercase());
                        self.emit_char(c);
                    }
                    c if c.is_ascii_lowercase() => {
                        self.temp.push(c);
                        self.emit_char(c);
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::ScriptDataEscaped;
                    }
                }
            }
            State::ScriptDataDoubleEscaped => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.emit_char('-');
                        self.state = State::ScriptDataDoubleEscapedDash;
                    }
                    '<' => {
                        self.emit_char('<');
                        self.state = State::ScriptDataDoubleEscapedLessThanSign;
                    }
                    '\0' => {
                        self.emit_char('\u{FFFD}');
                    }
                    _ => self.emit_char(ch),
                }
            }
            State::ScriptDataDoubleEscapedDash => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.emit_char('-');
                        self.state = State::ScriptDataDoubleEscapedDashDash;
                    }
                    '<' => {
                        self.emit_char('<');
                        self.state = State::ScriptDataDoubleEscapedLessThanSign;
                    }
                    '\0' => {
                        self.emit_char('\u{FFFD}');
                        self.state = State::ScriptDataDoubleEscaped;
                    }
                    _ => {
                        self.emit_char(ch);
                        self.state = State::ScriptDataDoubleEscaped;
                    }
                }
            }
            State::ScriptDataDoubleEscapedDashDash => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '-' => {
                        self.emit_char('-');
                    }
                    '<' => {
                        self.emit_char('<');
                        self.state = State::ScriptDataDoubleEscapedLessThanSign;
                    }
                    '>' => {
                        self.emit_char('>');
                        self.state = State::ScriptData;
                    }
                    '\0' => {
                        self.emit_char('\u{FFFD}');
                        self.state = State::ScriptDataDoubleEscaped;
                    }
                    _ => {
                        self.emit_char(ch);
                        self.state = State::ScriptDataDoubleEscaped;
                    }
                }
            }
            State::ScriptDataDoubleEscapedLessThanSign => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '/' => {
                        self.temp.clear();
                        self.emit_char('/');
                        self.state = State::ScriptDataDoubleEscapeEnd;
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::ScriptDataDoubleEscaped;
                    }
                }
            }
            State::ScriptDataDoubleEscapeEnd => {
                let Some(ch) = self.bump() else {
                    self.flush_chars(out);
                    self.eof = true;
                    self.emit(out, TokenKind::Eof, self.at, self.at);
                    return;
                };
                match ch {
                    '\t' | '\n' | '\x0C' | '\r' | ' ' | '/' | '>' => {
                        if self.temp == "script" {
                            self.state = State::ScriptDataEscaped;
                        } else {
                            self.state = State::ScriptDataDoubleEscaped;
                        }
                        self.emit_char(ch);
                    }
                    c if c.is_ascii_uppercase() => {
                        self.temp.push(c.to_ascii_lowercase());
                        self.emit_char(c);
                    }
                    c if c.is_ascii_lowercase() => {
                        self.temp.push(c);
                        self.emit_char(c);
                    }
                    _ => {
                        self.reconsume(ch);
                        self.state = State::ScriptDataDoubleEscaped;
                    }
                }
            }
        }
    }

    /// Emits the pending characters of an end-tag lookalike as literal text (used when the lookalike turns out not to
    /// be an appropriate end tag). The ORIGINAL-case spelling is the temporary buffer's content; the lowercased tag
    /// name would rewrite the source.
    fn emit_chars_from_tag_name(&mut self) {
        let spelling = core::mem::take(&mut self.temp);
        if spelling.is_empty() {
            let name = core::mem::take(&mut self.tag_name);
            for ch in name.chars() {
                self.emit_char(ch);
            }
        } else {
            for ch in spelling.chars() {
                self.emit_char(ch);
            }
        }
    }

    /// Emits the collected end tag (used by the text end-tag-name states) and arms the silent tail: the attributes and
    /// `>` that follow are consumed without emitting another token.
    fn emit_end_tag(&mut self, out: &mut Vec<Token>) {
        let start = self.tag_start;
        let name = core::mem::take(&mut self.tag_name);
        self.emit(out, TokenKind::EndTag { name }, start, self.at);
        self.tag_name.clear();
        self.end_tag_pending = true;
    }

    /// Pushes the current attribute into the tag's attribute list, dropping duplicates (first wins, the WHATWG recovery
    /// law).
    fn push_attribute(&mut self) {
        if self.attr_name.is_empty() {
            return;
        }
        let name = core::mem::take(&mut self.attr_name);
        if !self.attrs.iter().any(|existing| existing.name == name) {
            let value = core::mem::take(&mut self.attr_value);
            self.attrs.push(Attribute { name, value });
        }
        self.attr_value.clear();
    }

    /// Completes the current start/end tag token.
    fn finish_tag(&mut self, out: &mut Vec<Token>) {
        self.push_attribute();
        let start = self.tag_start;
        let name = core::mem::take(&mut self.tag_name);
        let end = self.at;
        let attributes = core::mem::take(&mut self.attrs);
        let self_closing = self.self_closing;
        self.self_closing = false;
        let in_end_tag = self.in_end_tag;
        self.in_end_tag = false;
        if self.end_tag_pending {
            // The text-mode end-tag path already emitted the end tag; this finish is the tag TAIL being consumed
            // silently.
            self.end_tag_pending = false;
            self.state = State::Data;
            return;
        }
        if in_end_tag {
            self.emit(out, TokenKind::EndTag { name }, start, end);
        } else {
            self.last_start_tag = Some(name.clone());
            self.emit(
                out,
                TokenKind::StartTag {
                    name,
                    attributes,
                    self_closing,
                },
                start,
                end,
            );
        }
        self.state = State::Data;
    }

    /// Emits the doctype token.
    fn emit_doctype(&mut self, out: &mut Vec<Token>) {
        // An EOF emission is never a correct doctype (the law: every eof-in-doctype error flips correctness).
        let kind = TokenKind::Doctype {
            name: self.doctype_name.take(),
            public_identifier: self.doctype_public.take(),
            system_identifier: self.doctype_system.take(),
            force_quirks: self.force_quirks,
            correct: self.doctype_correct && !self.eof,
        };
        self.force_quirks = false;
        self.doctype_correct = true;
        self.emit(out, kind, self.tag_start, self.at);
    }

    /// One step of the three text-content states.
    fn text_state(&mut self, out: &mut Vec<Token>, mode: TextMode) {
        // Bulk-scan the next clean run: the text-content states have no whitespace split, so every non-stop character
        // joins the run. For RAWTEXT/script data the `&` stop is conservative (it is ordinary text there, processed one
        // char at a time by the per-char path) — correct, merely not maximally fast on `&`-laden rawtext.
        if self.scan_text_run() {
            return;
        }
        if self.input[self.at..].starts_with('&') && mode == TextMode::Rcdata {
            self.at += 1;
            self.consume_character_reference();
            return;
        }
        if self.input[self.at..].starts_with('<') && mode != TextMode::Plaintext {
            self.flush_chars(out);
            self.at += 1;
            self.tag_start = self.at - 1;
            match mode {
                TextMode::Rcdata => self.state = State::RcdataLessThanSign,
                TextMode::Rawtext => self.state = State::RawtextLessThanSign,
                TextMode::ScriptData => self.state = State::ScriptDataLessThanSign,
                TextMode::Plaintext => unreachable!(),
            }
            return;
        }
        let Some(ch) = self.bump() else {
            self.flush_chars(out);
            self.eof = true;
            self.emit(out, TokenKind::Eof, self.at, self.at);
            return;
        };
        match ch {
            '\0' => {
                // the law: the text-content states replace the null with U+FFFD (only the DATA state keeps it raw).
                self.emit_char('\u{FFFD}');
            }
            '\r' => self.emit_normalized_cr(),
            _ => self.emit_char(ch),
        }
    }

    fn text_less_than(&mut self, out: &mut Vec<Token>, mode: TextMode) {
        let Some(ch) = self.bump() else {
            self.emit_char('<');
            self.flush_chars(out);
            self.eof = true;
            self.emit(out, TokenKind::Eof, self.at, self.at);
            return;
        };
        match ch {
            '/' => {
                self.temp.clear();
                match mode {
                    TextMode::Rcdata => self.state = State::RcdataEndTagOpen,
                    TextMode::Rawtext => self.state = State::RawtextEndTagOpen,
                    TextMode::ScriptData => self.state = State::ScriptDataEndTagOpen,
                    TextMode::Plaintext => unreachable!(),
                }
            }
            _ => {
                self.emit_char('<');
                self.reconsume(ch);
                match mode {
                    TextMode::Rcdata => self.state = State::Rcdata,
                    TextMode::Rawtext => self.state = State::Rawtext,
                    TextMode::ScriptData => self.state = State::ScriptData,
                    TextMode::Plaintext => unreachable!(),
                }
            }
        }
    }

    fn text_end_tag_open(&mut self, out: &mut Vec<Token>, mode: TextMode) {
        let Some(ch) = self.bump() else {
            self.emit_char('<');
            self.emit_char('/');
            self.flush_chars(out);
            self.eof = true;
            self.emit(out, TokenKind::Eof, self.at, self.at);
            return;
        };
        match ch {
            c if c.is_ascii_alphabetic() => {
                self.tag_name.clear();
                self.tag_name.push(c.to_ascii_lowercase());
                // The first letter also enters the temporary buffer: the ORIGINAL-case spelling is what a failed
                // lookalike emits.
                self.temp.push(c);
                match mode {
                    TextMode::Rcdata => self.state = State::RcdataEndTagName,
                    TextMode::Rawtext => self.state = State::RawtextEndTagName,
                    TextMode::ScriptData => self.state = State::ScriptDataEndTagName,
                    TextMode::Plaintext => unreachable!(),
                }
            }
            _ => {
                self.emit_char('<');
                self.emit_char('/');
                self.reconsume(ch);
                match mode {
                    TextMode::Rcdata => self.state = State::Rcdata,
                    TextMode::Rawtext => self.state = State::Rawtext,
                    TextMode::ScriptData => self.state = State::ScriptData,
                    TextMode::Plaintext => unreachable!(),
                }
            }
        }
    }

    fn text_end_tag_name(&mut self, out: &mut Vec<Token>, mode: TextMode) {
        let Some(ch) = self.bump() else {
            self.emit_char('<');
            self.emit_char('/');
            self.emit_chars_from_tag_name();
            self.flush_chars(out);
            self.eof = true;
            self.emit(out, TokenKind::Eof, self.at, self.at);
            return;
        };
        let back_to = match mode {
            TextMode::Rcdata => State::Rcdata,
            TextMode::Rawtext => State::Rawtext,
            TextMode::ScriptData => State::ScriptData,
            TextMode::Plaintext => unreachable!(),
        };
        match ch {
            '\t' | '\n' | '\x0C' | '\r' | ' ' | '/' | '>' => {
                if self.is_appropriate_end_tag() {
                    // The end tag is DEFERRED to the tail machinery: the `>` completes it (drops an incomplete tag at
                    // EOF, so emission happens only at the tail's finish).
                    self.flush_chars(out);
                    self.in_end_tag = true;
                    self.reconsume(ch);
                    self.state = State::BeforeAttributeName;
                } else {
                    self.emit_char('<');
                    self.emit_char('/');
                    self.emit_chars_from_tag_name();
                    self.reconsume(ch);
                    self.state = back_to;
                }
            }
            c if c.is_ascii_uppercase() => {
                self.tag_name.push(c.to_ascii_lowercase());
                self.temp.push(c);
            }
            c if c.is_ascii_lowercase() => {
                self.tag_name.push(c);
                self.temp.push(c);
            }
            _ => {
                self.emit_char('<');
                self.emit_char('/');
                self.emit_chars_from_tag_name();
                self.reconsume(ch);
                self.state = back_to;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextMode {
    Rcdata,
    Rawtext,
    ScriptData,
    Plaintext,
}

/// The windows-1252 mapping for the C1 range 0x80..=0x9F (the WHATWG table, used by both the windows-1252 decoder and
/// the numeric character reference resolution).
pub(crate) const WINDOWS_1252_MAP: [u32; 32] = [
    0x20AC, 0x0081, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, // 80-87
    0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008D, 0x017D, 0x008F, // 88-8F
    0x0090, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, // 90-97
    0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x009D, 0x017E, 0x0178, // 98-9F
];

/// Decodes one string in the WHATWG windows-1252 decoder law: bytes 0x00-0x7F are ASCII, 0x80-0x9F map through
/// [`WINDOWS_1252_MAP`] (0x81/0x8D/0x8F/ 0x90/0x9D are the unmapped controls, which decode to their own code points),
/// and 0xA0-0xFF are the ISO-8859-1 tail.
pub fn decode_windows_1252(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes {
        let ch = match byte {
            0x80..=0x9F => char::from_u32(WINDOWS_1252_MAP[(byte - 0x80) as usize]).unwrap_or('\u{FFFD}'),
            _ => char::from(byte),
        };
        out.push(ch);
    }
    out
}

/// The WHATWG UTF-8 decoder law: one U+FFFD per maximal invalid subpart.
pub fn decode_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(input: &str) -> Vec<Token> {
        let mut tokenizer = Tokenizer::new(input.to_owned());
        let mut out = Vec::new();
        while let Some(token) = tokenizer.next_token() {
            // Drive the text-mode switch exactly as the tree builder will: a text-content start tag re-enters the
            // tokenizer in its mode.
            if let TokenKind::StartTag { name, .. } = &token.kind {
                if matches!(
                    name.as_str(),
                    "title" | "textarea" | "style" | "xmp" | "iframe" | "noembed" | "noframes" | "script" | "plaintext"
                ) {
                    tokenizer.enter_text_mode(name);
                }
            }
            out.push(token);
        }
        out
    }

    fn assert_reassembly(input: &str) -> Vec<Token> {
        let tokens = tokenize(input);
        let mut rebuilt = String::new();
        for token in &tokens {
            if let TokenKind::Eof = token.kind {
                continue;
            }
            rebuilt.push_str(&input[token.span.clone()]);
        }
        assert_eq!(rebuilt, input, "reassembly diverged for {input:?}");
        tokens
    }

    #[test]
    fn the_token_stream_reassembles_the_source_exactly() {
        let cases = [
            "",
            "hello",
            "<p>hello</p>",
            "<p a=\"1\" b='2' c>text &amp; more</p>",
            "<!-- comment --><div>x</div>",
            "<!DOCTYPE html><html><head><title>t</title></head><body>y</body></html>",
            "<script>if (a < b) { x = \"</script>\" }</script>",
            "<textarea><b>raw</b></textarea>",
            "<p>a < b && c > d</p>",
            "<svg><![CDATA[raw ]]> text</svg>",
            "text with < unclosed",
            "<p title=\"a&quot;b\">x</p>",
            "<input disabled>",
            "<a href='x&amp;y'>z</a>",
            "&amp; &lt; &gt; &quot; &#65; &#x41; &notanentity;",
        ];
        for case in cases {
            assert_reassembly(case);
        }
    }

    #[test]
    fn start_tags_lowercase_and_deduplicate_attributes() {
        let tokens = tokenize("<DIV ID=\"1\" id=\"2\" x=3>");
        let TokenKind::StartTag { name, attributes, .. } = &tokens[0].kind else {
            panic!("expected start tag");
        };
        assert_eq!(name, "div");
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes[0].name, "id");
        assert_eq!(attributes[0].value, "1");
        assert_eq!(attributes[1].name, "x");
        assert_eq!(attributes[1].value, "3");
    }

    #[test]
    fn character_references_resolve() {
        let tokens = tokenize("&amp;&lt;&gt;&quot;&apos;&#65;&#x42;&nbsp;&AElig;x");
        let TokenKind::Character { data } = &tokens[0].kind else {
            panic!("expected character token");
        };
        assert_eq!(data, "&<>\"'AB\u{00A0}\u{00C6}x");
    }

    #[test]
    fn the_legacy_ampersand_rule_holds() {
        // `&not` is a legacy name: in text it expands; in an attribute followed by `=` or alphanumeric it stays
        // literal.
        let tokens = tokenize("<p>&notin; &not</p>");
        let text: String = tokens
            .iter()
            .filter_map(|token| match &token.kind {
                TokenKind::Character { data } => Some(data.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "\u{2209} \u{00AC}");
        let tokens = tokenize("<a href='&notit'>x</a>");
        let TokenKind::StartTag { attributes, .. } = &tokens[0].kind else {
            panic!("expected start tag");
        };
        assert_eq!(attributes[0].value, "&notit");
    }

    #[test]
    fn numeric_references_apply_the_c1_map_and_replacement() {
        let tokens = tokenize("&#128; &#0; &#xD800; &#x110000;");
        let TokenKind::Character { data } = &tokens[0].kind else {
            panic!("expected character");
        };
        assert_eq!(data, "\u{20AC} \u{FFFD} \u{FFFD} \u{FFFD}");
    }

    #[test]
    fn script_data_handles_escapes() {
        let tokens = tokenize("<script><!-- <script> x </script> --></script>");
        let TokenKind::StartTag { .. } = &tokens[0].kind else {
            panic!("expected start tag");
        };
        let kinds: Vec<&str> = tokens
            .iter()
            .map(|token| match &token.kind {
                TokenKind::StartTag { .. } => "start",
                TokenKind::EndTag { .. } => "end",
                TokenKind::Character { .. } => "char",
                _ => "other",
            })
            .collect();
        // The script body's text arrives as several ADJACENT character tokens (the spec emits characters as consumed;
        // the tree builder merges them), and the inner `</script>` is text, never an end tag.
        assert_eq!(kinds, ["start", "char", "char", "end", "other"]);
        let text: String = tokens
            .iter()
            .filter_map(|token| match &token.kind {
                TokenKind::Character { data } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "<!-- <script> x </script> -->");
    }

    #[test]
    fn doctype_states_collect_the_identifiers() {
        let tokens = tokenize(
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0//EN\" \"http://www.w3.org/TR/xhtml1/DTD/xhtml1.dtd\">",
        );
        let TokenKind::Doctype {
            name,
            public_identifier,
            system_identifier,
            force_quirks,
            ..
        } = &tokens[0].kind
        else {
            panic!("expected doctype");
        };
        assert_eq!(name.as_deref(), Some("html"));
        assert_eq!(public_identifier.as_deref(), Some("-//W3C//DTD XHTML 1.0//EN"));
        assert_eq!(
            system_identifier.as_deref(),
            Some("http://www.w3.org/TR/xhtml1/DTD/xhtml1.dtd")
        );
        assert!(!force_quirks);
    }

    #[test]
    fn bogus_doctype_forces_quirks() {
        let tokens = tokenize("<!DOCTYPE html PUBLIC>");
        let TokenKind::Doctype { force_quirks, .. } = &tokens[0].kind else {
            panic!("expected doctype");
        };
        assert!(force_quirks);
    }
}

#[cfg(test)]
mod html_stop_set_oracle_tests {
    use super::{Html, StopSet, prefix_len};
    use alloc::vec;
    use alloc::vec::Vec;

    fn check_alignment<S: StopSet>(bytes: &[u8]) {
        for start in 0..=bytes.len().min(3) {
            for end in start..=bytes.len().min(start + 48) {
                let slice = &bytes[start..end];
                assert_eq!(
                    prefix_len::<S>(slice),
                    slice.iter().take_while(|b| !S::stop(**b)).count(),
                    "{} mismatch at {start}..{end} of {bytes:?}",
                    core::any::type_name::<S>(),
                );
            }
        }
    }

    /// The alignment oracle for the text-data stop set this module owns: the wide kernel must agree with the scalar
    /// predicate at every alignment and length, so a wrong kernel is a test failure here.
    #[test]
    fn stop_sets_agree_with_their_scalar_predicates_at_every_alignment() {
        let mut corpus: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"\"".to_vec(),
            b"<p>&amp;\r</p>".to_vec(),
            b"\x00\x1f\x7f\xff".to_vec(),
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 0..48 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                let r = mix(&mut state);
                bytes.push(match r % 4 {
                    0 => b"<&\x00\r"[((r >> 8) % 4) as usize],
                    _ => ((r >> 16) & 0xFF) as u8,
                });
            }
            corpus.push(bytes);
        }
        for bytes in &corpus {
            check_alignment::<Html>(bytes);
        }
    }
}
