//! The bounded `--stream` event parser: a byte-at-a-time streaming JSON parser, under the strict-JSON acceptance
//! envelope.
//!
//! One job: turn one piece of JSON text into the STREAM of `[path, leaf]` events, one event at a time, WITHOUT
//! materializing the document. That is the "bounded" in `--stream`: `jqf --stream .` over a document larger than the
//! peak-RSS lane's ceiling stays under it because the parsed document never exists — only the current path stack and
//! the pending leaf do.
//!
//! # The machine is a byte-at-a-time state machine
//!
//! `--stream` is not a recursive walk: it is a byte-at-a-time state machine whose path bookkeeping, event emission (a
//! leaf completes → `[path, value]`, a non-empty container closes → `[path, [last-step]]`, an empty container
//! closes → `[path, {}|[]]`), line/column accounting, and error recovery are all carried by the current path stack
//! and the pending leaf — no recursion, no document. One scheduling law: EOF handling runs only when the final empty
//! chunk is polled — an unfinished string, a pending literal, an unclosed container, or — for a complete document
//! — one last `[path, value]`.
//!
//! # The strict envelope
//!
//! The stdin route is strict RFC 8259 while the parser is lenient (`01`, `+1`, `.5`, `1.`, `nan`, `inf` all parse; a
//! raw invalid UTF-8 byte in a string becomes U+FFFD; the streaming parser has no depth ceiling). The event route
//! accepts EXACTLY what the whole-document route accepts: lenient spellings are refused here with the same message
//! FORMAT (a reason sentence at a line/column), and those refusals are the catalogued strict-envelope divergence. Under
//! `--stream` a refusal is terminal (exit 5, prior events stand); under `--stream-errors` it becomes a `[message,
//! path]` event like any parse error, and only the terminal case — a malformed leading BOM — stays terminal.

use alloc::borrow::{Cow, ToOwned};
use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Array, Integer, Number, Value, ValueAllocationError};
use jqf_resource::ResourceContext;

use crate::error::{EngineRunError, message};
use crate::semantics::json_escape::{self, EscapeFault};

/// The line-fed chunk ceiling: input is read into a 4096-byte buffer with a 4-byte UTF-8 tail reservation, so a chunk
/// is at most 4091 bytes.
const CHUNK_MAX: usize = 4091;

const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The strict-JSON depth ceiling, the codec's whole-document guard. The STREAMING parser has no ceiling at all (`'[' ×
/// 10001` streams) — this is a strict-envelope refusal, recorded with the other strict-only rejections.
const DEPTH_SLOTS: usize = 10_000;

/// The reason sentences, verbatim. [`placed`] reasons get a position clause.
#[derive(Clone, Copy)]
pub struct Reason {
    pub text: &'static str,
    pub placed: bool,
}

const fn placed(text: &'static str) -> Reason {
    Reason { text, placed: true }
}

const NUMERIC_LITERAL: Reason = placed("Invalid numeric literal");
const KEYWORD_LITERAL: Reason = placed("Invalid literal");
const QUOTE_LITERAL: Reason = placed("Invalid string literal; expected \", but got '");
const SEPARATOR: Reason = placed("Expected separator between values");
const DEPTH: Reason = placed("Exceeds depth limit for parsing");
const KEY_BEFORE_COLON: Reason = placed("Expected string key before ':'");
const COLON_OUTSIDE_OBJECT: Reason = placed("':' not as part of an object");
const STRING_KEYS: Reason = placed("Object keys must be strings");
const SHOULD_FOLLOW_KEY: Reason = placed("':' should follow a key");
const VALUE_BEFORE_COMMA: Reason = placed("Expected value before ','");
const COMMA_OUTSIDE_CONTAINER: Reason = placed("',' not as part of an object or array");
const PAIRS: Reason = placed("Objects must consist of key:value pairs");
const ANOTHER_ELEMENT: Reason = placed("Expected another array element");
const ANOTHER_PAIR: Reason = placed("Expected another key-value pair");
const UNMATCHED_BRACKET: Reason = placed("Unmatched ']'");
const UNMATCHED_BRACE: Reason = placed("Unmatched '}'");
const MIDDLE_ARRAY: Reason = placed("Unmatched '}' in the middle of an array");
const MIDDLE_OBJECT: Reason = placed("Unmatched ']' in the middle of an object");
const STRING_KEY_ARRAY: Reason = placed("Expected string key after '{', not '['");
const STRING_KEY_OBJECT: Reason = placed("Expected string key after '{', not '{'");
const STRING_KEY_COMMA_ARRAY: Reason = placed("Expected string key after ',' in object, not '['");
const STRING_KEY_COMMA_OBJECT: Reason = placed("Expected string key after ',' in object, not '{'");
const UNFINISHED_TERM: Reason = placed("Unfinished JSON term");
const UNFINISHED_STRING: Reason = placed("Unfinished string");
const INVALID_ESCAPE: Reason = escape_reason(EscapeFault::Invalid);
const ESCAPE_AT_END: Reason = escape_reason(EscapeFault::AtEnd);
const SHORT_UNICODE_ESCAPE: Reason = escape_reason(EscapeFault::ShortUnicode);
const UNICODE_ESCAPE_DIGITS: Reason = escape_reason(EscapeFault::BadDigits);
const SURROGATE_PAIR: Reason = escape_reason(EscapeFault::SurrogatePair);

/// The verbatim sentence for an escape fault, shared with the whole-value reader through [`json_escape`].
const fn escape_reason(fault: EscapeFault) -> Reason {
    Reason {
        text: fault.text(),
        placed: true,
    }
}
const CONTROL_CHARACTERS: Reason =
    placed("Invalid string: control characters from U+0000 through U+001F must be escaped");
const MISSING_VALUE: Reason = placed("Missing value in key:value pair");
/// The strict-only UTF-8 refusal. The lenient parser accepts a raw invalid byte in a string (it becomes U+FFFD at
/// string construction); the strict envelope rejects it.
const INVALID_UTF8: Reason = placed("Invalid UTF-8 in string");

/// Why a read stopped.
#[derive(Clone, Copy)]
pub enum Failure {
    /// The text was refused (or the strict envelope refused a lenient spelling).
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
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Between tokens, accumulating a bare token when bytes come.
    Between,
    /// Inside a string.
    Text,
    /// Inside a string, one byte past a backslash.
    Escape,
}

/// What the last significant token established.
#[derive(Clone, Copy, PartialEq)]
enum LastSeen {
    None,
    Value,
    Comma,
    Colon,
    OpenArray,
    OpenObject,
}

/// The scan outcome of one byte.
enum ScanStop {
    Continue,
    Event(Value),
}

/// One terminal outcome of polling the event stream.
pub enum StreamEvent {
    /// One `[path, leaf]` / `[path]` / `[path, empty]` event value — or, under `--stream-errors`, a `[message, path]`
    /// parse-error event.
    Event(Value),
    /// The input was exhausted with no further events.
    Done,
    /// A terminal parse failure: the message text. Under `--stream` this is every parse refusal; under
    /// `--stream-errors` it is only the terminal case, a malformed leading BOM.
    Refused(String),
    /// The current buffer was exhausted without a complete event and more input is expected (the incremental shape):
    /// the drive must feed the next chunk through [`EventParser::set_buf`] and poll again.
    /// Never returned while the parser holds its LAST buffer (`is_last`).
    NeedMore,
}

/// The bounded `--stream` event parser, over one whole piece of retained text OR one incrementally-fed chunk stream.
///
/// The whole-input shape borrows the retained source (the seekable-stdin shape the peak-RSS lane measures); the parser
/// walks it in fgets-shaped chunks but never builds the document, so its residency is the source bytes plus the path
/// stack, not the parsed document.
///
/// The incremental shape owns its current buffer (a copy of the latest chunk the drive fed through
/// [`EventParser::set_buf`]) and returns [`StreamEvent::NeedMore`] when the buffer runs out without a complete event.
pub struct EventParser<'input> {
    /// The current source: a borrowed slice for the whole-input shape, the owned current chunk for the incremental
    /// shape.
    input: Cow<'input, [u8]>,
    /// The next byte to scan.
    pos: usize,
    /// The end of the current fgets-shaped chunk. On a parse error the rest of the chunk is thrown: `pos` jumps here.
    chunk_end: usize,
    /// Whether the final empty chunk's EOF handling has already run.
    eof: bool,
    /// Whether the current buffer is the LAST one: EOF handling runs only when the buffer runs out here.
    is_last: bool,
    /// 0..=3 bytes of the UTF-8 BOM matched so far (a partial prefix survives across buffers); `0xff` means a non-BOM
    /// byte followed a partial prefix — `Malformed BOM` on the next poll.
    bom: u8,
    /// 1 plus the newlines consumed.
    line: u32,
    /// Bytes consumed on the current line.
    column: u32,
    mode: Mode,
    /// The accumulated bare token / string body.
    token: Vec<u8>,
    /// The step stack of the value being parsed.
    path: Vec<Value>,
    /// The open-container count (`path.len()` equals this).
    stacklen: usize,
    last_seen: LastSeen,
    /// The complete value waiting to be placed or settled.
    next: Option<Value>,
    /// A pending event (`[path, last]` or `[path]`) waiting for the next poll.
    output: Option<Value>,
    /// Whether parse refusals become events (`--stream-errors`).
    stream_errors: bool,
}

impl<'input> EventParser<'input> {
    /// Starts a parser over the retained text, feeding it as ONE buffer: the whole-input shape (BORROWED, the peak-RSS
    /// lane's zero-copy law). BOM
    #[must_use]
    pub fn new(input: &'input [u8], stream_errors: bool) -> Self {
        let mut parser = Self::incremental(stream_errors);
        let stripped = parser.strip_bom(input);
        // A whole input ending with a partial BOM prefix promised three bytes and delivered fewer: the next poll
        // refuses it instead of parsing the stripped bytes as a complete (empty) document.
        if matches!(parser.bom, 1..=2) {
            parser.bom = 0xff;
        }
        parser.input = Cow::Borrowed(stripped);
        parser.is_last = true;
        parser.begin_chunk();
        parser
    }

    /// Starts an INCREMENTAL parser with no buffer: the drive feeds chunks with [`EventParser::set_buf`] and polls
    /// until [`StreamEvent::NeedMore`] tells it to feed again.
    #[must_use]
    pub fn incremental(stream_errors: bool) -> Self {
        Self {
            input: Cow::Owned(Vec::new()),
            pos: 0,
            chunk_end: 0,
            eof: false,
            is_last: false,
            bom: 0,
            line: 1,
            column: 0,
            mode: Mode::Between,
            token: Vec::new(),
            path: Vec::new(),
            stacklen: 0,
            last_seen: LastSeen::None,
            next: None,
            output: None,
            stream_errors,
        }
    }

    /// Feeds the next chunk: call only when the previous buffer was consumed ([`EventParser::remaining`] is zero),
    /// while the parse state (mode, token body, path, line/column) survives.
    /// The bytes are COPIED into the parser's owned buffer, so the drive may reuse its read scratch immediately.
    /// `is_last` marks the FINAL buffer:
    /// EOF handling runs when it runs out. BOM stripping spans buffers.
    pub fn set_buf(&mut self, buf: &[u8], is_last: bool) {
        let stripped = self.strip_bom(buf);
        // A final buffer ending with a partial BOM prefix is malformed at EOF:
        // the next poll refuses it rather than parsing the stripped bytes as if the prefix were complete.
        if is_last && matches!(self.bom, 1..=2) {
            self.bom = 0xff;
        }
        self.input = Cow::Owned(stripped.to_vec());
        self.pos = 0;
        self.is_last = is_last;
        self.begin_chunk();
    }

    /// The `set_buf` BOM law: match the UTF-8 BOM bytes in order; a partial prefix survives across buffers, a
    /// non-matching byte after a partial prefix is a malformed BOM refused on the next poll.
    fn strip_bom<'a>(&mut self, mut buf: &'a [u8]) -> &'a [u8] {
        while !buf.is_empty() && self.bom < 3 {
            if buf[0] == UTF8_BOM[usize::from(self.bom)] {
                buf = &buf[1..];
                self.bom += 1;
            } else if self.bom == 0 {
                self.bom = 3; // no BOM in this document
            } else {
                self.bom = 0xff; // malformed BOM prefix
            }
        }
        buf
    }

    /// The bytes of the current buffer not yet scanned; the drive feeds the next chunk through [`EventParser::set_buf`]
    /// when this is zero.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.input.len() - self.pos
    }

    /// The next event: one value event, a `--stream-errors` error event, the end of the stream, a terminal refusal, or
    /// — incremental shape only — [`StreamEvent::NeedMore`] when the current buffer is exhausted and more input is
    /// expected.
    pub fn next_event(&mut self, resources: &ResourceContext<'_>) -> Result<StreamEvent, EngineRunError> {
        // `Malformed BOM`: a non-BOM byte followed a partial BOM prefix.
        // Terminal under both flags, refused before anything else on the next poll.
        if self.bom == 0xff {
            return Ok(StreamEvent::Refused("Malformed BOM".to_owned()));
        }
        // A pending marker from a closed non-empty container is delivered before any further bytes are scanned
        // (`stream_check_done`).
        if let Some(marker) = self.output.take() {
            return Ok(StreamEvent::Event(marker));
        }
        loop {
            if self.pos < self.chunk_end {
                let stop = match self.scan_byte(self.input[self.pos], resources) {
                    Ok(stop) => stop,
                    Err(Failure::Allocation) => return Err(allocation()),
                    Err(Failure::Refused(reason)) => return self.refused(reason, resources),
                };
                self.pos += 1;
                match stop {
                    ScanStop::Continue => {}
                    ScanStop::Event(value) => return Ok(StreamEvent::Event(value)),
                }
                continue;
            }
            if self.pos >= self.input.len() {
                if self.is_last {
                    // The final empty chunk: EOF handling, exactly once.
                    if self.eof {
                        return Ok(StreamEvent::Done);
                    }
                    self.eof = true;
                    return self.finish(resources);
                }
                // The incremental shape: the buffer ran out mid-parse; the drive must feed the next chunk and poll
                // again.
                return Ok(StreamEvent::NeedMore);
            }
            self.begin_chunk();
        }
    }

    /// One parse refusal: an event under `--stream-errors`, terminal under `--stream`. The error event carries the
    /// message and the parser path at the failure point (`[message, path]`).
    fn refused(&mut self, reason: Reason, resources: &ResourceContext<'_>) -> Result<StreamEvent, EngineRunError> {
        let place = reason.placed.then_some(message::JsonPlace {
            line: self.line,
            column: self.column,
            at_eof: false,
        });
        let message = message::json_parse_message_head(reason.text, place.as_ref())?;
        if !self.stream_errors {
            return Ok(StreamEvent::Refused(message));
        }
        let event = self.error_event(&message, resources)?;
        self.reset();
        // The rest of the current chunk is thrown after a parse error; the next poll resumes at the next chunk's start.
        self.pos = self.chunk_end;
        Ok(StreamEvent::Event(event))
    }

    /// Builds a `[message, path]` error event from the current path.
    fn error_event(&mut self, message: &str, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        let mut event = Array::try_new().map_err(|_| allocation())?;
        let text = Value::try_string(message).map_err(|_| allocation())?;
        event.try_push(text).map_err(|_| allocation())?;
        let path = self.path_value(resources)?;
        event.try_push(path).map_err(|_| allocation())?;
        Ok(Value::Array(event))
    }

    /// `parser_reset`: clear everything a parse error throws away. The path is cleared only under streaming (it is
    /// always streaming here).
    fn reset(&mut self) {
        self.path.clear();
        self.stacklen = 0;
        self.last_seen = LastSeen::None;
        self.output = None;
        self.next = None;
        self.token.clear();
        self.mode = Mode::Between;
    }

    /// The current path as a `[step, …]` array value.
    fn path_value(&mut self, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        let mut array = Array::try_new().map_err(|_| allocation())?;
        for step in &self.path {
            array.try_push(step.clone()).map_err(|_| allocation())?;
        }
        Ok(Value::Array(array))
    }

    /// One event tuple `[path, value]` (a leaf or empty-container event).
    fn pair(&mut self, value: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        let mut array = Array::try_new().map_err(|_| allocation())?;
        let path = self.path_value(resources)?;
        array.try_push(path).map_err(|_| allocation())?;
        array.try_push(value.clone()).map_err(|_| allocation())?;
        Ok(Value::Array(array))
    }

    /// `stream_check_done`: hand over one pending event, or the top-level `[path, next]` when a top-level value just
    /// completed.
    fn check_done(&mut self, resources: &ResourceContext<'_>) -> Result<Option<Value>, Failure> {
        if self.stacklen == 0 && self.next.is_some() {
            let value = self.next.take().ok_or(Failure::Allocation)?;
            return self.pair(&value, resources).map(Some).map_err(|_| Failure::Allocation);
        }
        if let Some(output) = self.output.as_ref() {
            if array_of(output).len() > 2 {
                // A closed non-empty container: deliver `[path, value]` now and keep `[path]` for the next poll.
                let mut head = Array::try_new().map_err(|_| Failure::Allocation)?;
                for step in array_of(output).iter().take(2) {
                    head.try_push(step.clone()).map_err(|_| Failure::Allocation)?;
                }
                let head = Value::Array(head);
                let mut tail = Array::try_new().map_err(|_| Failure::Allocation)?;
                let first = self
                    .output
                    .as_ref()
                    .and_then(|o| array_of(o).iter().next())
                    .ok_or(Failure::Allocation)?
                    .clone();
                tail.try_push(first).map_err(|_| Failure::Allocation)?;
                self.output = Some(Value::Array(tail));
                return Ok(Some(head));
            }
            let delivered = self.output.take().ok_or(Failure::Allocation)?;
            return Ok(Some(delivered));
        }
        Ok(None)
    }

    /// Consumes one byte, the `scan` law (the position was already advanced; the counting happens here, before the
    /// byte's token is processed).
    fn scan_byte(&mut self, ch: u8, resources: &ResourceContext<'_>) -> Result<ScanStop, Failure> {
        if ch == b'\n' {
            self.line = self.line.saturating_add(1);
            self.column = 0;
        } else {
            self.column = self.column.saturating_add(1);
        }
        match self.mode {
            Mode::Text => {
                self.token.try_reserve(1).map_err(|_| Failure::Allocation)?;
                self.token.push(ch);
                if ch == b'"' {
                    // The closing quote is part of the accumulated body; drop it and decode.
                    self.token.pop();
                    let value = self.found_string(resources)?;
                    self.mode = Mode::Between;
                    self.publish(value)?;
                    return self.after_token(resources);
                }
                if ch == b'\\' {
                    self.mode = Mode::Escape;
                }
                Ok(ScanStop::Continue)
            }
            Mode::Escape => {
                self.token.try_reserve(1).map_err(|_| Failure::Allocation)?;
                self.token.push(ch);
                self.mode = Mode::Text;
                Ok(ScanStop::Continue)
            }
            Mode::Between => self.scan_outside(ch, resources),
        }
    }

    /// One byte that is not inside a string (the `scan` NORMAL branch).
    fn scan_outside(&mut self, ch: u8, resources: &ResourceContext<'_>) -> Result<ScanStop, Failure> {
        let class = classify(ch);
        if class != Class::Literal {
            self.check_literal()?;
            let event = self.check_done(resources)?;
            if let Some(event) = event {
                return Ok(ScanStop::Event(event));
            }
        }
        match class {
            Class::Literal => {
                self.token.try_reserve(1).map_err(|_| Failure::Allocation)?;
                self.token.push(ch);
            }
            Class::Whitespace => {}
            Class::Quote => {
                self.mode = Mode::Text;
            }
            Class::Structure => self.stream_token(ch, resources)?,
        }
        let event = self.check_done(resources)?;
        Ok(match event {
            Some(event) => ScanStop::Event(event),
            None => ScanStop::Continue,
        })
    }

    /// The `check_done` call after a token completed.
    fn after_token(&mut self, resources: &ResourceContext<'_>) -> Result<ScanStop, Failure> {
        let event = self.check_done(resources)?;
        Ok(match event {
            Some(event) => ScanStop::Event(event),
            None => ScanStop::Continue,
        })
    }

    /// `value()`: record one complete value as the pending one.
    fn publish(&mut self, value: Value) -> Result<(), Failure> {
        if self.next.is_some() || self.last_seen == LastSeen::Value {
            return Err(SEPARATOR.into());
        }
        self.last_seen = if self.stacklen > 0 {
            LastSeen::Value
        } else {
            LastSeen::None
        };
        self.next = Some(value);
        Ok(())
    }

    /// `check_literal`, under the strict envelope: keywords must match exactly and a number must be a strict JSON
    /// literal (the strict-only difference — the lenient number reader accepts `01`, `+1`, `.5`, `1.`, `nan`, `inf`,
    /// and NUL truncation).
    fn check_literal(&mut self) -> Result<(), Failure> {
        if self.token.is_empty() {
            return Ok(());
        }
        let value = match self.token.as_slice() {
            [b't', ..] => keyword(&self.token, b"true", Value::Bool(true)).map_err(Failure::from),
            [b'f', ..] => keyword(&self.token, b"false", Value::Bool(false)).map_err(Failure::from),
            [b'n', second, ..] if *second == b'u' => keyword(&self.token, b"null", Value::Null).map_err(Failure::from),
            [b'\'', ..] => Err(QUOTE_LITERAL.into()),
            _ => strict_number(&self.token),
        };
        self.token.clear();
        let value = value?;
        self.publish(value)
    }

    /// Decodes a closed string's body into its value.
    fn found_string(&mut self, resources: &ResourceContext<'_>) -> Result<Value, Failure> {
        let body = core::mem::take(&mut self.token);
        decode_string(&body, resources)
    }

    /// `stream_token` for one structure byte.
    ///
    /// Deliberately one faithful transcript of the state machine rather than a factored subset: the arms share
    /// `path`/`last_seen`/`output` bookkeeping and a split would obscure the byte-for-byte law they pin.
    #[allow(clippy::too_many_lines)]
    fn stream_token(&mut self, ch: u8, resources: &ResourceContext<'_>) -> Result<(), Failure> {
        match ch {
            b'[' => {
                if self.next.is_some() {
                    return Err(SEPARATOR.into());
                }
                if self.last_seen == LastSeen::OpenObject {
                    return Err(STRING_KEY_ARRAY.into());
                }
                if self.last_seen == LastSeen::Comma && !self.top_is_number() {
                    return Err(STRING_KEY_COMMA_ARRAY.into());
                }
                if self.stacklen >= DEPTH_SLOTS {
                    return Err(DEPTH.into());
                }
                self.path.try_reserve(1).map_err(|_| Failure::Allocation)?;
                self.path.push(index_value(0));
                self.last_seen = LastSeen::OpenArray;
                self.stacklen += 1;
            }
            b'{' => {
                if self.next.is_some() {
                    return Err(SEPARATOR.into());
                }
                if self.last_seen == LastSeen::OpenObject {
                    return Err(STRING_KEY_OBJECT.into());
                }
                if self.last_seen == LastSeen::Comma && !self.top_is_number() {
                    return Err(STRING_KEY_COMMA_OBJECT.into());
                }
                if self.stacklen >= DEPTH_SLOTS {
                    return Err(DEPTH.into());
                }
                self.path.try_reserve(1).map_err(|_| Failure::Allocation)?;
                self.path.push(Value::Null);
                self.last_seen = LastSeen::OpenObject;
                self.stacklen += 1;
            }
            b':' => {
                // The top must not be a NUMBER (an array frame); an unset key (null) and a consumed key (string) both
                // pass.
                if self.stacklen == 0 || self.top_is_number() {
                    return Err(COLON_OUTSIDE_OBJECT.into());
                }
                if self.next.is_none() || self.last_seen == LastSeen::None {
                    return Err(KEY_BEFORE_COLON.into());
                }
                let key = self.next.take().ok_or(Failure::Allocation)?;
                if !matches!(key.untagged(), Value::String(_)) {
                    return Err(STRING_KEYS.into());
                }
                if self.last_seen != LastSeen::Value {
                    return Err(SHOULD_FOLLOW_KEY.into());
                }
                self.last_seen = LastSeen::Colon;
                *self.path.last_mut().ok_or(Failure::Allocation)? = key;
            }
            b',' => {
                if self.last_seen != LastSeen::Value {
                    return Err(VALUE_BEFORE_COMMA.into());
                }
                if self.stacklen == 0 {
                    return Err(COMMA_OUTSIDE_CONTAINER.into());
                }
                let top = self.path.last().ok_or(Failure::Allocation)?;
                if matches!(top.untagged(), Value::Number(_)) {
                    if let Some(next) = self.next.take() {
                        self.output = Some(self.pair(&next, resources).map_err(|_| Failure::Allocation)?);
                    }
                    let top = self.path.last_mut().ok_or(Failure::Allocation)?;
                    let index = match top.untagged() {
                        Value::Number(number) => number.to_i64().ok_or(Failure::Allocation)?,
                        _ => return Err(Failure::Allocation),
                    };
                    *top = index_value(index + 1);
                    self.last_seen = LastSeen::Comma;
                } else if matches!(top.untagged(), Value::String(_)) {
                    if let Some(next) = self.next.take() {
                        self.output = Some(self.pair(&next, resources).map_err(|_| Failure::Allocation)?);
                    }
                    *self.path.last_mut().ok_or(Failure::Allocation)? = Value::Null;
                    self.last_seen = LastSeen::Comma;
                } else {
                    return Err(PAIRS.into());
                }
            }
            b']' => {
                if self.stacklen == 0 {
                    return Err(UNMATCHED_BRACKET.into());
                }
                if self.last_seen == LastSeen::Comma {
                    return Err(ANOTHER_ELEMENT.into());
                }
                let top = self.path.last().ok_or(Failure::Allocation)?;
                if !matches!(top.untagged(), Value::Number(_)) {
                    return Err(MIDDLE_OBJECT.into());
                }
                if let Some(next) = self.next.take() {
                    self.output = Some(self.triple(&next, resources)?);
                } else if self.last_seen != LastSeen::OpenArray {
                    self.output = Some(self.marker(resources)?);
                }
                self.path.truncate(self.stacklen.saturating_sub(1));
                self.stacklen = self.stacklen.saturating_sub(1);
                if self.last_seen == LastSeen::OpenArray {
                    self.output = Some(
                        self.pair(
                            &Value::Array(Array::try_new().map_err(|_| Failure::Allocation)?),
                            resources,
                        )
                        .map_err(|_| Failure::Allocation)?,
                    );
                }
                if self.stacklen == 0 {
                    self.last_seen = LastSeen::None;
                } else {
                    self.last_seen = LastSeen::Value;
                }
            }
            b'}' => {
                if self.stacklen == 0 {
                    return Err(UNMATCHED_BRACE.into());
                }
                if self.last_seen == LastSeen::Comma {
                    return Err(ANOTHER_PAIR.into());
                }
                let top = self.path.last().ok_or(Failure::Allocation)?;
                if matches!(top.untagged(), Value::Number(_)) {
                    return Err(MIDDLE_ARRAY.into());
                }
                if let Some(next) = self.next.take() {
                    if !matches!(top.untagged(), Value::String(_)) {
                        return Err(PAIRS.into());
                    }
                    self.output = Some(self.triple(&next, resources)?);
                } else {
                    if self.last_seen == LastSeen::Colon {
                        return Err(MISSING_VALUE.into());
                    }
                    if self.last_seen == LastSeen::Comma {
                        return Err(ANOTHER_PAIR.into());
                    }
                    if self.last_seen == LastSeen::OpenArray {
                        return Err(MIDDLE_ARRAY.into());
                    }
                    if self.last_seen != LastSeen::Value && self.last_seen != LastSeen::OpenObject {
                        return Err(UNMATCHED_BRACE.into());
                    }
                    if self.last_seen != LastSeen::OpenObject {
                        self.output = Some(self.marker(resources)?);
                    }
                }
                self.path.truncate(self.stacklen.saturating_sub(1));
                self.stacklen = self.stacklen.saturating_sub(1);
                if self.last_seen == LastSeen::OpenObject {
                    self.output = Some(
                        self.pair(
                            &Value::Object(jqf_data::Object::try_new().map_err(|_| Failure::Allocation)?),
                            resources,
                        )
                        .map_err(|_| Failure::Allocation)?,
                    );
                }
                if self.stacklen == 0 {
                    self.last_seen = LastSeen::None;
                } else {
                    self.last_seen = LastSeen::Value;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Whether the path's top is a number (an array frame).
    fn top_is_number(&self) -> bool {
        matches!(self.path.last().map(Value::untagged), Some(Value::Number(_)))
    }

    /// The `[path, value, true]` — the closed non-empty container's event, sliced by `check_done` into `[path,
    /// value]` then `[path]`.
    fn triple(&mut self, value: &Value, resources: &ResourceContext<'_>) -> Result<Value, Failure> {
        let mut array = Array::try_new().map_err(|_| Failure::Allocation)?;
        let path = self.path_value(resources).map_err(|_| Failure::Allocation)?;
        array.try_push(path).map_err(|_| Failure::Allocation)?;
        array.try_push(value.clone()).map_err(|_| Failure::Allocation)?;
        array.try_push(Value::Bool(true)).map_err(|_| Failure::Allocation)?;
        Ok(Value::Array(array))
    }

    /// The `[path + [last-step]]` — the marker for a closed non-empty container, delivered after its `[path, last]`
    /// event.
    fn marker(&mut self, _resources: &ResourceContext<'_>) -> Result<Value, Failure> {
        let mut path = Array::try_new().map_err(|_| Failure::Allocation)?;
        for step in &self.path {
            path.try_push(step.clone()).map_err(|_| Failure::Allocation)?;
        }
        let mut marker = Array::try_new().map_err(|_| Failure::Allocation)?;
        marker.try_push(Value::Array(path)).map_err(|_| Failure::Allocation)?;
        Ok(Value::Array(marker))
    }

    /// The next fgets-shaped chunk: up to [`CHUNK_MAX`] bytes ending at the first newline, or a [`CHUNK_MAX`]-byte run
    /// extended to complete a trailing multi-byte UTF-8 character (the backtrack).
    fn begin_chunk(&mut self) {
        let start = self.pos;
        let limit = (start + CHUNK_MAX).min(self.input.len());
        if let Some(offset) = self.input[start..limit].iter().position(|byte| *byte == b'\n') {
            self.chunk_end = start + offset + 1;
        } else {
            let mut end = limit;
            if limit < self.input.len() {
                end = self.utf8_backtrack_end(start, limit);
            }
            self.chunk_end = end;
        }
    }

    /// Extends a newline-free chunk to complete a multi-byte UTF-8 character whose tail was cut at the chunk boundary.
    #[must_use]
    fn utf8_backtrack_end(&self, start: usize, limit: usize) -> usize {
        if limit == start {
            return limit;
        }
        // Walk back over continuation bytes from the last byte.
        let mut lead = limit - 1;
        while lead > start && (0x80..=0xBF).contains(&self.input[lead]) {
            lead -= 1;
        }
        let Some(length) = utf8_coding_length(self.input[lead]) else {
            return limit;
        };
        let seen = limit - lead;
        if seen > usize::from(length) {
            return limit;
        }
        let missing = usize::from(length) - seen;
        if missing == 0 {
            return limit;
        }
        (limit + missing).min(self.input.len())
    }

    /// EOF handling, run once when the input is exhausted.
    fn finish(&mut self, resources: &ResourceContext<'_>) -> Result<StreamEvent, EngineRunError> {
        if self.mode == Mode::Text || self.mode == Mode::Escape {
            return self.refused_eof(UNFINISHED_STRING, resources);
        }
        if !self.token.is_empty() {
            // Bind the result: an Allocation failure from the literal check must propagate as the machine class, not
            // fall through to a normal Done after the token was already cleared.
            match self.check_literal() {
                Ok(()) => {}
                Err(Failure::Refused(reason)) => return self.refused_eof(reason, resources),
                Err(Failure::Allocation) => return Err(allocation()),
            }
        }
        if self.stacklen != 0 {
            return self.refused_eof(UNFINISHED_TERM, resources);
        }
        if self.next.is_some() {
            let value = self.next.take().ok_or_else(allocation)?;
            let event = self.pair(&value, resources).map_err(|_| allocation())?;
            return Ok(StreamEvent::Event(event));
        }
        Ok(StreamEvent::Done)
    }

    /// An EOF-position refusal (`at EOF at line L, column C`).
    fn refused_eof(&mut self, reason: Reason, resources: &ResourceContext<'_>) -> Result<StreamEvent, EngineRunError> {
        let place = reason.placed.then_some(message::JsonPlace {
            line: self.line,
            column: self.column,
            at_eof: true,
        });
        let message = message::json_parse_message_head(reason.text, place.as_ref())?;
        if !self.stream_errors {
            return Ok(StreamEvent::Refused(message));
        }
        let event = self.error_event(&message, resources)?;
        self.reset();
        Ok(StreamEvent::Event(event))
    }

    /// The parser's current 1-based line, for per-event error reports.
    #[must_use]
    pub const fn line(&self) -> u32 {
        self.line
    }
}

fn allocation() -> EngineRunError {
    EngineRunError::allocation_failure()
}

/// The array payload of a `Value::Array` (the parser only stores arrays here).
fn array_of(value: &Value) -> &jqf_data::Array {
    match value.untagged() {
        Value::Array(array) => array,
        _ => panic!("internal contract: event output is an array"),
    }
}

/// One keyword, which has to match exactly — `truefalse` is one token.
fn keyword(token: &[u8], spelling: &[u8], value: Value) -> Result<Value, Reason> {
    if token == spelling {
        Ok(value)
    } else {
        Err(KEYWORD_LITERAL)
    }
}

/// One number token under the strict envelope: the RFC 8259 grammar the codec's own acceptor enforces. The lenient
/// number acceptance is looser — the strict-only divergence — so `01`, `+1`, `.5`, `1.`, `nan`, `inf` and a
/// NUL-truncated token are refused here with the reference's own message text.
fn strict_number(token: &[u8]) -> Result<Value, Failure> {
    // The token bytes must be valid UTF-8 text before the codec acceptor sees them (a raw `\xff` in a token is invalid
    // both ways).
    let Ok(spelling) = core::str::from_utf8(token) else {
        return Err(NUMERIC_LITERAL.into());
    };
    if !is_strict_json_number(spelling) {
        return Err(NUMERIC_LITERAL.into());
    }
    match Number::try_json_literal(spelling) {
        Ok(number) => Ok(Value::Number(number)),
        // An allocation inside the numeric storage is the machine allocation class, never a parse refusal (the stream
        // doc's own law).
        Err(jqf_data::NumericError::Allocation) => Err(Failure::Allocation),
        Err(_) => Err(NUMERIC_LITERAL.into()),
    }
}

/// RFC 8259's number grammar: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
/// The whole token must be consumed; `01`, `+1`, `.5`, `1.`, `1e` and friends are refused here (the lenient number
/// reader accepts them — the strict-only divergence).
fn is_strict_json_number(spelling: &str) -> bool {
    let bytes = spelling.as_bytes();
    let mut index = 0;
    if bytes.get(index) == Some(&b'-') {
        index += 1;
    }
    // The integer part: `0` or a nonzero digit followed by digits.
    match bytes.get(index) {
        Some(b'0') => index += 1,
        Some(b'1'..=b'9') => {
            index += 1;
            while matches!(bytes.get(index), Some(b'0'..=b'9')) {
                index += 1;
            }
        }
        _ => return false,
    }
    // The fraction part: `.` with at least one digit.
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let digits_start = index;
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
        if index == digits_start {
            return false;
        }
    }
    // The exponent part: `e`/`E`, an optional sign, at least one digit.
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let digits_start = index;
        while matches!(bytes.get(index), Some(b'0'..=b'9')) {
            index += 1;
        }
        if index == digits_start {
            return false;
        }
    }
    index == bytes.len()
}

/// `classify`: which byte class a byte outside a string belongs to.
#[derive(Clone, Copy, PartialEq)]
enum Class {
    Literal,
    Whitespace,
    Quote,
    Structure,
}

fn classify(byte: u8) -> Class {
    match byte {
        b' ' | b'\t' | b'\r' | b'\n' => Class::Whitespace,
        b'"' => Class::Quote,
        b'[' | b',' | b']' | b'{' | b':' | b'}' => Class::Structure,
        _ => Class::Literal,
    }
}

fn index_value(index: i64) -> Value {
    Value::Number(Number::integer(Integer::from_i64(index)))
}

/// The expected byte length of a UTF-8 sequence from its lead byte; `None` for continuation and invalid bytes.
fn utf8_coding_length(byte: u8) -> Option<u8> {
    match byte {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

/// Decodes one strict string body (bytes without the quotes) into its value:
/// the escape decoding with the control-character refusal, plus the strict-envelope UTF-8 validation folded into the
/// same walk.
///
/// The strict UTF-8 validation accepts only well-formed sequences — no continuation-out-of-place, no overlong
/// encodings, no UTF-8-encoded surrogates, nothing past U+10FFFF — which is RFC 8259's law and what the codec's
/// validate pass enforces.
fn decode_string(body: &[u8], _resources: &ResourceContext<'_>) -> Result<Value, Failure> {
    if body.iter().all(|byte| *byte != b'\\' && *byte >= 0x20) {
        // One UTF-8 check: `from_utf8` is the strict envelope. The prior custom walk plus a second `from_utf8` said the
        // same thing twice.
        let text = core::str::from_utf8(body).map_err(|_| INVALID_UTF8)?;
        return Ok(Value::try_string(text)?);
    }
    let mut out = alloc::string::String::new();
    out.try_reserve(body.len()).map_err(|_| Failure::Allocation)?;
    let mut index = 0;
    while index < body.len() {
        let byte = body[index];
        if byte == b'\\' {
            let (character, consumed) = escape(&body[index..])?;
            out.try_reserve(character.len_utf8()).map_err(|_| Failure::Allocation)?;
            out.push(character);
            index += consumed;
            continue;
        }
        if byte < 0x20 {
            return Err(CONTROL_CHARACTERS.into());
        }
        let start = index;
        index += 1;
        while index < body.len() && body[index] != b'\\' && body[index] >= 0x20 {
            index += 1;
        }
        let text = core::str::from_utf8(&body[start..index]).map_err(|_| INVALID_UTF8)?;
        out.try_reserve(text.len()).map_err(|_| Failure::Allocation)?;
        out.push_str(text);
    }
    Ok(Value::try_string(&out)?)
}

/// Decodes one escape from `tail` (which starts at the backslash).
///
/// The law itself is [`json_escape`]'s — this wrapper only maps a fault onto this parser's reason type.
fn escape(tail: &[u8]) -> Result<(char, usize), Reason> {
    json_escape::parse_escape(tail).map_err(|fault| match fault {
        EscapeFault::AtEnd => ESCAPE_AT_END,
        EscapeFault::Invalid => INVALID_ESCAPE,
        EscapeFault::ShortUnicode => SHORT_UNICODE_ESCAPE,
        EscapeFault::BadDigits => UNICODE_ESCAPE_DIGITS,
        EscapeFault::SurrogatePair => SURROGATE_PAIR,
    })
}

#[cfg(test)]
mod tests {
    use super::{EventParser, StreamEvent};
    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    use crate::semantics::render::to_json;

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger: every string and container the parser builds is charged at its own construction,
    /// so nothing parses without an account.
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    /// The compact JSON of the parser's full stream over `input`. A `--stream` refusal is rendered `ERR: <message>`;
    /// under `--stream-errors` the refusal is an event like any other.
    fn collect(input: &str, stream_errors: bool) -> Vec<String> {
        let resources = ledger();
        let mut parser = EventParser::new(input.as_bytes(), stream_errors);
        let mut out = Vec::new();
        loop {
            match parser.next_event(&resources).expect("poll") {
                StreamEvent::Event(value) => out.push(to_json(&value).expect("renders")),
                StreamEvent::Done => break,
                StreamEvent::Refused(message) => {
                    out.push(format!("ERR: {message}"));
                    break;
                }
                // The whole-input shape never needs more input.
                StreamEvent::NeedMore => unreachable!("whole-input parser never needs more"),
            }
        }
        out
    }

    fn event(input: &str) -> Vec<String> {
        collect(input, false)
    }

    #[test]
    fn compact_stream_shapes_match_the_stream_law() {
        // The tostream law: leaves and empty containers emit [path, value], a non-empty container emits only [path,
        // last-step].
        assert_eq!(event("[1,2]"), ["[[0],1]", "[[1],2]", "[[1]]"]);
        assert_eq!(event("{}"), ["[[],{}]"]);
        assert_eq!(event("5"), ["[[],5]"]);
        assert_eq!(
            event("{\"a\":1,\"b\":[2,3],\"c\":{\"d\":4}}"),
            [
                "[[\"a\"],1]",
                "[[\"b\",0],2]",
                "[[\"b\",1],3]",
                "[[\"b\",1]]",
                "[[\"c\",\"d\"],4]",
                "[[\"c\",\"d\"]]",
                "[[\"c\"]]",
            ]
        );
        assert_eq!(event("1 2 3"), ["[[],1]", "[[],2]", "[[],3]"]);
        assert_eq!(event("\"hi\""), ["[[],\"hi\"]"]);
        assert_eq!(event("\"héllo\""), ["[[],\"héllo\"]"]);
        assert_eq!(event("\"a\\nb\""), ["[[],\"a\\nb\"]"]);
        assert_eq!(event("[]"), ["[[],[]]"]);
        assert_eq!(event("[1,[]]"), ["[[0],1]", "[[1],[]]", "[[1]]"]);
        assert_eq!(event("null"), ["[[],null]"]);
        assert_eq!(event("true"), ["[[],true]"]);
        assert_eq!(event("false"), ["[[],false]"]);
    }

    #[test]
    fn stream_token_messages_match_the_law() {
        // The stream-token law: each shape refuses with the exact sentence at the exact position (paths in the events).
        assert_eq!(collect("{\"a\":\"b\":1}", false), ["[[\"b\"],1]", "[[\"b\"]]"]);
        assert_eq!(
            collect("{1:2}", true),
            ["[\"Object keys must be strings at line 1, column 3\",[null]]"]
        );
        assert_eq!(
            collect("{\"a\":1:2}", true),
            ["[\"Object keys must be strings at line 1, column 7\",[\"a\"]]"]
        );
        assert_eq!(
            collect("{:1}", true),
            ["[\"Expected string key before ':' at line 1, column 2\",[null]]"]
        );
        assert_eq!(
            collect("[1:2]", true),
            ["[\"':' not as part of an object at line 1, column 3\",[0]]"]
        );
        assert_eq!(
            collect("{\"a\" 1}", true),
            ["[\"Expected separator between values at line 1, column 7\",[null]]"]
        );
        assert_eq!(
            collect("{\"a\",1}", true),
            ["[\"Objects must consist of key:value pairs at line 1, column 5\",[null]]"]
        );
        assert_eq!(
            collect("[1,2 3]", true),
            [
                "[[0],1]",
                "[\"Expected separator between values at line 1, column 7\",[1]]",
            ]
        );
        assert_eq!(
            collect("[[1] 2]", true),
            [
                "[[0,0],1]",
                "[[0,0]]",
                "[\"Expected separator between values at line 1, column 7\",[0]]",
            ]
        );
    }

    #[test]
    fn error_events_carry_messages_and_paths() {
        // Under --stream-errors a refusal is an event: the message text and the parser path at the failure point.
        assert_eq!(
            collect("{bad}", true),
            ["[\"Invalid numeric literal at line 1, column 5\",[null]]"]
        );
        assert_eq!(
            collect("{\"a\":\n", true),
            ["[\"Unfinished JSON term at EOF at line 2, column 0\",[\"a\"]]"]
        );
        assert_eq!(
            collect("notjson", true),
            ["[\"Invalid numeric literal at EOF at line 1, column 7\",[]]"]
        );
        assert_eq!(
            collect("[1,2\n", true),
            ["[[0],1]", "[\"Unfinished JSON term at EOF at line 2, column 0\",[1]]"]
        );
        // Under plain --stream the same refusals are terminal and named ERR.
        assert_eq!(
            collect("notjson", false),
            ["ERR: Invalid numeric literal at EOF at line 1, column 7"]
        );
    }

    #[test]
    fn line_chunk_recovery_matches_the_chunk_law() {
        // One line, one error (the rest of the line is thrown).
        assert_eq!(
            collect("x x x", true),
            ["[\"Invalid numeric literal at line 1, column 2\",[]]"]
        );
        // One error per line; a line's discarded tail is never counted.
        assert_eq!(
            collect("x\nx\nx", true),
            [
                "[\"Invalid numeric literal at line 2, column 0\",[]]",
                "[\"Invalid numeric literal at line 3, column 0\",[]]",
                "[\"Invalid numeric literal at EOF at line 3, column 1\",[]]",
            ]
        );
        // A failing line is thrown; the next line parses.
        assert_eq!(
            collect("{bad}\n{\"c\":2}", true),
            [
                "[\"Invalid numeric literal at line 1, column 5\",[null]]",
                "[[\"c\"],2]",
                "[[\"c\"]]",
            ]
        );
        // `xx yy\nzz`: the first error throws `yy\n` with it, so the second error still reports line 1 (the newline was
        // never counted).
        assert_eq!(
            collect("xx yy\nzz", true),
            [
                "[\"Invalid numeric literal at line 1, column 3\",[]]",
                "[\"Invalid numeric literal at EOF at line 1, column 5\",[]]",
            ]
        );
    }

    #[test]
    fn eof_events_complete_top_level_values() {
        // A top-level value ending at EOF emits its [path, value] via the EOF handling.
        assert_eq!(event("1"), ["[[],1]"]);
        assert_eq!(event("{\"a\":1}"), ["[[\"a\"],1]", "[[\"a\"]]"]);
        assert_eq!(event(""), Vec::<String>::new());
    }

    #[test]
    fn strict_envelope_refuses_lenient_spellings() {
        // The lenient number/literal spellings are refused here with the message format — the catalogued
        // strict-envelope divergence.
        assert_eq!(
            collect("01", true),
            ["[\"Invalid numeric literal at EOF at line 1, column 2\",[]]"]
        );
        assert_eq!(
            collect("nan", true),
            ["[\"Invalid numeric literal at EOF at line 1, column 3\",[]]"]
        );
        assert_eq!(
            collect("+1", true),
            ["[\"Invalid numeric literal at EOF at line 1, column 2\",[]]"]
        );
        // A leading BOM is stripped.
        assert_eq!(event("\u{feff}{\"a\":1}"), ["[[\"a\"],1]", "[[\"a\"]]"]);
    }

    fn collect_incremental(input: &str, stream_errors: bool) -> Vec<String> {
        let resources = ledger();
        let bytes = input.as_bytes();
        let mut parser = EventParser::incremental(stream_errors);
        let mut out = Vec::new();
        let mut start = 0usize;
        let mut fed_last = false;
        loop {
            match parser.next_event(&resources).expect("poll") {
                StreamEvent::Event(value) => out.push(to_json(&value).expect("renders")),
                StreamEvent::Done => break,
                StreamEvent::Refused(message) => {
                    out.push(format!("ERR: {message}"));
                    break;
                }
                StreamEvent::NeedMore => {
                    assert!(!fed_last, "no NeedMore after the final buffer");
                    // The next fgets-shaped chunk: the next line (or the remaining tail at EOF, then an empty final
                    // buffer).
                    let end = if let Some(newline) = bytes[start..].iter().position(|byte| *byte == b'\n') {
                        start + newline + 1
                    } else {
                        bytes.len()
                    };
                    let is_last = end == bytes.len();
                    parser.set_buf(&bytes[start..end], is_last);
                    start = end;
                    fed_last = is_last;
                }
            }
        }
        out
    }

    #[test]
    fn incremental_chunks_match_the_whole_input_shape() {
        let cases = [
            "[1,2]",
            "{\"a\":1,\"b\":[2,3],\"c\":{\"d\":4}}",
            "1 2 3",
            "x x x",
            "x\nx\nx",
            "xx yy\nzz",
            "{\"a\":1\n}",
            "\u{feff}[1,2]",
            "{\"a\":\n\"b\"}",
            "{\"a\":1,\"b\":[2,3]}\n{\"c\":4}",
            "",
            "\u{feff}",
        ];
        for case in cases {
            let whole = collect(case, false);
            let whole_errors = collect(case, true);
            assert_eq!(
                collect_incremental(case, false),
                whole,
                "incremental != whole for {case:?}"
            );
            assert_eq!(
                collect_incremental(case, true),
                whole_errors,
                "incremental-errors != whole for {case:?}"
            );
        }
    }

    /// A 3-byte UTF-8 sequence whose codepoint sits at/above U+5000 decodes:
    /// the pre-shift accumulation made every lead >= 0xE5 compute a value past U+10FFFF and refuse — most of the CJK
    /// and Hangul ranges — while 4-byte leads 0xF1+ wrapped silently and passed by luck.
    #[test]
    fn three_byte_sequences_at_and_above_u5000_are_valid() {
        for (spelling, expected) in [
            ("\u{0800}", "ࠀ"), // U+0800: the 3-byte floor
            ("\u{4fff}", "俿"),
            ("\u{5000}", "倀"), // U+5000: the old failure boundary
            ("\u{5927}", "大"),
            ("\u{ffff}", "￿"),   // U+FFFF: the 3-byte ceiling
            ("\u{1f600}", "😀"), // 4-byte
        ] {
            assert_eq!(event(&format!("\"{spelling}\"")), vec![format!("[[],\"{expected}\"]")]);
        }
    }

    /// The malformed forms the codepoint bounds still refuse: an overlong 3-byte lead, a UTF-8-encoded surrogate, and a
    /// 4-byte lead past U+10FFFF. Fed as raw bytes, because the byte sequences themselves are not well-formed UTF-8.
    #[test]
    fn utf8_bounds_still_refuse_overlong_surrogate_and_past_max() {
        let resources = ledger();
        for bytes in [
            // 0xE0 0x9F 0xBF: overlong encoding of U+07FF.
            &b"\xe0\x9f\xbf"[..],
            // 0xED 0xA0 0x80: surrogate U+D800.
            &b"\xed\xa0\x80"[..],
            // 0xF4 0x90 0x80 0x80: past U+10FFFF.
            &b"\xf4\x90\x80\x80"[..],
            // 0xC0 0x80: overlong ASCII.
            &b"\xc0\x80"[..],
            // 0xC1 0xBF: the other overlong 2-byte form.
            &b"\xc1\xbf"[..],
        ] {
            let mut input = Vec::new();
            input.push(b'"');
            input.extend_from_slice(bytes);
            input.push(b'"');
            let mut parser = EventParser::new(&input, false);
            let refused = loop {
                match parser.next_event(&resources).expect("poll") {
                    StreamEvent::Refused(message) => break message,
                    StreamEvent::Done => break String::new(),
                    _ => {}
                }
            };
            assert!(
                refused.starts_with("Invalid UTF-8 in string"),
                "{bytes:?} must be refused, got {refused:?}"
            );
        }
    }

    /// A whole input ending with a partial BOM prefix promised three bytes and delivered fewer: the next poll refuses
    /// `Malformed BOM` instead of parsing the stripped bytes as a complete empty document.
    #[test]
    fn a_partial_bom_at_eof_is_refused() {
        let resources = ledger();
        for bytes in [&b"\xef\xbb"[..], &b"\xef"[..]] {
            let mut parser = EventParser::new(bytes, false);
            let refused = loop {
                match parser.next_event(&resources).expect("poll") {
                    StreamEvent::Refused(message) => break message,
                    StreamEvent::Done => break String::new(),
                    _ => {}
                }
            };
            assert_eq!(refused, "Malformed BOM", "{bytes:?}");
        }
    }
}
