//! The JSON READER: the lenient parse `fromjson` performs, and the messages it reports when the text is not JSON.
//!
//! One job: turn one piece of text into one value the way the reader does, including WHERE it says a fault is. It is
//! [`super::render`]'s mirror image — that module exists so the engine can spell the JSON writer without reaching for
//! a codec, and this one exists for the same reason on the reading side. The engine's dependency list has no JSON
//! codec, deliberately: a codec owns its format's I/O, the engine owns the value semantics.
//!
//! Even if the dependency were allowed the codec could not serve, because this is a different function. The acceptance
//! is LOOSER than JSON's (`+1`, `01`, `nan` and a leading BOM all parse) and its faults are one sentence naming the
//! whole input, where the codec's are span-carrying diagnostics. The number spellings are exactly [`super::scan`]'s,
//! which is the point of that module:
//! `tonumber` and this reader share one acceptor.
//!
//! # The machine is an explicit stack, not a recursive descent
//!
//! The reader scans byte by byte with an explicit stack, and every message class and position below falls out of that
//! shape. Two consequences are load-bearing:
//!
//! * a fault found while CONSUMING a byte reads `at line L, column C`, while one
//!   found after the last byte reads `at EOF at line L, column C` — the same reason appears both ways (`[tru]` is `at
//!   line 1, column 5`, `tru` is `at EOF at line 1, column 3`);
//! * `line` is 1 plus the number of `\n` bytes consumed and `column` is the
//!   number of BYTES consumed since the last one, so a newline resets the column to zero (`[1\n` is `at EOF at line 2,
//!   column 0`) and a multi-byte character counts once per byte (`é` alone is `column 2`).
//!
//! The stack is explicit here for a second reason: the ceiling is 10 000 frames, and a recursive reader would meet the
//! process stack long before it met the limit.
//!
//! # Where the faults sit
//!
//! ```text
//! $ '{"a"}'      → Objects must consist of key:value pairs at line 1, column 5
//! $ '{"a":}'     → Unmatched '}' at line 1, column 6
//! $ '[1}'        → Objects must consist of key:value pairs at line 1, column 3
//! $ '{"a":1]'    → Unmatched ']' at line 1, column 7
//! $ '{"a":"b":1}'→ ':' not as part of an object at line 1, column 9
//! $ '[1 2]'      → Expected separator between values at line 1, column 5
//! $ '"a<TAB>b"'  → Invalid string: control characters … at line 1, column 5
//! ```
//!
//! Three of those are why the branches below read in the order they do. A `}` inspects the PENDING VALUE before the
//! open frame, so a value with no key waiting is the pairs message whatever the frame is, while a `]` inspects the
//! frame first and calls everything else unmatched. A string's escapes are decoded when its closing quote arrives, not
//! as they are read, so every string-body fault is reported at that quote.
//!
//! # The depth ceiling counts SLOTS, and an object costs two
//!
//! An open array holds one slot; an open object holds one, plus a second once a `:` has consumed its key. The check
//! runs only when an opener is consumed, and compares the count BEFORE the push against 10 000:
//!
//! ```text
//! $ '[' × 10000           → parses      $ '[' × 10001         → column 10001
//! $ '{"a":' × 5000 + '1'  → parses      $ '{"a":' × 5001      → column 25001
//! $ '[' × 9999 + '{"a":1}' → parses     (9999 + 2 exceeds 10 000 and still
//!                                        parses: the check is at the opener)
//! ```
//!
//! # Divergences, both inherited
//!
//! The non-finite spellings (`nan`, `snan`, `inf`, `Infinity`) are accepted here, because the values are constructible
//! and ordered: the acceptance lives in [`super::scan`] and this reader routes through it, so `fromjson`/`tonumber` and
//! the `nan`/`infinite` literals answer the same numbers. And `"-0"` reads as `0` rather than `-0`, which is the
//! integer path's `-0` defect and not this module's.
//!
//! Negative space: it owns no kind check — a non-string input is the builtin's refusal, because only the builtin
//! knows its own name — and no message FRAME, which is [`message::json_parse_message`]'s. It reads text and never a
//! document handle, so no located view reaches it.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Array, ObjectBuilder, ObjectKey, Value, ValueAllocationError};
use jqf_resource::ResourceContext;

use super::json_escape::{self, EscapeFault};
use super::scan;
use crate::error::{EngineRunError, message};
use crate::semantics::path::raise;

/// The parsing ceiling, in stack SLOTS.
const DEPTH_SLOTS: usize = 10_000;

/// Reads `text` as one JSON value.
///
/// # Errors
///
/// Returns the parse message — reason, position clause and whole-input echo — or an allocation failure.
pub fn json(text: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let mut reader = Reader::new(text, resources);
    match reader.read() {
        Ok(value) => Ok(value),
        Err(Failure::Allocation) => Err(EngineRunError::allocation_failure()),
        Err(Failure::Refused(reason)) => Err(reader.refusal(reason, text, resources)),
    }
}

/// Reads `text` as a stream of ADJACENT JSON values.
///
/// Every value is parsed and returned in order; a single leading BOM is stripped exactly once at the stream start (the
/// stdin BOM law), and every later BOM is ordinary token bytes that fault like any other text. Line and column
/// positions in the refusal message are absolute across the whole text, because the values are one stream, not
/// independent documents.
///
/// # Errors
///
/// Returns the parse message WITHOUT the echoed input — the form `--slurpfile` reports (`Bad JSON in --slurpfile NAME
/// FILE: <message>`) — or an allocation failure.
pub fn json_sequence(text: &str, resources: &ResourceContext<'_>) -> Result<Vec<Value>, EngineRunError> {
    // One source-start BOM is stream metadata (RFC 8259 section 8.1): it is stripped before counting, exactly as the
    // stdin path strips it. A BOM anywhere else is bytes the next value's token reader must fault on.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut values = Vec::new();
    // The byte offset of the next value's first byte. Line/column are NOT carried across values: each value's reader
    // counts its own slice, and a refusal converts the local position to the stream's absolute one from this offset
    // (prefix newlines and prefix column). That keeps the settle byte — the delimiter or token byte a reader counted
    // while closing the previous value — counted exactly once.
    let mut offset = 0usize;
    loop {
        offset = skip_separators(text, offset);
        if offset == text.len() {
            break;
        }
        let mut reader = Reader::new_at(&text[offset..], resources);
        match reader.read_one() {
            Ok((value, end)) => {
                values.push(value);
                offset += end;
            }
            Err(Failure::Allocation) => return Err(EngineRunError::allocation_failure()),
            Err(Failure::Refused(reason)) => {
                // The reader counted its own slice; lift its local line/column into the whole stream's coordinates. The
                // local column counts bytes since the slice's last newline, so it is absolute only when the slice
                // crossed no newline (local line 1) — then the prefix's column-on-line adds on.
                let prefix = &text[..offset];
                let prefix_newlines = prefix.bytes().filter(|byte| *byte == b'\n').count();
                let prefix_column = match prefix.rfind('\n') {
                    Some(last) => offset - last - 1,
                    None => offset,
                };
                let place = reason.placed.then_some(message::JsonPlace {
                    line: u32::try_from(prefix_newlines)
                        .unwrap_or(u32::MAX)
                        .saturating_add(reader.line),
                    column: if reader.line == 1 {
                        u32::try_from(prefix_column)
                            .unwrap_or(u32::MAX)
                            .saturating_add(reader.column)
                    } else {
                        reader.column
                    },
                    at_eof: reader.at_eof,
                });
                return match message::json_parse_message_head(reason.text, place.as_ref()) {
                    Ok(message) => Err(raise(&message, resources)),
                    Err(error) => Err(error),
                };
            }
        }
    }
    Ok(values)
}

/// One step of the INCREMENTAL adjacent-value reader ([`json_stream_next`]), which parses a GROWING window a streaming
/// host refills between calls.
pub enum JsonStreamStep {
    /// One complete value, and the byte offset of its end within the window:
    /// everything before `consumed` (leading separators included) belongs to this value and may be drained; the byte AT
    /// `consumed` — a delimiter or the next value's first byte — has not been claimed.
    Value {
        /// The parsed value.
        value: Value,
        /// The byte offset within the window where the value ended.
        consumed: usize,
    },
    /// The window holds only value-separator whitespace: drain it whole and refill, or stop at end of stream.
    Exhausted,
    /// The window ends where the value could continue — an open string or container, or a bare token whose next byte
    /// could extend it (`12`, `tru`, `nul`) — so nothing is consumed and the caller must refill.
    /// Never returned when `at_eof` is true.
    NeedMore,
    /// The parse refusal, in the no-echo stream form `--slurpfile` reports, with the position lifted to the STREAM's
    /// absolute coordinates.
    Refused(String),
}

/// Reads the next adjacent JSON value from a growing stream window.
///
/// The single-puller streaming form of [`json_sequence`]: the caller drains each returned value's bytes from the window
/// and refills it from the live source on [`JsonStreamStep::NeedMore`]. The hold decision is EXACT, not a span
/// heuristic: a fault while consuming a byte can never be cured by later bytes (the bytes before it are already fixed),
/// so only the window RUNNING OUT holds — an open string or container, or a bare token, whose reading finishes only
/// at a delimiter or EOF. With `at_eof` the held tail finishes exactly as the whole-input read finishes it, faults
/// included, so a complete stream parses byte-identically to [`json_sequence`] over the same bytes.
///
/// `prefix_newlines`/`prefix_column` are the stream-absolute coordinates of the window's first byte (newlines drained
/// so far, and bytes since the last drained newline): a refusal's position clause reads as if the whole stream were one
/// text, [`json_sequence`]'s own law. The caller owns the one stream-start BOM strip, exactly as the sequence driver
/// owns it.
///
/// # Errors
///
/// Returns only the machine allocation class; parse refusals are the [`JsonStreamStep::Refused`] arm, because the
/// caller (the shared input cursor) raises them catch-eligibly at the pull site.
pub fn json_stream_next(
    window: &str,
    at_eof: bool,
    prefix_newlines: u64,
    prefix_column: u64,
    resources: &ResourceContext<'_>,
) -> Result<JsonStreamStep, EngineRunError> {
    json_stream_next_hinted(window, at_eof, prefix_newlines, prefix_column, None, resources)
}

/// [`json_stream_next`] over one pulled record WITH a kept-subtree hint:
/// members the record's consumer never reads are validated but not built.
/// Over-delivery is always sound, so `None` (or a hint that keeps the root) is the whole-decode floor byte for byte —
/// including its failures.
pub fn json_stream_next_hinted(
    window: &str,
    at_eof: bool,
    prefix_newlines: u64,
    prefix_column: u64,
    prune: Option<&PruneHint>,
    resources: &ResourceContext<'_>,
) -> Result<JsonStreamStep, EngineRunError> {
    let offset = skip_separators(window, 0);
    if offset == window.len() {
        return Ok(JsonStreamStep::Exhausted);
    }
    let mut reader = Reader::new_at(&window[offset..], resources);
    if let Some(hint) = prune {
        reader.prune = Some(hint.clone());
    }
    let mut refusal: Option<Reason> = None;
    let local_len = window.len() - offset;
    let mut local = 0;
    while local < local_len {
        match reader.step_run(local, local_len) {
            Ok((next, settle_pos)) => {
                local = next;
                if let Some(value) = reader.settled.take() {
                    return Ok(JsonStreamStep::Value {
                        value,
                        // The byte at which settle ran — the value's end, as the per-byte reader reported it. The
                        // byte AT `consumed` has not been claimed.
                        consumed: offset + settle_pos.unwrap_or(local),
                    });
                }
            }
            Err(Failure::Allocation) => return Err(EngineRunError::allocation_failure()),
            Err(Failure::Refused(reason)) => {
                refusal = Some(reason);
                break;
            }
        }
    }
    if refusal.is_none() {
        // The window ran out. A still-open string, container, or bare token could all continue in the next refill (a
        // token GREEDILY: `12` may become `123`, `tru` may become `true`), so only a value already closed by its own
        // final byte finishes here before end of stream.
        if !at_eof && (!matches!(reader.mode, Mode::Between) || !reader.frames.is_empty()) {
            return Ok(JsonStreamStep::NeedMore);
        }
        reader.at_eof = true;
        match reader.finish() {
            Ok(value) => {
                return Ok(JsonStreamStep::Value {
                    value,
                    consumed: window.len(),
                });
            }
            Err(Failure::Allocation) => return Err(EngineRunError::allocation_failure()),
            Err(Failure::Refused(reason)) => refusal = Some(reason),
        }
    }
    let Some(reason) = refusal else {
        return Err(EngineRunError::allocation_failure());
    };
    // Lift the reader's window-local position to the stream's absolute one:
    // [`json_sequence`]'s law, with the drained prefix's coordinates supplied by the caller instead of recounted from
    // retained bytes.
    let internal = &window[..offset];
    let internal_newlines = internal.bytes().filter(|byte| *byte == b'\n').count();
    let internal_column = match internal.rfind('\n') {
        Some(last) => offset - last - 1,
        None => offset,
    };
    let total_newlines = prefix_newlines.saturating_add(u64::try_from(internal_newlines).unwrap_or(u64::MAX));
    let value_start_column = if internal_newlines > 0 {
        u64::try_from(internal_column).unwrap_or(u64::MAX)
    } else {
        prefix_column.saturating_add(u64::try_from(internal_column).unwrap_or(u64::MAX))
    };
    let place = reason.placed.then_some(message::JsonPlace {
        line: u32::try_from(total_newlines)
            .unwrap_or(u32::MAX)
            .saturating_add(reader.line),
        column: if reader.line == 1 {
            u32::try_from(value_start_column)
                .unwrap_or(u32::MAX)
                .saturating_add(reader.column)
        } else {
            reader.column
        },
        at_eof: reader.at_eof,
    });
    match message::json_parse_message_head(reason.text, place.as_ref()) {
        Ok(message) => Ok(JsonStreamStep::Refused(message)),
        Err(error) => Err(error),
    }
}

/// Advances `offset` past RFC 8259 value-separator whitespace (space, tab, line feed, carriage return). Pure byte
/// skipping: it carries no counting state, because the stream's positions are derived from the offset on the refusal
/// path instead.
fn skip_separators(text: &str, mut offset: usize) -> usize {
    while matches!(text.as_bytes().get(offset), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        offset += 1;
    }
    offset
}

/// One refusal the reader can report.
#[derive(Clone, Copy)]
struct Reason {
    /// The sentence, verbatim.
    text: &'static str,
    /// Whether a position clause is appended to it. Only the two reasons that describe the WHOLE input do not.
    placed: bool,
}

/// A reason with a position.
const fn placed(text: &'static str) -> Reason {
    Reason { text, placed: true }
}

/// A reason reported without a position, because it is about the whole input.
const fn whole(text: &'static str) -> Reason {
    Reason { text, placed: false }
}

const EXPECTED_VALUE: Reason = whole("Expected JSON value");
const EXTRA_VALUES: Reason = whole("Unexpected extra JSON values");
const NUMERIC_LITERAL: Reason = placed("Invalid numeric literal");
const KEYWORD_LITERAL: Reason = placed("Invalid literal");
const QUOTE_LITERAL: Reason = placed("Invalid string literal; expected \", but got '");
const SEPARATOR: Reason = placed("Expected separator between values");
const DEPTH: Reason = placed("Exceeds depth limit for parsing");
const KEY_BEFORE_COLON: Reason = placed("Expected string key before ':'");
const COLON_OUTSIDE_OBJECT: Reason = placed("':' not as part of an object");
const STRING_KEYS: Reason = placed("Object keys must be strings");
const VALUE_BEFORE_COMMA: Reason = placed("Expected value before ','");
const COMMA_OUTSIDE_CONTAINER: Reason = placed("',' not as part of an object or array");
const PAIRS: Reason = placed("Objects must consist of key:value pairs");
const ANOTHER_ELEMENT: Reason = placed("Expected another array element");
const ANOTHER_PAIR: Reason = placed("Expected another key-value pair");
const UNMATCHED_BRACKET: Reason = placed("Unmatched ']'");
const UNMATCHED_BRACE: Reason = placed("Unmatched '}'");
const UNFINISHED_TERM: Reason = placed("Unfinished JSON term");
const UNFINISHED_STRING: Reason = placed("Unfinished string");
const INVALID_ESCAPE: Reason = escape_reason(EscapeFault::Invalid);
const ESCAPE_AT_END: Reason = escape_reason(EscapeFault::AtEnd);
const SHORT_UNICODE_ESCAPE: Reason = escape_reason(EscapeFault::ShortUnicode);
const UNICODE_ESCAPE_DIGITS: Reason = escape_reason(EscapeFault::BadDigits);
const SURROGATE_PAIR: Reason = escape_reason(EscapeFault::SurrogatePair);

/// The verbatim sentence for an escape fault, shared with the streaming event parser through [`json_escape`].
const fn escape_reason(fault: EscapeFault) -> Reason {
    Reason {
        text: fault.text(),
        placed: true,
    }
}
const CONTROL_CHARACTERS: Reason =
    placed("Invalid string: control characters from U+0000 through U+001F must be escaped");

/// Why a read stopped.
///
/// An allocation failure is not one of the reasons and never becomes a parse message: it is the engine's own budget
/// answer.
enum Failure {
    /// The text was refused.
    Refused(Reason),
    /// The value under construction could not be allocated.
    Allocation,
}

impl From<Reason> for Failure {
    fn from(reason: Reason) -> Self {
        Self::Refused(reason)
    }
}

impl From<ValueAllocationError> for Failure {
    fn from(_: ValueAllocationError) -> Self {
        Self::Allocation
    }
}

/// What the scanner is in the middle of.
#[derive(Clone, Copy)]
enum Mode {
    /// Between tokens.
    Between,
    /// Accumulating a bare token — a keyword, a number, or neither.
    Token {
        /// Where the token's first byte sits.
        start: usize,
    },
    /// Inside a string, whose body starts at `start`.
    Text {
        /// Where the body's first byte sits, past the opening quote.
        start: usize,
    },
    /// Inside a string, one byte past a backslash.
    Escape {
        /// The body start the string keeps while the escape is consumed.
        start: usize,
    },
}

/// The flattened kept-subtree hint over ONE pulled record: the engine's transported tree (`jqf_codec_core::PruneTree`)
/// copied into plain lookup nodes so a pull owns it without borrowing the requirement — the same shape the JSON
/// codec's `PruneMap` flattens into, for the same reason.
///
/// The streaming `-n inputs` cursor hands this to every `json_stream_next` call so each pulled record parses
/// field-pruned: members the fold body never reads are validated but not built, which is what turns a full-record
/// decode into a field-only one. Over-delivery is always sound (the hint is monotone); any doubt routes to `None`
/// (whole decode).
#[derive(Clone)]
pub struct PruneHint {
    nodes: Vec<HintNode>,
}

#[derive(Clone)]
struct HintNode {
    all: bool,
    element: Option<u32>,
    /// Ascending by key bytes (the transport's push-order contract).
    keys: Vec<(Box<[u8]>, u32)>,
}

impl PruneHint {
    /// Sentinel ids over the node range, mirroring the codec's own sentinels: a node id at or above [`Self::OMIT`] is
    /// never minted by the engine's dense allocator, so both are free as decision markers.
    pub(crate) const OMIT: u32 = u32::MAX - 1;
    pub(crate) const ALL: u32 = u32::MAX;
    /// The document root's id, seeded so the TOP-LEVEL object's members resolve against node 0 without a special case.
    pub(crate) const ROOT_SEED: u32 = 0;

    /// Flattens the transported tree; `None` when the hint keeps everything at the root (nothing to prune). The scan
    /// walks ids from 0 and stops at the FIRST gap — sound because the engine assigns ids densely from 0 and reserves
    /// the top of the u32 range for its sentinels, exactly the inference `PruneMap::from_transport` makes in the codec.
    #[must_use]
    pub fn from_tree(tree: &jqf_codec_core::PruneTree) -> Option<Self> {
        if tree.root().is_all() {
            return None;
        }
        let mut nodes = Vec::new();
        for id in 0..=u32::MAX - 2 {
            let Some(node) = tree.node(id) else { break };
            nodes.push(HintNode {
                all: node.is_all(),
                element: node.element(),
                keys: node
                    .members()
                    .map(|(name, child)| (Box::from(name.as_bytes()), child))
                    .collect(),
            });
        }
        Some(Self { nodes })
    }

    fn node(&self, id: u32) -> Option<&HintNode> {
        self.nodes.get(id as usize)
    }

    /// Resolves one OBJECT member demand: hit-or-omit with no join — a key the subtree does not name is omitted,
    /// everything else keeps whole.
    fn child_for_key(&self, node: u32, key: &[u8]) -> u32 {
        if node == Self::ALL {
            return Self::ALL;
        }
        let Some(entry) = self.node(node) else {
            return Self::ALL;
        };
        if entry.all {
            return Self::ALL;
        }
        match entry.keys.binary_search_by(|(name, _)| name.as_ref().cmp(key)) {
            Ok(index) => entry.keys[index].1,
            // Hit-or-element with no join — the engine pre-folds element demand into named keys exactly as it does
            // for the codec's map; an un-named key falls back to the element demand, else omit.
            Err(_) => entry.element.unwrap_or(Self::OMIT),
        }
    }

    /// Resolves one ARRAY element demand: arrays never omit — omission would shift indices and counts — so an array
    /// without an element node keeps every element whole.
    fn element_of(&self, node: u32) -> u32 {
        if node == Self::ALL {
            return Self::ALL;
        }
        let Some(entry) = self.node(node) else {
            return Self::ALL;
        };
        if entry.all {
            return Self::ALL;
        }
        entry.element.unwrap_or(Self::ALL)
    }
}

/// One open container on the reader's stack.
enum Frame {
    /// An open array and the elements read so far.
    Array(Array),
    /// An open object, its pairs, and the key whose value is still to come.
    ///
    /// A consumed key is pushed onto the same stack as the containers, which is why it costs a depth slot. Holding it
    /// inside the frame makes the pairing structural: a key cannot outlive its object.
    Object {
        /// The pairs read so far, in first-position last-value order.
        pairs: ObjectBuilder,
        /// How many pairs have been collected, which `}` needs and the builder does not publish.
        count: usize,
        /// The key a `:` consumed, if one is waiting for its value.
        key: Option<ObjectKey>,
    },
}

/// Which container a delimiter opens or closes.
#[derive(Clone, Copy)]
enum Container {
    /// `[` and `]`.
    Array,
    /// `{` and `}`.
    Object,
}

/// A byte that ends a bare token, and what it does.
#[derive(Clone, Copy)]
enum Delimiter {
    /// Space, tab, carriage return, newline — and nothing else. The set is narrower than `isspace`: `0x0b` and `0x0c`
    /// are token bytes, so `"\u000b1"` is `Invalid numeric literal`.
    Whitespace,
    /// `"`, opening a string.
    Quote,
    /// `[` or `{`.
    Open(Container),
    /// `]` or `}`.
    Close(Container),
    /// `:`.
    Colon,
    /// `,`.
    Comma,
}

/// Classifies one byte outside a string, or `None` when it belongs to a token.
const fn delimiter(byte: u8) -> Option<Delimiter> {
    match byte {
        b' ' | b'\t' | b'\r' | b'\n' => Some(Delimiter::Whitespace),
        b'"' => Some(Delimiter::Quote),
        b'[' => Some(Delimiter::Open(Container::Array)),
        b'{' => Some(Delimiter::Open(Container::Object)),
        b']' => Some(Delimiter::Close(Container::Array)),
        b'}' => Some(Delimiter::Close(Container::Object)),
        b':' => Some(Delimiter::Colon),
        b',' => Some(Delimiter::Comma),
        _ => None,
    }
}

/// Finds the first byte in `body` that ends a bare token: RFC 8259 whitespace plus the structural bytes `"[]{}:,` —
/// [`delimiter`]'s set.
///
/// The first byte is tested before any scan: most tokens in a record stream end immediately (a `1` followed by a
/// separator), and a memchr pass over the WHOLE remaining window just to answer "index 0" is the pool this function
/// exists to close. For a longer token, memchr3 has no 12-needle form, so the set is split into four 3-needle scans and
/// the earliest match wins; `0xFF` fills the 12th needle's slot and can never match, because `body` comes from a `&str`
/// and `0xFF` is not a valid UTF-8 byte.
fn token_delimiter(body: &[u8]) -> Option<usize> {
    if delimiter(body[0]).is_some() {
        return Some(0);
    }
    // Each pass is bounded by the best position found so far, so a pass can never scan past the current answer — four
    // passes over one run stay O(run), never O(run × passes) or worse. The calls are SEQUENTIAL for exactly this
    // reason: an array literal would evaluate every memchr3 over the full body before any bounding could apply.
    let mut limit = body.len();
    let mut best: Option<usize> = None;
    let found = memchr::memchr3(b'\t', b'\n', b'\r', &body[..limit]);
    if let Some(index) = found
        && best.is_none_or(|current| index < current)
    {
        best = Some(index);
        limit = index;
    }
    let found = memchr::memchr3(b' ', b'"', b',', &body[..limit]);
    if let Some(index) = found
        && best.is_none_or(|current| index < current)
    {
        best = Some(index);
        limit = index;
    }
    let found = memchr::memchr3(b':', b'[', b']', &body[..limit]);
    if let Some(index) = found
        && best.is_none_or(|current| index < current)
    {
        best = Some(index);
        limit = index;
    }
    let found = memchr::memchr3(b'{', b'}', 0xFF, &body[..limit]);
    if let Some(index) = found
        && best.is_none_or(|current| index < current)
    {
        best = Some(index);
    }
    best
}

/// The byte-at-a-time reader over one piece of text.
struct Reader<'text, 'resources> {
    /// The bytes being scanned: the input past a leading BOM, which is stripped before counting starts (`"\u{feff}x"`
    /// faults at `column 1`).
    scanned: &'text str,
    /// 1 plus the newlines consumed.
    line: u32,
    /// Bytes consumed on the current line.
    column: u32,
    /// Whether the last byte has been consumed, which decides `at EOF`.
    at_eof: bool,
    /// What the scanner is in the middle of.
    mode: Mode,
    /// The open containers, outermost first.
    frames: Vec<Frame>,
    /// The slots those frames occupy, which is not `frames.len()`: an object with a key waiting occupies two.
    slots: usize,
    /// The complete value waiting to be placed into its container, or to settle.
    next: Option<Value>,
    /// The first complete top-level value.
    settled: Option<Value>,
    /// The budget every payload is charged against.
    resources: &'resources ResourceContext<'resources>,
    /// The kept-subtree hint for this pull, when the drive derived one.
    prune: Option<PruneHint>,
    /// The hint node each open frame resolves its children against, parallel to `frames` and seeded with the document
    /// root (id 0), which is never popped. `ALL` short-circuits every lookup below it.
    prune_ids: Vec<u32>,
    /// The demand waiting for the member/element value about to start:
    /// a node id, [`PruneHint::ALL`], or [`PruneHint::OMIT`]. Consumed by whichever delimiter opens the value.
    pending_prune: Option<u32>,
    /// An omitted-subtree scan in progress: bytes are consumed and validated but no value is built. `Scalar` ends when
    /// its own string closes or token completes; `Container` counts the containers opened inside it (charged against
    /// [`DEPTH_SLOTS`] so an omitted subtree cannot nest past where whole decode would refuse) and ends at the close
    /// that matches its root.
    omit: Option<Omit>,
    /// Set when an omitted SCALAR member just completed: the parent object owes the pair bookkeeping (key released,
    /// count advanced) but has no value to insert. Consumed by the next `,` or `}`.
    pair_skip: bool,
}

/// Which kind of omitted subtree is being scanned; see `Reader::omit`.
enum Omit {
    Scalar,
    Container { depth: usize },
}

impl<'text, 'resources> Reader<'text, 'resources> {
    /// Starts a reader over `text`, past one leading BOM — the same literal as `new_at`'s, differing only in the BOM
    /// strip.
    fn new(text: &'text str, resources: &'resources ResourceContext<'resources>) -> Self {
        Self::new_at(text.strip_prefix('\u{feff}').unwrap_or(text), resources)
    }

    /// Starts a reader over one value of a LARGER stream, with NO BOM stripping: the stream driver owns the single
    /// source-start BOM and every later BOM must fault as ordinary token bytes. Positions are LOCAL to this value's
    /// slice; the stream driver lifts them to absolute coordinates from the slice's start offset.
    fn new_at(text: &'text str, resources: &'resources ResourceContext<'resources>) -> Self {
        Self {
            scanned: text,
            line: 1,
            column: 0,
            at_eof: false,
            mode: Mode::Between,
            frames: Vec::new(),
            slots: 0,
            next: None,
            settled: None,
            resources,
            prune: None,
            prune_ids: alloc::vec![PruneHint::ROOT_SEED],
            pending_prune: None,
            omit: None,
            pair_skip: false,
        }
    }

    /// Scans until the FIRST complete top-level value settles, returning it and the byte offset (within `scanned`)
    /// where it ended.
    ///
    /// The caller owns the next value's start: everything after `end` is deliberately not scanned, so a second adjacent
    /// value is not refused as `Unexpected extra JSON values` — the stream form parses a stream of values, and the
    /// single-value `Unexpected extra` law belongs to the one-value readers only.
    fn read_one(&mut self) -> Result<(Value, usize), Failure> {
        let bytes = self.scanned.as_bytes();
        let mut offset = 0;
        while offset < bytes.len() {
            let (next, settle_pos) = self.step_run(offset, bytes.len())?;
            offset = next;
            if let Some(value) = self.settled.take() {
                // settle() ran at `settle_pos` — the first non-whitespace byte of the batch — exactly the byte the
                // per-byte reader reported as the value's end.
                return Ok((value, settle_pos.unwrap_or(offset)));
            }
        }
        self.at_eof = true;
        let value = self.finish()?;
        Ok((value, self.scanned.len()))
    }

    /// Scans every byte, then finishes.
    fn read(&mut self) -> Result<Value, Failure> {
        let bytes = self.scanned.as_bytes();
        let mut offset = 0;
        while offset < bytes.len() {
            let (next, _) = self.step_run(offset, bytes.len())?;
            offset = next;
        }
        self.at_eof = true;
        self.finish()
    }

    /// Processes the bytes in `[offset, end)`, returning the next offset to process and, when a value settled during
    /// the call, the byte position at which it settled (always the first non-whitespace byte of the batch — the byte
    /// handed to [`Self::step`], which is the only place settle fires).
    ///
    /// The modes that can batch consume a whole run: a string body scans to its next `"` or `\` (memchr2 — the
    /// per-record value floor's byte-at-a-time pool), and Between-mode whitespace skips its whole run with an inline
    /// count. The single-byte modes — an escape body byte, a bare-token byte, or any delimiter/opener the whitespace
    /// skip hands over — consume exactly one byte through [`Self::step`].
    ///
    /// Line/column are counted per byte exactly as per-byte stepping would (they are the only cross-byte state), so a
    /// batched run is indistinguishable from stepping it one byte at a time; the stream reader's
    /// [`JsonStreamStep::NeedMore`] frontier is a mode test, so a batch that runs to the window's end leaves the same
    /// mode a full per-byte pass would.
    fn step_run(&mut self, offset: usize, end: usize) -> Result<(usize, Option<usize>), Failure> {
        let bytes = self.scanned.as_bytes();
        match self.mode {
            Mode::Text { start } => {
                let Some(index) = memchr::memchr2(b'"', b'\\', &bytes[offset..end]) else {
                    self.advance_count(offset, end);
                    return Ok((end, None));
                };
                let found = offset + index;
                self.advance_count(offset, found + 1);
                if bytes[found] == b'"' {
                    self.close_text(start, found)?;
                } else {
                    self.mode = Mode::Escape { start };
                }
                Ok((found + 1, None))
            }
            Mode::Between => {
                let whitespace = bytes[offset..end]
                    .iter()
                    .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
                    .unwrap_or(end - offset);
                let next = offset + whitespace;
                self.advance_count(offset, next);
                if next == end {
                    return Ok((end, None));
                }
                self.step(next, bytes[next])?;
                // The per-byte reader settles at the FIRST whitespace byte of the run, so the settle position is the
                // batch start (the byte the skipped run began at), not the non-whitespace byte this arm eventually
                // stepped.
                let settle_pos = self.settled.is_some().then_some(offset);
                Ok((next + 1, settle_pos))
            }
            // An escape body byte: one byte, exactly as before — the escape itself is the rare case and its handling
            // is two bytes wide.
            Mode::Escape { .. } => {
                self.step(offset, bytes[offset])?;
                Ok((offset + 1, None))
            }
            // A bare-token run: scan ahead to the next delimiter and hand that byte to the ordinary single-byte path
            // (which closes the token, settles, and dispatches). A token may not skip bytes, because its spelling is
            // the value — but finding its end is a scan, and the parse only runs when the end is known.
            Mode::Token { .. } => {
                let Some(index) = token_delimiter(&bytes[offset..end]) else {
                    self.advance_count(offset, end);
                    return Ok((end, None));
                };
                let found = offset + index;
                // Count the run up to (not including) the delimiter; the `step` below counts the delimiter itself,
                // exactly once.
                self.advance_count(offset, found);
                self.step(found, bytes[found])?;
                let settle_pos = self.settled.is_some().then_some(found);
                Ok((found + 1, settle_pos))
            }
        }
    }

    /// Counts the bytes in `[offset, end)` into line/column, exactly as per-byte stepping would: every `\n` ends a
    /// line, every other byte advances the column.
    fn advance_count(&mut self, offset: usize, end: usize) {
        let bytes = &self.scanned.as_bytes()[offset..end];
        // ponytail: naive newline count; bytecount is not a dependency and the loop is already branch-predictable over
        // real input.
        #[expect(clippy::naive_bytecount, reason = "bytecount is not a dependency")]
        let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
        self.line = self.line.saturating_add(u32::try_from(newlines).unwrap_or(u32::MAX));
        self.column = match bytes.iter().rposition(|byte| *byte == b'\n') {
            Some(last) => u32::try_from(end - offset - last - 1).unwrap_or(u32::MAX),
            None => self
                .column
                .saturating_add(u32::try_from(end - offset).unwrap_or(u32::MAX)),
        };
    }

    /// Consumes one byte.
    fn step(&mut self, offset: usize, byte: u8) -> Result<(), Failure> {
        if byte == b'\n' {
            self.line = self.line.saturating_add(1);
            self.column = 0;
        } else {
            self.column = self.column.saturating_add(1);
        }
        match self.mode {
            Mode::Text { start } => {
                if byte == b'"' {
                    return self.close_text(start, offset);
                }
                if byte == b'\\' {
                    self.mode = Mode::Escape { start };
                }
                Ok(())
            }
            // One byte after a backslash is body whatever it is, so an escaped quote does not close the string.
            Mode::Escape { start } => {
                self.mode = Mode::Text { start };
                Ok(())
            }
            Mode::Between | Mode::Token { .. } => self.step_outside(offset, byte),
        }
    }

    /// Consumes one byte that is not inside a string.
    fn step_outside(&mut self, offset: usize, byte: u8) -> Result<(), Failure> {
        let Some(delimiter) = delimiter(byte) else {
            if matches!(self.mode, Mode::Between) {
                // A token may follow a settled top-level value with no whitespace at all (`[1]2`), so the settle cannot
                // wait for the token to close.
                self.settle()?;
                let member = self.take_pending();
                if member == PruneHint::OMIT {
                    self.omit = Some(Omit::Scalar);
                }
                self.mode = Mode::Token { start: offset };
            }
            return Ok(());
        };
        // The delimiter may CLOSE an omitted scalar (its token/string ends here), so the token close runs before the
        // omit interception — `close_token` discards and ends a `Scalar` omission itself.
        self.close_token(offset)?;
        if let Some(Omit::Scalar) = self.omit {
            // The delimiter just consumed ended the omitted scalar; fall through with the skip bookkeeping armed (see
            // `pair_skip`).
        } else if self.omit.is_some() {
            return self.step_omitting(offset, delimiter);
        }
        self.settle()?;
        match delimiter {
            Delimiter::Whitespace => Ok(()),
            Delimiter::Quote => {
                if self.take_pending() == PruneHint::OMIT {
                    self.omit = Some(Omit::Scalar);
                }
                self.mode = Mode::Text { start: offset + 1 };
                Ok(())
            }
            Delimiter::Open(container) => {
                let member = self.take_pending();
                if member == PruneHint::OMIT {
                    // The pending demand omits this whole container: scan and validate it, build nothing.
                    self.omit = Some(Omit::Container { depth: 0 });
                    Ok(())
                } else {
                    self.open(container, member)
                }
            }
            Delimiter::Close(Container::Array) => self.close_array(),
            Delimiter::Close(Container::Object) => {
                if core::mem::take(&mut self.pair_skip) {
                    self.close_object_skipping_last()
                } else {
                    self.close_object()
                }
            }
            Delimiter::Colon => {
                self.pair_skip = false;
                self.begin_pair()
            }
            Delimiter::Comma => {
                if core::mem::take(&mut self.pair_skip) {
                    self.release_skipped_key()
                } else {
                    self.separate()
                }
            }
        }
    }

    /// The pair bookkeeping of [`Self::separate`] for a member whose value was omitted: the key is released and the
    /// count advanced, but nothing is inserted — the parent object continues exactly as if a value had been built and
    /// dropped.
    fn release_skipped_key(&mut self) -> Result<(), Failure> {
        match self.frames.last_mut() {
            Some(Frame::Object { count, key: slot, .. }) => {
                let key = slot.take().ok_or(PAIRS)?;
                drop(key);
                self.slots -= 1;
                *count += 1;
                Ok(())
            }
            _ => Err(COMMA_OUTSIDE_CONTAINER.into()),
        }
    }

    /// [`Self::close_object`] for an object whose LAST member's value was omitted: identical bookkeeping, no insertion.
    fn close_object_skipping_last(&mut self) -> Result<(), Failure> {
        if self.frames.is_empty() {
            return Err(UNMATCHED_BRACE.into());
        }
        let frame = self.frames.pop();
        self.slots -= 1;
        match frame {
            Some(Frame::Object {
                pairs, key: Some(key), ..
            }) => {
                drop(key);
                self.slots -= 1;
                self.finished_object(pairs)
            }
            // A skipped value with no key waiting (`{"a"}`-shaped) refuses like the floor's pairs message.
            _ => Err(PAIRS.into()),
        }
    }

    /// Consumes one delimiter while an omitted subtree is being scanned:
    /// bytes are counted and validated exactly as whole decode would count them, but nothing is built and the reader's
    /// own stack is untouched.
    /// Strings still open Text mode (their bodies scan and validate like any other); a close matching the subtree's
    /// root ends the omission — the parent then sees the ordinary `,`/`}`/`]` that follows.
    fn step_omitting(&mut self, offset: usize, delimiter: Delimiter) -> Result<(), Failure> {
        match delimiter {
            Delimiter::Whitespace | Delimiter::Colon | Delimiter::Comma => Ok(()),
            Delimiter::Quote => {
                self.mode = Mode::Text { start: offset + 1 };
                Ok(())
            }
            Delimiter::Open(_) => {
                // Counted against the same ceiling as built containers so an omitted subtree cannot nest past where
                // whole decode would have refused.
                let omit = self.omit.as_mut().ok_or(COMMA_OUTSIDE_CONTAINER)?;
                match omit {
                    Omit::Container { depth } => {
                        *depth += 1;
                        if *depth >= DEPTH_SLOTS {
                            return Err(DEPTH.into());
                        }
                        Ok(())
                    }
                    // A scalar omission contains no opener: its first byte started a string or token, and any
                    // structural byte in it is string body this arm never sees.
                    Omit::Scalar => Ok(()),
                }
            }
            Delimiter::Close(_) => {
                let end = matches!(self.omit, Some(Omit::Container { depth: 0 }));
                match &mut self.omit {
                    Some(Omit::Container { depth }) if !end => {
                        *depth -= 1;
                        Ok(())
                    }
                    _ => {
                        self.omit = None;
                        // The subtree WAS some member's value: the parent owes the pair bookkeeping without a value.
                        self.pair_skip = true;
                        Ok(())
                    }
                }
            }
        }
    }

    /// Takes the pending member/element demand, defaulting to ALL when no hint is active or nothing is pending (root
    /// values, hint-free frames).
    fn take_pending(&mut self) -> u32 {
        if self.prune.is_none() {
            return PruneHint::ALL;
        }
        self.pending_prune.take().unwrap_or(PruneHint::ALL)
    }

    /// Reads what a bare token spelled, if one was open.
    fn close_token(&mut self, end: usize) -> Result<(), Failure> {
        let Mode::Token { start } = self.mode else {
            return Ok(());
        };
        self.mode = Mode::Between;
        let value = token(&self.scanned[start..end])?;
        // An omitted SCALAR subtree ends here: the token was validated by `token()` above, then dropped instead of
        // published. A token inside an omitted CONTAINER is discarded without ending it — only the matching close
        // delimiter can do that.
        if let Some(Omit::Scalar) = self.omit {
            self.omit = None;
            self.pair_skip = true;
            return Ok(());
        }
        if self.omit.is_some() {
            return Ok(());
        }
        self.publish(value)
    }

    /// Decodes a closed string's body into its value.
    fn close_text(&mut self, start: usize, end: usize) -> Result<(), Failure> {
        self.mode = Mode::Between;
        let value = unescape(&self.scanned[start..end], self.resources)?;
        // Same split as `close_token`: validated always, published only when not omitting; a Scalar omission ends at
        // its own string's close.
        if let Some(Omit::Scalar) = self.omit {
            self.omit = None;
            self.pair_skip = true;
            return Ok(());
        }
        if self.omit.is_some() {
            return Ok(());
        }
        self.publish(value)
    }

    /// Records one complete value as the pending one.
    fn publish(&mut self, value: Value) -> Result<(), Failure> {
        if self.next.is_some() {
            return Err(SEPARATOR.into());
        }
        self.next = Some(value);
        Ok(())
    }

    /// Hands a completed top-level value over, or refuses a second one.
    ///
    /// The caller reads exactly one value and then asks for another, so a second complete value is `Unexpected extra
    /// JSON values` — while a second value that FAULTS reports its own fault (`'"éé" x'` is a numeric-literal
    /// refusal, not an extra-values one).
    fn settle(&mut self) -> Result<(), Failure> {
        if !self.frames.is_empty() || self.next.is_none() {
            return Ok(());
        }
        if self.settled.is_some() {
            return Err(EXTRA_VALUES.into());
        }
        self.settled = self.next.take();
        Ok(())
    }

    /// Opens a container.
    fn open(&mut self, container: Container, member: u32) -> Result<(), Failure> {
        if self.next.is_some() {
            return Err(SEPARATOR.into());
        }
        if self.slots >= DEPTH_SLOTS {
            return Err(DEPTH.into());
        }
        // The container being opened IS the pending member value: its own children resolve against the demand that
        // named it (`ALL` keeps everything; an array frame resolves elements against its element node, which is ALL
        // when absent — arrays never omit). The OMIT sentinel never reaches here (the delimiter arm starts the omit
        // scan instead); over-delivery keeps it sound if it ever does.
        let frame_id = if member != PruneHint::ALL && member != PruneHint::OMIT {
            member
        } else if let Some(hint) = &self.prune {
            let current = *self.prune_ids.last().unwrap_or(&PruneHint::ROOT_SEED);
            match container {
                Container::Array => hint.element_of(current),
                Container::Object => current,
            }
        } else {
            PruneHint::ALL
        };
        let frame = match container {
            Container::Array => Frame::Array(Array::try_new()?),
            Container::Object => Frame::Object {
                pairs: ObjectBuilder::new(),
                count: 0,
                key: None,
            },
        };
        self.frames.try_reserve(1).map_err(|_| Failure::Allocation)?;
        self.prune_ids.try_reserve(1).map_err(|_| Failure::Allocation)?;
        self.frames.push(frame);
        self.prune_ids.push(frame_id);
        self.slots += 1;
        Ok(())
    }

    /// Consumes a `:`, taking the pending value as a key.
    fn begin_pair(&mut self) -> Result<(), Failure> {
        let key = self.next.take().ok_or(KEY_BEFORE_COLON)?;
        let Some(Frame::Object { key: slot, .. }) = self.frames.last_mut() else {
            return Err(COLON_OUTSIDE_OBJECT.into());
        };
        // A key already waiting means the stack top is that key rather than its object, which is the same refusal
        // (`{"a":"b":1}`).
        if slot.is_some() {
            return Err(COLON_OUTSIDE_OBJECT.into());
        }
        let Value::String(text) = key.untagged() else {
            return Err(STRING_KEYS.into());
        };
        // Resolve the member's demand NOW, while the key text is at hand:
        // whichever delimiter opens the value consumes it.
        if self.prune.is_some() && self.omit.is_none() {
            let top = *self.prune_ids.last().unwrap_or(&PruneHint::ROOT_SEED);
            self.pending_prune = Some(
                self.prune
                    .as_ref()
                    .map_or(PruneHint::ALL, |hint| hint.child_for_key(top, text.as_bytes())),
            );
        }
        *slot = Some(ObjectKey::try_from_str(text.as_str())?);
        self.slots += 1;
        Ok(())
    }

    /// Consumes a `,`, placing the pending value into the open container.
    fn separate(&mut self) -> Result<(), Failure> {
        let value = self.next.take().ok_or(VALUE_BEFORE_COMMA)?;
        match self.frames.last_mut() {
            Some(Frame::Array(array)) => Ok(array.try_push(value)?),
            Some(Frame::Object {
                pairs,
                count,
                key: slot,
            }) => {
                // No key waiting means the pending value is where a key belongs (`{"a" ,1}`).
                let key = slot.take().ok_or(PAIRS)?;
                // The consumed key's slot is released HERE — the value's comma pops its stack entry: 50 000 rows of
                // 5-key objects (net zero per closed row) parse, and a 2500-level chain of 2-key objects (4 slots per
                // level) refuses. Counting the consumed key until its object closes made the ceiling shape-dependent
                // and lower — a 2500-row document refused at row 2500.
                self.slots -= 1;
                pairs.try_insert_last(key, value)?;
                *count += 1;
                Ok(())
            }
            // Unreachable: a pending value at top level has already settled, so the comma above sees none. The message
            // is spelled and cannot reach it either.
            None => Err(COMMA_OUTSIDE_CONTAINER.into()),
        }
    }

    /// Closes an array.
    ///
    /// The frame is inspected before the pending value, which is why every mispaired `]` is `Unmatched ']'` even with a
    /// value waiting (`{"a":1]`).
    fn close_array(&mut self) -> Result<(), Failure> {
        let Some(Frame::Array(mut array)) = self.frames.pop() else {
            return Err(UNMATCHED_BRACKET.into());
        };
        self.slots -= 1;
        if self.prune_ids.len() > 1 {
            self.prune_ids.pop();
        }
        match self.next.take() {
            Some(value) => array.try_push(value)?,
            // Nothing pending after at least one element is a trailing comma.
            None if !array.is_empty() => return Err(ANOTHER_ELEMENT.into()),
            None => {}
        }
        self.next = Some(Value::Array(array));
        Ok(())
    }

    /// Closes an object.
    ///
    /// The pending value is inspected before the frame, which is why `[1}` is the pairs message rather than an
    /// unmatched brace.
    fn close_object(&mut self) -> Result<(), Failure> {
        if self.frames.is_empty() {
            return Err(UNMATCHED_BRACE.into());
        }
        let pending = self.next.take();
        let frame = self.frames.pop();
        self.slots -= 1;
        if self.prune_ids.len() > 1 {
            self.prune_ids.pop();
        }
        match (pending, frame) {
            (
                Some(value),
                Some(Frame::Object {
                    mut pairs,
                    count: _,
                    key: Some(key),
                }),
            ) => {
                self.slots -= 1;
                pairs.try_insert_last(key, value)?;
                self.finished_object(pairs)
            }
            // A value with no key waiting for it (`{"a"}`), or a value in an array closed with a brace (`[1}`).
            (Some(_), _) => Err(PAIRS.into()),
            (
                None,
                Some(Frame::Object {
                    pairs,
                    count,
                    key: None,
                }),
            ) => {
                if count != 0 {
                    // Nothing pending after at least one pair is a trailing comma.
                    return Err(ANOTHER_PAIR.into());
                }
                self.finished_object(pairs)
            }
            // A key with no value (`{"a":}`), or an array closed with a brace.
            (None, _) => Err(UNMATCHED_BRACE.into()),
        }
    }

    /// Publishes one closed object, duplicate keys resolved by the last-value law.
    fn finished_object(&mut self, pairs: ObjectBuilder) -> Result<(), Failure> {
        // First position, last value: `{"b":1,"a":2,"b":3}` is `{"b":3,"a":2}`.
        let object = pairs.try_finish()?;
        self.next = Some(Value::Object(object));
        Ok(())
    }

    /// Finishes after the last byte.
    fn finish(&mut self) -> Result<Value, Failure> {
        match self.mode {
            Mode::Text { .. } | Mode::Escape { .. } => return Err(UNFINISHED_STRING.into()),
            Mode::Token { .. } => self.close_token(self.scanned.len())?,
            Mode::Between => {}
        }
        self.settle()?;
        if !self.frames.is_empty() {
            return Err(UNFINISHED_TERM.into());
        }
        self.settled.take().ok_or(EXPECTED_VALUE.into())
    }

    /// Builds the parse message for one reason.
    fn refusal(&self, reason: Reason, text: &str, resources: &ResourceContext<'_>) -> EngineRunError {
        let place = reason.placed.then_some(message::JsonPlace {
            line: self.line,
            column: self.column,
            at_eof: self.at_eof,
        });
        match message::json_parse_message(reason.text, place.as_ref(), echo(text)) {
            Ok(message) => raise(&message, resources),
            Err(error) => error,
        }
    }
}

/// The input as it is echoed into `while parsing '…'`.
///
/// The echo is built from a C string and so stops at the first NUL, while the scan keeps counting real bytes:
/// `"nope\u0000tail"` reports `column 9` and echoes `nope`. Nothing else is elided — the echo of a 10 001-deep input
/// is 20 076 characters long, and truncating it would fail the suite case that asserts so.
fn echo(text: &str) -> &str {
    match text.find('\0') {
        Some(index) => &text[..index],
        None => text,
    }
}

/// Reads what one bare token spelled.
///
/// The FIRST byte picks the branch, and the `n` rule is not the obvious one: jq routes `n` followed by `u` to the
/// `null` keyword and leaves every other `n` to the number reader, which is what lets `nan` be a number. So `nul` and
/// `nullx` are `Invalid literal` while `n`, `no` and `nan1` are `Invalid numeric literal`.
fn token(text: &str) -> Result<Value, Failure> {
    match text.as_bytes() {
        [b't', ..] => keyword(text, "true", Value::Bool(true)).map_err(Failure::from),
        [b'f', ..] => keyword(text, "false", Value::Bool(false)).map_err(Failure::from),
        [b'n', b'u', ..] => keyword(text, "null", Value::Null).map_err(Failure::from),
        // jq names the quote it wanted, at the delimiter that ended the token:
        // `{'a': 123}` faults at the colon, `column 5`.
        [b'\'', ..] => Err(QUOTE_LITERAL.into()),
        _ => number(text),
    }
}

/// One keyword, which has to match exactly — `truefalse` is one token.
fn keyword(text: &str, spelling: &str, value: Value) -> Result<Value, Reason> {
    if text == spelling {
        return Ok(value);
    }
    Err(KEYWORD_LITERAL)
}

/// One number, through the acceptor `tonumber` shares.
///
/// The spelling is read up to the first NUL and no further, which is jq's own answer rather than a shortcut: it parses
/// the token as a C string, so `"1\u00002"` is `1` while `"no\u0000pe"` still faults — and still counts all five
/// bytes in its column.
fn number(text: &str) -> Result<Value, Failure> {
    let spelling = match text.find('\0') {
        Some(index) => &text[..index],
        None => text,
    };
    match scan::number(spelling) {
        scan::NumberParse::Value(value) => Ok(value),
        scan::NumberParse::Refused => Err(NUMERIC_LITERAL.into()),
        // An allocation inside the numeric storage is the machine allocation class, never a parse refusal (the stream
        // doc's own law).
        scan::NumberParse::Allocation => Err(Failure::Allocation),
    }
}

/// Decodes one string body into its value.
///
/// A body with neither an escape nor a control byte is the common case and is published as it stands, without a second
/// copy.
fn unescape(body: &str, _resources: &ResourceContext<'_>) -> Result<Value, Failure> {
    if body.bytes().all(|byte| byte != b'\\' && byte >= 0x20) {
        return Ok(Value::try_string(body)?);
    }
    let mut out = String::new();
    out.try_reserve(body.len()).map_err(|_| Failure::Allocation)?;
    let mut rest = body;
    while let Some(index) = rest.find('\\') {
        let (plain, tail) = rest.split_at(index);
        push_plain(&mut out, plain)?;
        let (character, consumed) = escape(tail)?;
        push_char(&mut out, character)?;
        rest = &tail[consumed..];
    }
    push_plain(&mut out, rest)?;
    Ok(Value::try_string(&out)?)
}

/// Appends body text that carries no escape.
///
/// A raw byte below `0x20` is refused here, which is why a literal tab inside a string is a fault and `\t` is not. The
/// bound includes both ends: a raw NUL and a raw `U+001F` are equally refused, while `\u0000` and `\u001f` both decode.
fn push_plain(out: &mut String, plain: &str) -> Result<(), Failure> {
    if plain.bytes().any(|byte| byte < 0x20) {
        return Err(CONTROL_CHARACTERS.into());
    }
    out.try_reserve(plain.len()).map_err(|_| Failure::Allocation)?;
    out.push_str(plain);
    Ok(())
}

/// Appends one decoded character.
fn push_char(out: &mut String, character: char) -> Result<(), Failure> {
    out.try_reserve(character.len_utf8()).map_err(|_| Failure::Allocation)?;
    out.push(character);
    Ok(())
}

/// Decodes one escape, returning its character and the bytes it consumed.
///
/// `tail` starts at the backslash. The law itself is [`json_escape`]'s — this wrapper only maps a fault onto this
/// reader's reason type.
fn escape(tail: &str) -> Result<(char, usize), Reason> {
    json_escape::parse_escape(tail.as_bytes()).map_err(|fault| match fault {
        EscapeFault::AtEnd => ESCAPE_AT_END,
        EscapeFault::Invalid => INVALID_ESCAPE,
        EscapeFault::ShortUnicode => SHORT_UNICODE_ESCAPE,
        EscapeFault::BadDigits => UNICODE_ESCAPE_DIGITS,
        EscapeFault::SurrogatePair => SURROGATE_PAIR,
    })
}

#[cfg(test)]
mod tests {
    use super::{Failure, PruneHint, Reader, json};
    use alloc::string::{String, ToString};
    use jqf_data::Value;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    use crate::error::EngineRunError;
    use crate::semantics::render::to_json;

    /// A `{"v": *}` kept-subtree hint: the record's `v` member whole, everything else omittable — the shape `reduce
    /// inputs as $r (…$r.v)` derives.
    fn hint_v() -> PruneHint {
        let ledger = ledger();
        let mut tree = jqf_codec_core::PruneTree::try_new(&ledger).expect("tree");
        let v = tree.try_push_node(true).expect("node");
        tree.try_push_key(jqf_codec_core::PruneTree::ROOT, "v", v).expect("key");
        PruneHint::from_tree(&tree).expect("hint")
    }

    /// A `{"a": [*]}` hint: `a`'s ARRAY demanded with every element whole.
    fn hint_a_elements() -> PruneHint {
        let ledger = ledger();
        let mut tree = jqf_codec_core::PruneTree::try_new(&ledger).expect("tree");
        let a = tree.try_push_node(false).expect("node");
        let elements = tree.try_push_node(true).expect("node");
        tree.try_set_element(a, elements).expect("element");
        tree.try_push_key(jqf_codec_core::PruneTree::ROOT, "a", a).expect("key");
        PruneHint::from_tree(&tree).expect("hint")
    }

    /// One hinted stream step's value rendering (the consumed offset is asserted separately where it matters).
    fn stream_step_hinted(window: &str, at_eof: bool, hint: Option<&PruneHint>) -> String {
        match super::json_stream_next_hinted(window, at_eof, 0, 0, hint, &ledger())
            .expect("the hinted stream step allocates")
        {
            super::JsonStreamStep::Value { value, .. } => to_json(&value).expect("the value renders"),
            super::JsonStreamStep::Exhausted => "exhausted".to_string(),
            super::JsonStreamStep::NeedMore => "need-more".to_string(),
            super::JsonStreamStep::Refused(message) => alloc::format!("refused: {message}"),
        }
    }

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger: every string and container the reader builds is charged at its own construction,
    /// so nothing parses without an account.
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    /// The compact JSON of what the reader made of `text`.
    fn read(text: &str) -> String {
        let value = json(text, &ledger()).expect("the reader accepts this text");
        to_json(&value).expect("the value renders")
    }

    /// The compact JSON of what the SEQUENCE reader made of `text`.
    fn sequence(text: &str) -> String {
        let values = super::json_sequence(text, &ledger()).expect("the sequence reader accepts this text");
        to_json(&jqf_data::Value::Array(
            jqf_data::Array::try_from_vec(values).expect("the array allocates"),
        ))
        .expect("the value renders")
    }

    /// The message the SEQUENCE reader raises for `text`.
    fn sequence_refusal(text: &str) -> String {
        match super::json_sequence(text, &ledger()) {
            Err(EngineRunError::Raised(Value::String(message))) => message.as_str().to_string(),
            Err(other) => panic!("the sequence reader failed off its refusal channel: {other:?}"),
            Ok(values) => panic!("the sequence reader accepted {text:?} as {values:?}"),
        }
    }

    /// The message the reader raises for `text`.
    fn refusal(text: &str) -> String {
        match json(text, &ledger()) {
            Err(EngineRunError::Raised(Value::String(message))) => message.as_str().to_string(),
            Err(other) => panic!("the reader failed off its refusal channel: {other:?}"),
            Ok(value) => panic!("the reader accepted {text:?} as {value:?}"),
        }
    }

    /// A refusal carries the reason, its position clause and its whole echo.
    ///
    /// The positioned rows pin the column law (the column counts BYTES since that newline, so the failing third byte of
    /// line two reads column 4), a reason discovered by the settle of a top-level value (`:` after `1`), a fault found
    /// in `finish` (which places `at EOF` BEFORE the position), and an unpositioned reason (which carries no position
    /// clause at all). The echo is the whole input every time, newline included.
    #[test]
    fn a_refusal_carries_the_reason_position_and_echo() {
        assert_eq!(
            refusal("[1,\n2 3]"),
            "Expected separator between values at line 2, column 4 (while parsing '[1,\n2 3]')"
        );
        assert_eq!(
            refusal("1:2"),
            "Expected string key before ':' at line 1, column 2 (while parsing '1:2')"
        );
        assert_eq!(
            refusal("[1,"),
            "Unfinished JSON term at EOF at line 1, column 3 (while parsing '[1,')"
        );
        assert_eq!(refusal("[1]2"), "Unexpected extra JSON values (while parsing '[1]2')");
    }

    /// A leading byte-order mark is stripped, uncounted, and still echoed.
    ///
    /// The mark is consumed before the machine starts and never charges those three bytes to the column, so the `[`
    /// here is column 1 and the refusal that follows reads column 2 — the clause the markless text gives. The echo is
    /// the ORIGINAL text, mark included, because it is spelled from the input rather than from what the machine
    /// scanned.
    #[test]
    fn a_leading_byte_order_mark_is_stripped_and_uncounted() {
        assert_eq!(
            refusal("\u{feff}[1"),
            "Unfinished JSON term at EOF at line 1, column 2 (while parsing '\u{feff}[1')"
        );
        assert_eq!(read("\u{feff}[1]"), "[1]");
    }

    /// A duplicate key keeps the FIRST position and the LAST value.
    ///
    /// The two halves of the law fail independently: a builder that replaced in place would keep `1`, and one that
    /// appended would move `a` behind `b`.
    /// Rendering asserts both at once, being insertion-ordered.
    #[test]
    fn a_duplicate_key_keeps_the_first_position_and_the_last_value() {
        assert_eq!(read("{\"a\":1,\"b\":2,\"a\":3}"), "{\"a\":3,\"b\":2}");
    }

    /// Escapes decode by the pair-join law: a pair joins, a lone low surrogate replaces.
    ///
    /// A high surrogate followed by its low one is ONE scalar — the pair is read together, not as two unpaired units
    /// — while a low surrogate standing alone is U+FFFD rather than a refusal. The third row is the NUL escape, which
    /// survives into the string: the reader's own ECHO truncates at a NUL, and the decoded value must not.
    #[test]
    fn escapes_decode_as_jqs_pairs_replacements_and_nul() {
        assert_eq!(read("[\"\\ud83d\\ude00\"]"), "[\"\u{1f600}\"]");
        assert_eq!(read("[\"\\udc00\"]"), "[\"\u{fffd}\"]");
        assert_eq!(read("\"a\\u0000b\""), "\"a\\u0000b\"");
    }

    /// The slot ceiling matches the stack law: a key consumed by a comma is released, so the ceiling is a property of
    /// LIVE nesting, not of the total key count. 50 000 rows of 5-key objects (net zero slots per closed row) parse,
    /// and a 2500-level chain of 2-key objects (4 slots per level) is refused; the reader must land on the same
    /// boundary. Counting a consumed key until its object closes shifts the boundary down (a 2500-row document refused
    /// at row 2500) — this test pins the parity law.
    #[test]
    fn the_slot_ceiling_matches_the_stack_law() {
        fn chain(levels: usize) -> String {
            use core::fmt::Write as _;
            let mut out = String::new();
            for i in 0..levels {
                write!(out, "{{\"k{i}\": {{\"a\": {i}, \"b\": ").expect("render");
            }
            out.push_str("{\"end\": true}");
            out.push('}');
            for _ in 0..(2 * levels - 1) {
                out.push('}');
            }
            out
        }

        // recurses over its nesting, which is the big-stack CLI thread's job, not this test thread's.
        extern crate std;
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let outcome = {
                    let text = chain(2400);
                    let ledger = ledger();
                    let mut reader = Reader::new(&text, &ledger);
                    let outcome = reader.read();
                    core::mem::forget(reader);
                    outcome
                };
                assert!(outcome.is_ok(), "2400 levels must parse");
                let outcome = {
                    let text = chain(2500);
                    let ledger = ledger();
                    let mut reader = Reader::new(&text, &ledger);
                    let outcome = reader.read();
                    core::mem::forget(reader);
                    outcome
                };
                assert!(
                    matches!(
                        outcome,
                        Err(Failure::Refused(reason)) if reason.text.starts_with("Exceeds depth limit")
                    ),
                    "2500 levels must refuse with the depth message"
                );
            })
            .expect("big-stack probe thread")
            .join()
            .expect("probe thread");
    }

    /// The sequence reader collects every adjacent value.
    ///
    /// Whitespace separates values but is not required (`[1]2` is two values), an empty or whitespace-only stream is
    /// `[]`, and ONE leading BOM is stripped and uncounted exactly like the stdin stream's.
    #[test]
    fn a_sequence_collects_every_adjacent_value() {
        assert_eq!(sequence("1 2 3"), "[1,2,3]");
        assert_eq!(sequence("1\n2\n3\n"), "[1,2,3]");
        assert_eq!(sequence("[1]2"), "[[1],2]");
        assert_eq!(sequence("{\"a\":1}{\"b\":2}"), "[{\"a\":1},{\"b\":2}]");
        assert_eq!(sequence(""), "[]");
        assert_eq!(sequence("   \n\t \r"), "[]");
        assert_eq!(sequence("\u{feff}1\n2"), "[1,2]");
    }

    /// A sequence refusal reports an ABSOLUTE position in the whole stream.
    ///
    /// The values are one stream, not independent documents: the column after `1 garbage` counts the `1 ` prefix
    /// (column 9, not the second value's local 7), the fault after two newlines lands on line 4, and a token that
    /// begins on the settle byte of the previous value (`[1]2x`) still counts every byte exactly once. There is no
    /// echoed input: the `--slurpfile` message names a file, not a text.
    #[test]
    fn a_sequence_refusal_reports_an_absolute_position_without_echo() {
        assert_eq!(
            sequence_refusal("1 garbage"),
            "Invalid numeric literal at EOF at line 1, column 9"
        );
        assert_eq!(
            sequence_refusal("1\n2\nx\n3\n"),
            "Invalid numeric literal at line 4, column 0"
        );
        assert_eq!(
            sequence_refusal("[1]2x"),
            "Invalid numeric literal at EOF at line 1, column 5"
        );
        assert_eq!(
            sequence_refusal("{\"x\": 1} trailing"),
            "Invalid literal at EOF at line 1, column 17"
        );
        assert_eq!(
            sequence_refusal("1 \u{feff}2"),
            "Invalid numeric literal at EOF at line 1, column 6"
        );
    }

    /// One [`super::json_stream_next`] step over `window`, rendered for asserting: the settled value's compact JSON
    /// with its consumed length, or the step's own name.
    fn stream_step(window: &str, at_eof: bool, newlines: u64, column: u64) -> String {
        match super::json_stream_next(window, at_eof, newlines, column, &ledger()).expect("the stream step allocates") {
            super::JsonStreamStep::Value { value, consumed } => {
                let rendered = to_json(&value).expect("the value renders");
                alloc::format!("{rendered}@{consumed}")
            }
            super::JsonStreamStep::Exhausted => "exhausted".to_string(),
            super::JsonStreamStep::NeedMore => "need-more".to_string(),
            super::JsonStreamStep::Refused(message) => alloc::format!("refused: {message}"),
        }
    }

    /// The streaming step's hold rule: a window that ends inside a value — open frames, open string, or a bare token
    /// more bytes could extend (`12` could become `123`, `tru` could become `true`) — answers `NeedMore` rather than
    /// guessing, while a settled value names exactly the bytes it consumed and a whitespace-only window is exhausted.
    #[test]
    fn a_stream_step_holds_open_values_and_settles_complete_ones() {
        assert_eq!(stream_step("{\"a\": 1", false, 0, 0), "need-more");
        assert_eq!(stream_step("\"unter", false, 0, 0), "need-more");
        assert_eq!(stream_step("12", false, 0, 0), "need-more");
        assert_eq!(stream_step("tru", false, 0, 0), "need-more");
        assert_eq!(stream_step("[1,2]", false, 0, 0), "[1,2]@5");
        assert_eq!(stream_step(" {\"a\":1} {\"b\"", false, 0, 0), "{\"a\":1}@8");
        assert_eq!(stream_step("  \n ", false, 0, 0), "exhausted");
    }

    /// At EOF the same windows finalize instead of holding: a greedy token settles as the value it already is, an open
    /// frame refuses, and the refusal's position is ABSOLUTE — rebased by the caller's drained newline and column
    /// prefix, the whole-read sequence reader's own law.
    #[test]
    fn a_stream_step_finalizes_at_eof_with_absolute_positions() {
        assert_eq!(stream_step("12", true, 0, 0), "12@2");
        assert_eq!(
            stream_step("[1,", true, 0, 0),
            "refused: Unfinished JSON term at EOF at line 1, column 3"
        );
        // The `1\n2\n3\nx` stream reports exactly this line and column.
        assert_eq!(
            stream_step("x", true, 3, 0),
            "refused: Invalid numeric literal at EOF at line 4, column 1"
        );
        assert_eq!(
            stream_step("garbage", true, 0, 2),
            "refused: Invalid numeric literal at EOF at line 1, column 9"
        );
    }

    #[test]
    fn sequence_probe_small() {
        let values = super::json_sequence("99\n88", &ledger()).expect("decode");
        assert_eq!(values.len(), 2, "values: {values:?}");
    }

    /// Two 4096-digit integers in one sequence must decode — the token span-scan must not regress the sequence drive.
    #[test]
    fn two_big_numbers_decode() {
        let text = alloc::format!("{}\n{}", "9".repeat(4096), "9".repeat(4096));
        let values = super::json_sequence(&text, &ledger()).expect("decode");
        assert_eq!(values.len(), 2);
    }

    /// A long whitespace run between values must decode — the Between-mode batch skip must not regress the sequence
    /// drive.
    #[test]
    fn whitespace_runs_decode() {
        let text = alloc::format!("1{}1", " ".repeat(4096));
        let values = super::json_sequence(&text, &ledger()).expect("decode");
        assert_eq!(values.len(), 2);
    }

    // ------------------------------------------------------------------ The pulled-record prune hint (the streamed `-n
    // inputs` fold lane's field-pruned decode). The law: the pruned pull publishes exactly what the whole-decode floor
    // would have fed the fold body, and fails exactly where it would fail.

    /// Members outside the kept subtree are not built; members inside it survive with their whole payloads. Duplicate
    /// keys keep the last-value law under pruning (the scanner sees every occurrence).
    #[test]
    fn a_hinted_pull_builds_only_the_kept_subtree() {
        let hint = hint_v();
        assert_eq!(
            stream_step_hinted("{\"v\":{\"deep\":[1,2]},\"w\":9,\"x\":\"s\"}", true, Some(&hint)),
            "{\"v\":{\"deep\":[1,2]}}"
        );
        assert_eq!(stream_step_hinted("{\"v\":1,\"v\":2}", true, Some(&hint)), "{\"v\":2}");
        // A record lacking `v` entirely decodes to an empty object; the fold body reads null from it either way.
        assert_eq!(stream_step_hinted("{\"w\":3}", true, Some(&hint)), "{}");
    }

    /// Arrays never omit: an array demanded with an element node keeps every element whole (omission would shift
    /// indices and counts).
    #[test]
    fn arrays_keep_their_elements_whole() {
        let hint = hint_a_elements();
        assert_eq!(
            stream_step_hinted("{\"a\":[5,{\"x\":1},\"s\"],\"b\":2}", true, Some(&hint)),
            "{\"a\":[5,{\"x\":1},\"s\"]}"
        );
    }

    /// Grammar parity: a fault inside an OMITTED member refuses exactly as whole decode refuses — the scanners run
    /// over every byte, only the build is skipped. This is what makes the pruned pull's failures identical to the
    /// floor's.
    #[test]
    fn faults_inside_omitted_members_still_refuse() {
        let hint = hint_v();
        let broken = "{\"w\":\"\\uZZ\",\"v\":1}";
        let expected = match super::json_stream_next(broken, true, 0, 0, &ledger()) {
            Ok(super::JsonStreamStep::Value { value, .. }) => to_json(&value).expect("renders"),
            Ok(super::JsonStreamStep::Refused(message)) => alloc::format!("refused: {message}"),
            _ => String::from("other"),
        };
        assert_eq!(
            stream_step_hinted(broken, true, Some(&hint)),
            expected,
            "the hinted refusal must spell identically"
        );
        assert!(expected.starts_with("refused:"), "sanity");
    }

    /// An omitted subtree cannot nest past where whole decode would refuse:
    /// its containers are counted against the same ceiling class.
    #[test]
    fn omitted_subtrees_still_bound_nesting() {
        let hint = hint_v();
        let mut deep = String::from("{");
        deep.push_str("\"w\":[");
        for _ in 0..super::DEPTH_SLOTS + 10 {
            deep.push('[');
        }
        for _ in 0..super::DEPTH_SLOTS + 10 {
            deep.push(']');
        }
        deep.push_str("],\"v\":1}");
        let refused = stream_step_hinted(&deep, true, Some(&hint));
        assert!(
            refused.starts_with("refused:") && refused.contains("depth"),
            "an omitted chain must hit the depth bound, got: {refused}"
        );
    }

    /// A held value (`NeedMore`) is unaffected by the hint: the hold rule is a mode test on the same machine the hint
    /// rides.
    #[test]
    fn a_hint_does_not_change_the_hold_rule() {
        let hint = hint_v();
        assert_eq!(stream_step_hinted("{\"v\": 1, \"w\":", false, Some(&hint)), "need-more");
    }
}
