//! The validating scan: source bytes to a span skeleton.
//!
//! One straight-line walk turns the whole source into a skeleton of key/value entries (with their authored key and
//! value spans), sections (INI), and comment runs. The skeleton is a VALIDATING scan — every grammar clause is checked
//! here, so a terminal failure names the clause it violated, and materialization cannot fail on grammar.
//!
//! The scan is TERMINAL (no recovery dialect), CORRUPT-LATE (a corrupt byte anywhere in the file fails the whole scan,
//! never a silent prefix), and WORK-CHECKED at the loop head (each logical line admits one work transition, so a long
//! document cannot block a poll past its budget).
//!
//! ## The three grammars share one line walk
//!
//! The differences between `properties`, `ini`, and `dotenv` are clauses on the SAME scanner: whether backslash
//! continuation is a grammar feature, which comment markers open a line, whether sections exist, which separators are
//! legal, and whether the value is cooked or literal. Each grammar's clause list is written in the crate docs; the scan
//! is the executable form of that list.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::byte_scan::{NdjsonFrame, prefix_len};
use jqf_codec_core::{CodecError, CodecFailureKind, CodecRunContext};
use jqf_resource::WorkAdmission;
use jqf_source::{Namespace, ResolvedSource, Span};

use crate::options::Grammar;

/// The diagnostic namespace of one grammar's rejects.
const fn namespace(grammar: Grammar) -> Namespace {
    match grammar {
        Grammar::Properties => Namespace::new("properties"),
        Grammar::Ini => Namespace::new("ini"),
        Grammar::Dotenv => Namespace::new("dotenv"),
    }
}

/// One key/value entry, in source order.
#[derive(Debug)]
pub(crate) struct Entry {
    /// The COOKED key text (escapes resolved under the properties grammar).
    pub(crate) key: String,
    /// The COOKED value text (escapes resolved under the properties grammar; literal under ini and unquoted dotenv).
    pub(crate) value: String,
    /// The authored value bytes: `source[value_span]` slices the exact raw value text as written. Properties spans run
    /// to the logical line's last byte (trailing blanks are value text there); ini and dotenv trim trailing blanks.
    pub(crate) value_span: Span,
    /// The comment lines directly above this entry (leading comments), marker-stripped and trimmed.
    pub(crate) leading_comments: Vec<String>,
    /// The owning section's index into [`Skeleton::sections`]; `None` is the root object (a key line before any
    /// section).
    pub(crate) section: Option<u32>,
}

/// One INI section header, in first-definition order.
#[derive(Debug)]
pub(crate) struct Section {
    /// The exact byte run between the brackets, trimmed of surrounding spaces (clause 1 of the written list).
    pub(crate) name: String,
    /// The authored `[name]` bytes including both brackets.
    pub(crate) header_span: Span,
}

/// The span skeleton of one whole document.
#[derive(Debug)]
pub(crate) struct Skeleton {
    /// Sections in first-definition order (INI only; empty otherwise).
    pub(crate) sections: Vec<Section>,
    /// Entries in source order, each naming its owning section.
    pub(crate) entries: Vec<Entry>,
    /// The comment lines after the LAST entry (the document trailer): the root's foot-comment fact. Pending comments
    /// that never found an owning entry flush here at EOF instead of being dropped.
    pub(crate) trailing_comments: Vec<String>,
    /// The whole source's byte length, so the materializer can bind the ROOT a whole-document authored span (the edit
    /// lane's trailer write anchor).
    pub(crate) source_len: usize,
}

/// Scans one whole source into its span skeleton.
///
/// # Errors
///
/// Returns a terminal decode failure naming the violated grammar clause.
pub(crate) fn scan(
    source: ResolvedSource<'_>,
    grammar: Grammar,
    resources: &mut CodecRunContext<'_, '_>,
) -> Result<Skeleton, CodecError> {
    let bytes = source.bytes();
    // Strict UTF-8, no BOM. One whole-source validation makes the scan corrupt-late by construction — a non-UTF-8 byte
    // anywhere is the same terminal failure.
    if bytes.starts_with(b"\xef\xbb\xbf") {
        return Err(invalid(
            grammar,
            source,
            0,
            "bom",
            "a byte-order mark is not accepted; re-encode to UTF-8 without a BOM",
        ));
    }
    if core::str::from_utf8(bytes).is_err() {
        return Err(invalid(
            grammar,
            source,
            0,
            "invalid-utf8",
            "input is not valid UTF-8; re-encode to UTF-8",
        ));
    }
    let mut scanner = Scanner {
        source,
        bytes,
        grammar,
        line_start: 0,
        pos: 0,
        scratch: Vec::new(),
        origin: Vec::new(),
        line: &[],
        line_base: 0,
        line_borrowed: true,
        pending_comments: Vec::new(),
        sections: Vec::new(),
        entries: Vec::new(),
        current_section: None,
        root_keys: BTreeSet::new(),
        section_names: BTreeSet::new(),
    };
    scanner.run(resources)?;
    Ok(Skeleton {
        sections: scanner.sections,
        entries: scanner.entries,
        // Comments after the last entry never find an owning entry; they are the document trailer, flushed here as the
        // root's foot (the ownership-precedence rule: no next node follows, so the previous owner — the whole document
        // — takes them as its FOOT).
        trailing_comments: core::mem::take(&mut scanner.pending_comments),
        source_len: bytes.len(),
    })
}

struct Scanner<'a> {
    source: ResolvedSource<'a>,
    bytes: &'a [u8],
    grammar: Grammar,
    /// The original offset of the current logical line's first byte (the diagnostic anchor).
    line_start: usize,
    pos: usize,
    /// The current logical line's content, continuations removed.
    scratch: Vec<u8>,
    /// For each scratch index, the ORIGINAL byte offset it came from.
    origin: Vec<usize>,
    /// Borrowed identity line (no properties continuation).
    line: &'a [u8],
    line_base: usize,
    line_borrowed: bool,
    /// Comment texts awaiting their owning entry.
    pending_comments: Vec<String>,
    sections: Vec<Section>,
    entries: Vec<Entry>,
    current_section: Option<u32>,
    /// Root-object keys, for the root-key collision check.
    root_keys: BTreeSet<String>,
    /// Section names, for O(1) duplicate and collision probes.
    section_names: BTreeSet<String>,
}

impl Scanner<'_> {
    fn run(&mut self, resources: &mut CodecRunContext<'_, '_>) -> Result<(), CodecError> {
        loop {
            // Work-checked at the loop head: one admitted transition per logical line.
            let remaining = resources.resources().remaining_work() as usize;
            match resources.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => resources.replenish_work()?,
                WorkAdmission::Granted(_) => {}
            }
            if self.pos >= self.bytes.len() {
                break;
            }
            self.read_logical_line();
            self.process_line()?;
        }
        // A root key after a section is not in this dialect (`current_section` is never cleared). The live collision
        // check is header-vs-`root_keys`.
        Ok(())
    }

    /// Builds one source-aware reject diagnostic anchored at the current logical line.
    fn invalid(&self, code: &'static str, message: &'static str) -> CodecError {
        invalid(self.grammar, self.source, self.line_start, code, message)
    }

    /// The current logical line's bytes (borrowed identity, or joined scratch).
    fn line(&self) -> &[u8] {
        if self.line_borrowed { self.line } else { &self.scratch }
    }

    /// Reads the next logical line into `scratch`/`origin` and advances `pos`. Under the properties grammar a natural
    /// line ending in an ODD run of backslashes immediately before its terminator is continued on the next natural line
    /// (each backslash pair cooks to one literal `\`, per the odd-backslash continuation law), except a comment (`#` or
    /// `!` as first non-blank of that natural line), whose trailing `\` is literal. Ini and dotenv have no
    /// continuations.
    fn read_logical_line(&mut self) {
        self.scratch.clear();
        self.origin.clear();
        self.line_borrowed = false;
        let mut first_line = true;
        let mut continued_any = false;
        loop {
            let (start, content_end, terminator) = read_natural_line(self.bytes, self.pos);
            if first_line {
                self.line_start = start;
            }
            self.pos += terminator + (content_end - start);
            // Comments are classified per natural line, before continuation join. A `#`/`!` on a *continued* value/key
            // line is data.
            let comment = first_line
                && self.grammar == Grammar::Properties
                && properties_comment_natural_line(&self.bytes[start..content_end]);
            first_line = false;
            // Continuation is only on an ODD trailing backslash run: a pair cooks to one literal `\`, so an even run
            // (`a=b\\`) ends the logical line there.
            let trailing_backslashes = self.bytes[start..content_end]
                .iter()
                .rev()
                .take_while(|&&byte| byte == b'\\')
                .count();
            let continued =
                self.grammar == Grammar::Properties && !comment && trailing_backslashes % 2 == 1 && terminator > 0;
            let seg_end = if continued { content_end - 1 } else { content_end };
            if continued {
                continued_any = true;
            }
            if !continued_any && !continued {
                self.line = &self.bytes[start..seg_end];
                self.line_base = start;
                self.line_borrowed = true;
                break;
            }
            for index in start..seg_end {
                self.origin.push(index);
                self.scratch.push(self.bytes[index]);
            }
            if continued {
                continue;
            }
            break;
        }
    }

    fn process_line(&mut self) -> Result<(), CodecError> {
        match self.grammar {
            Grammar::Properties => self.process_properties_line(),
            Grammar::Ini => self.process_ini_line(),
            Grammar::Dotenv => self.process_dotenv_line(),
        }
    }

    /// Skips blank bytes (space, tab, form feed) from `from`, returning the first non-blank scratch index (or
    /// `scratch.len()`).
    fn skip_blank(&self, mut from: usize) -> usize {
        while from < self.line().len() && is_blank(self.line()[from]) {
            from += 1;
        }
        from
    }

    // ------------------------------------------------------------------ properties (`properties.jdk@1`)
    // ------------------------------------------------------------------

    fn process_properties_line(&mut self) -> Result<(), CodecError> {
        let first = self.skip_blank(0);
        if first >= self.line().len() {
            // A blank line is ignored; pending comments survive blanks (a comment run above an entry stays its leading
            // comments).
            return Ok(());
        }
        if matches!(self.line()[first], b'#' | b'!') {
            self.pending_comments.push(comment_text(self.line(), first + 1));
            return Ok(());
        }
        let (key, cursor) = self.cook_properties_key(first)?;
        // Any whitespace after the key is skipped; if the first non-whitespace character after the key is `=` or `:`,
        // it is ignored and the whitespace after it is skipped too.
        let cursor = self.skip_blank(cursor);
        let cursor = if cursor < self.line().len() && matches!(self.line()[cursor], b'=' | b':') {
            self.skip_blank(cursor + 1)
        } else {
            cursor
        };
        // Trailing raw whitespace is VALUE TEXT: only the whitespace between separator and value's first byte is
        // skipped, and cooking runs over the full range so an escaped tail (`v\ ` cooks its space) survives whole. A
        // decoder-side trim here would silently drop every encoded value that ends in a blank and would eat a `\
        // `-escaped final space before it could cook.
        let value = self.cook_properties_value(cursor, self.line().len())?;
        let value_span = self.span(cursor, self.line().len())?;
        self.entries.push(Entry {
            key,
            value,
            value_span,
            leading_comments: core::mem::take(&mut self.pending_comments),
            section: None,
        });
        Ok(())
    }

    /// Cooks a properties key from `first` to the first unescaped separator.
    fn cook_properties_key(&self, first: usize) -> Result<(String, usize), CodecError> {
        let mut key = String::new();
        let mut cursor = first;
        let mut run_start = cursor;
        while cursor < self.line().len() {
            let byte = self.line()[cursor];
            if byte == b'\\' {
                self.push_run(&mut key, run_start, cursor)?;
                cursor += 1;
                if cursor >= self.line().len() {
                    break;
                }
                cook_escape(
                    self.line(),
                    &mut cursor,
                    &mut key,
                    self.grammar,
                    self.source,
                    self.line_start,
                )?;
                run_start = cursor;
                continue;
            }
            if byte == b'=' || byte == b':' || is_blank(byte) {
                self.push_run(&mut key, run_start, cursor)?;
                return Ok((key, cursor));
            }
            cursor += 1;
        }
        self.push_run(&mut key, run_start, cursor)?;
        Ok((key, cursor))
    }

    /// Cooks a properties value over `[start, end)`, dropping a trailing lone backslash at EOF.
    fn cook_properties_value(&self, start: usize, end: usize) -> Result<String, CodecError> {
        let mut value = String::new();
        let mut cursor = start;
        let mut run_start = cursor;
        while cursor < end {
            if self.line()[cursor] == b'\\' {
                self.push_run(&mut value, run_start, cursor)?;
                cursor += 1;
                if cursor >= end {
                    break;
                }
                cook_escape(
                    self.line(),
                    &mut cursor,
                    &mut value,
                    self.grammar,
                    self.source,
                    self.line_start,
                )?;
                run_start = cursor;
            } else {
                cursor += 1;
            }
        }
        self.push_run(&mut value, run_start, cursor)?;
        Ok(value)
    }

    fn push_run(&self, out: &mut String, start: usize, end: usize) -> Result<(), CodecError> {
        push_utf8_run(out, self.line(), start, end, self.grammar, self.source, self.line_start)
    }

    // ------------------------------------------------------------------ ini (the written clause list)
    // ------------------------------------------------------------------

    fn process_ini_line(&mut self) -> Result<(), CodecError> {
        let first = self.skip_blank(0);
        if first >= self.line().len() {
            return Ok(());
        }
        if matches!(self.line()[first], b';' | b'#') {
            self.pending_comments.push(comment_text(self.line(), first + 1));
            return Ok(());
        }
        if self.line()[first] == b'[' {
            // A section header must be ALONE on its line: the closing `]` and nothing after it but blanks.
            let close = self
                .line()
                .iter()
                .position(|byte| *byte == b']')
                .ok_or_else(|| self.invalid("unterminated-section", "an unterminated '[' section header"))?;
            let tail = self.skip_blank(close + 1);
            if tail != self.line().len() {
                return Err(self.invalid("section-trailing-content", "a section header must be alone on its line"));
            }
            let name_start = self.skip_blank(first + 1);
            let mut name_end = close;
            while name_end > name_start && is_blank(self.line()[name_end - 1]) {
                name_end -= 1;
            }
            if name_start >= name_end {
                return Err(self.invalid("empty-section", "a section header must name a non-empty section"));
            }
            let name = core::str::from_utf8(&self.line()[name_start..name_end])
                .map_err(|_| self.invalid("invalid-utf8", "a section name must be valid UTF-8"))?
                .to_owned();
            if self.root_keys.contains(&name) {
                return Err(self.invalid(
                    "key-section-collision",
                    "a root key collides with a section of the same name",
                ));
            }
            if !self.section_names.insert(name.clone()) {
                return Err(self.invalid("duplicate-section", "a duplicate section header is a terminal failure"));
            }
            // The authored header span covers the full `[name]` region in the ORIGINAL source.
            let header_span = self.span(first, close + 1)?;
            let index = u32::try_from(self.sections.len())
                .map_err(|_| self.invalid("too-many-sections", "too many sections"))?;
            self.sections.push(Section { name, header_span });
            self.current_section = Some(index);
            return Ok(());
        }
        // key = value or key: value; the key is the text before the FIRST separator, trimmed; the value is the rest of
        // the LOGICAL line, trimmed. No continuations, no quote processing, no escapes: the value is the literal bytes
        // (clause 3 of the written list).
        let separator = self
            .line()
            .iter()
            .position(|byte| matches!(byte, b'=' | b':'))
            .ok_or_else(|| self.invalid("bare-key", "a bare key with no separator is a terminal failure"))?;
        let key_start = first;
        let key_end = separator;
        let key = core::str::from_utf8(&self.line()[key_start..key_end])
            .map_err(|_| self.invalid("invalid-utf8", "a key must be valid UTF-8"))?
            .trim()
            .to_owned();
        let value_start = self.skip_blank(separator + 1);
        let mut value_end = self.line().len();
        while value_end > value_start && is_blank(self.line()[value_end - 1]) {
            value_end -= 1;
        }
        let value = core::str::from_utf8(&self.line()[value_start..value_end])
            .map_err(|_| self.invalid("invalid-utf8", "a value must be valid UTF-8"))?
            .to_owned();
        let value_span = self.span(value_start, value_end)?;
        if self.current_section.is_none() {
            if self.section_names.contains(&key) {
                return Err(self.invalid(
                    "key-section-collision",
                    "a root key collides with a section of the same name",
                ));
            }
            self.root_keys.insert(key.clone());
        }
        self.entries.push(Entry {
            key,
            value,
            value_span,
            leading_comments: core::mem::take(&mut self.pending_comments),
            section: self.current_section,
        });
        Ok(())
    }

    // ------------------------------------------------------------------ dotenv (the written clause list)
    // ------------------------------------------------------------------

    fn process_dotenv_line(&mut self) -> Result<(), CodecError> {
        let first = self.skip_blank(0);
        if first >= self.line().len() {
            return Ok(());
        }
        if self.line()[first] == b'#' {
            self.pending_comments.push(comment_text(self.line(), first + 1));
            return Ok(());
        }
        // An `export ` prefix is accepted and stripped: the key is the text after it. Canonical encode writes no
        // prefix.
        let mut key_start = first;
        if self.line().len() >= first + 7 && &self.line()[first..first + 7] == b"export " {
            key_start = first + 7;
        }
        // KEY=VALUE: the separator is the FIRST `=`.
        let separator = self
            .line()
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| self.invalid("bare-key", "a line without '=' is not a dotenv pair"))?;
        let key = core::str::from_utf8(&self.line()[key_start..separator])
            .map_err(|_| self.invalid("invalid-utf8", "a key must be valid UTF-8"))?
            .trim()
            .to_owned();
        let value_raw = self.skip_blank(separator + 1);
        let mut value_end = self.line().len();
        while value_end > value_raw && is_blank(self.line()[value_end - 1]) {
            value_end -= 1;
        }
        let value = if value_end > value_raw && self.line()[value_raw] == b'\'' {
            // Single-quoted: literal bytes, no escapes, no interpolation.
            let close = self.line()[value_raw + 1..value_end]
                .iter()
                .position(|byte| *byte == b'\'')
                .map(|offset| value_raw + 1 + offset)
                .ok_or_else(|| self.invalid("unterminated-quote", "an unterminated single-quoted value"))?;
            let content = core::str::from_utf8(&self.line()[value_raw + 1..close])
                .map_err(|_| self.invalid("invalid-utf8", "a value must be valid UTF-8"))?;
            // A single-quoted value must end at the quote.
            if self.skip_blank(close + 1) != value_end {
                return Err(self.invalid("quote-trailing-content", "trailing content after a single-quoted value"));
            }
            content.to_owned()
        } else if value_end > value_raw && self.line()[value_raw] == b'"' {
            // Double-quoted: the standard escape set is processed; NO `$VAR` interpolation (jqf reads files, it does
            // not evaluate environments).
            let close = dotenv_double_close(&self.line()[value_raw + 1..value_end])
                .map(|offset| value_raw + 1 + offset)
                .ok_or_else(|| self.invalid("unterminated-quote", "an unterminated double-quoted value"))?;
            if self.skip_blank(close + 1) != value_end {
                return Err(self.invalid("quote-trailing-content", "trailing content after a double-quoted value"));
            }
            cook_dotenv_double(self.line(), value_raw + 1, close, self.source, self.line_start)?
        } else {
            // Unquoted: literal bytes, no escapes, no interpolation.
            core::str::from_utf8(&self.line()[value_raw..value_end])
                .map_err(|_| self.invalid("invalid-utf8", "a value must be valid UTF-8"))?
                .to_owned()
        };
        let value_span = self.span(value_raw, value_end)?;
        self.entries.push(Entry {
            key,
            value,
            value_span,
            leading_comments: core::mem::take(&mut self.pending_comments),
            section: None,
        });
        Ok(())
    }

    /// Builds the authored span for a scratch range (the original offsets of its first and last bytes; a continuation's
    /// dropped bytes fall inside the span, so `source[span]` is the authored byte run as written).
    fn span(&self, start: usize, end: usize) -> Result<Span, CodecError> {
        if self.line_borrowed {
            let at = self.line_base + start;
            let stop = self.line_base + end;
            return Span::try_from_usize(at, stop)
                .map_err(|_| self.invalid("span-overflow", "a source span exceeds the addressable range"));
        }
        if start >= end {
            // An empty authored region (an empty key or value) is an empty span at the position where it would have
            // started — after the last copied origin byte, not at source EOF (a later entry would otherwise park this
            // span on the file's last byte).
            let at = self.origin.last().map_or(self.line_start, |index| index + 1);
            return Span::try_from_usize(at, at)
                .map_err(|_| self.invalid("span-overflow", "a source span exceeds the addressable range"));
        }
        let start = self.origin.get(start).copied().unwrap_or(self.bytes.len());
        let end = self.origin.get(end - 1).map_or(start, |index| index + 1);
        Span::try_from_usize(start, end)
            .map_err(|_| self.invalid("span-overflow", "a source span exceeds the addressable range"))
    }
}

/// Reads one natural line: `(start, content_end, terminator_len)` where `content_end` is one past the last content byte
/// and `terminator_len` is 0 (EOF), 1 (`\n` or `\r`), or 2 (`\r\n`). The caller advances `pos`.
fn read_natural_line(bytes: &[u8], pos: usize) -> (usize, usize, usize) {
    let start = pos;
    let run = prefix_len::<NdjsonFrame>(&bytes[pos..]);
    let cursor = pos + run;
    if cursor >= bytes.len() {
        return (start, bytes.len(), 0);
    }
    match bytes[cursor] {
        b'\n' => (start, cursor, 1),
        b'\r' => {
            let terminator = if cursor + 1 < bytes.len() && bytes[cursor + 1] == b'\n' {
                2
            } else {
                1
            };
            (start, cursor, terminator)
        }
        _ => (start, bytes.len(), 0),
    }
}

/// Whether a byte counts as blank (space, tab, form feed).
const fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | 0x0c)
}

/// A properties comment is a natural line whose first non-blank byte is `#` or `!`. A trailing `\` on such a line is
/// not a continuation.
fn properties_comment_natural_line(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index < bytes.len() && is_blank(bytes[index]) {
        index += 1;
    }
    matches!(bytes.get(index), Some(b'#' | b'!'))
}

/// One comment line's text: everything after the marker, trimmed at both ends (`strip_prefix + trim`).
fn comment_text(scratch: &[u8], from: usize) -> String {
    let text = core::str::from_utf8(&scratch[from..]).unwrap_or_default();
    text.trim().to_owned()
}

/// Builds one source-aware `InvalidInput` reject in the grammar's namespace, anchored at `offset`.
fn invalid(
    grammar: Grammar,
    source: ResolvedSource<'_>,
    offset: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    let end = offset.saturating_add(usize::from(offset < source.bytes().len()));
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        namespace(grammar),
        source,
        offset,
        end,
        code,
        message,
    )
}

/// Cooks ONE escape at `cursor` (which sits on the byte after the `\`), appending the cooked character and advancing
/// `cursor` past the escape.
fn cook_escape(
    scratch: &[u8],
    cursor: &mut usize,
    out: &mut String,
    grammar: Grammar,
    source: ResolvedSource<'_>,
    line_start: usize,
) -> Result<(), CodecError> {
    let byte = scratch[*cursor];
    match byte {
        b'u' => {
            // \uXXXX with surrogate pairing; the JSON surrogate law is adopted (a lone low decodes to U+FFFD; a high
            // not followed by a valid low raises).
            *cursor += 1;
            if *cursor + 4 > scratch.len() {
                return Err(invalid(
                    grammar,
                    source,
                    line_start,
                    "malformed-unicode-escape",
                    "a malformed \\uxxxx escape (fewer than four hex digits)",
                ));
            }
            let mut value = 0u32;
            for _ in 0..4 {
                let digit = hex(scratch[*cursor]).ok_or_else(|| {
                    invalid(
                        grammar,
                        source,
                        line_start,
                        "malformed-unicode-escape",
                        "a malformed \\uxxxx escape (non-hex digit)",
                    )
                })?;
                value = (value << 4) | u32::from(digit);
                *cursor += 1;
            }
            if (0xd800..=0xdbff).contains(&value) {
                // A high surrogate must be followed by a valid low escape.
                if *cursor + 6 <= scratch.len() && scratch[*cursor] == b'\\' && scratch[*cursor + 1] == b'u' {
                    let mut low = 0u32;
                    let mut valid = true;
                    for index in 0..4 {
                        let Some(digit) = hex(scratch[*cursor + 2 + index]) else {
                            valid = false;
                            break;
                        };
                        low = (low << 4) | u32::from(digit);
                    }
                    if valid && (0xdc00..=0xdfff).contains(&low) {
                        let code = 0x1_0000 + ((value - 0xd800) << 10) + (low - 0xdc00);
                        out.push(char::from_u32(code).expect("a valid surrogate pair"));
                        *cursor += 6;
                        return Ok(());
                    }
                }
                return Err(invalid(
                    grammar,
                    source,
                    line_start,
                    "missing-low-surrogate",
                    "a high surrogate escape must be followed by a low surrogate escape",
                ));
            }
            if (0xdc00..=0xdfff).contains(&value) {
                out.push('\u{fffd}');
                return Ok(());
            }
            let ch = char::from_u32(value).expect("a validated scalar escape");
            out.push(ch);
            Ok(())
        }
        b't' => {
            out.push('\t');
            *cursor += 1;
            Ok(())
        }
        b'n' => {
            out.push('\n');
            *cursor += 1;
            Ok(())
        }
        b'r' => {
            out.push('\r');
            *cursor += 1;
            Ok(())
        }
        b'f' => {
            out.push('\u{000c}');
            *cursor += 1;
            Ok(())
        }
        other => {
            // \= \: \ \\ \# \! and any other escaped byte are literal (this is also how a separator or comment marker
            // is kept out of grammar position). The `grammar` argument is unused today — the escape set is
            // properties-only — but the call sites pass the grammar so a future dialect's escape difference has one
            // choke point.
            let _ = other;
            let _ = grammar;
            push_utf8_scalar(out, scratch, cursor, grammar, source, line_start)
        }
    }
}

/// Cooks a double-quoted dotenv value's interior (the standard escape set: `\n \r \t \\ \" \$`; any other `\x` is a
/// literal `x`).
fn cook_dotenv_double(
    scratch: &[u8],
    start: usize,
    end: usize,
    source: ResolvedSource<'_>,
    line_start: usize,
) -> Result<String, CodecError> {
    let mut out = String::new();
    let mut cursor = start;
    let mut run_start = cursor;
    while cursor < end {
        let byte = scratch[cursor];
        if byte == b'\\' {
            push_utf8_run(
                &mut out,
                scratch,
                run_start,
                cursor,
                Grammar::Dotenv,
                source,
                line_start,
            )?;
            cursor += 1;
            if cursor >= end {
                return Err(invalid(
                    Grammar::Dotenv,
                    source,
                    line_start,
                    "lone-backslash",
                    "a value ending in a lone backslash",
                ));
            }
            match scratch[cursor] {
                b'n' => {
                    out.push('\n');
                    cursor += 1;
                }
                b'r' => {
                    out.push('\r');
                    cursor += 1;
                }
                b't' => {
                    out.push('\t');
                    cursor += 1;
                }
                b'\\' => {
                    out.push('\\');
                    cursor += 1;
                }
                b'"' => {
                    out.push('"');
                    cursor += 1;
                }
                b'$' => {
                    out.push('$');
                    cursor += 1;
                }
                _ => push_utf8_scalar(&mut out, scratch, &mut cursor, Grammar::Dotenv, source, line_start)?,
            }
            run_start = cursor;
        } else {
            cursor += 1;
        }
    }
    push_utf8_run(
        &mut out,
        scratch,
        run_start,
        cursor,
        Grammar::Dotenv,
        source,
        line_start,
    )?;
    Ok(out)
}

/// The closing `"` of a double-quoted dotenv value, skipping `\"`.
fn dotenv_double_close(bytes: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = index.saturating_add(2),
            b'"' => return Some(index),
            _ => index += 1,
        }
    }
    None
}

/// Copies one UTF-8 run between ASCII specials. Continuation bytes are never ASCII, so a run that starts and ends on an
/// ASCII boundary of a file already validated as UTF-8 is a valid scalar sequence.
fn push_utf8_run(
    out: &mut String,
    bytes: &[u8],
    start: usize,
    end: usize,
    grammar: Grammar,
    source: ResolvedSource<'_>,
    line_start: usize,
) -> Result<(), CodecError> {
    if start >= end {
        return Ok(());
    }
    let text = core::str::from_utf8(&bytes[start..end]).map_err(|_| {
        invalid(
            grammar,
            source,
            line_start,
            "invalid-utf8",
            "a key or value must be valid UTF-8",
        )
    })?;
    out.push_str(text);
    Ok(())
}

/// Decodes one UTF-8 scalar at `cursor` and advances past it. The whole source was validated as UTF-8 up front, so the
/// decode bounds itself by the lead byte's scalar length instead of re-validating the entire remaining line tail (an
/// escape-heavy line would pay O(k·len) for k escapes).
fn push_utf8_scalar(
    out: &mut String,
    bytes: &[u8],
    cursor: &mut usize,
    grammar: Grammar,
    source: ResolvedSource<'_>,
    line_start: usize,
) -> Result<(), CodecError> {
    let rest = bytes.get(*cursor..).unwrap_or(&[]);
    // The lead byte's family fixes the scalar's byte width (0xC0/0xC1 and >= 0xF5 are never legal leads).
    let width = match rest.first() {
        None => 0,
        Some(&lead) => match lead {
            0x00..=0x7F => 1,
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => {
                return Err(invalid(
                    grammar,
                    source,
                    line_start,
                    "invalid-utf8",
                    "a key or value must be valid UTF-8",
                ));
            }
        },
    };
    let ch = rest
        .get(..width)
        .and_then(|window| core::str::from_utf8(window).ok())
        .and_then(|text| text.chars().next())
        .ok_or_else(|| {
            invalid(
                grammar,
                source,
                line_start,
                "invalid-utf8",
                "a key or value must be valid UTF-8",
            )
        })?;
    out.push(ch);
    *cursor += ch.len_utf8();
    Ok(())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Grammar;

    fn test_source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(0), jqf_source::SourceKind::Input),
            "test",
            bytes,
            0,
        )
    }

    fn scan_ok(bytes: &[u8], grammar: Grammar) -> Skeleton {
        let mut resources = test_resources();
        let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4096);
        scan(test_source(bytes), grammar, &mut context).expect("scan")
    }

    fn test_resources() -> jqf_resource::ResourceContext<'static> {
        static CONTROL: jqf_resource::ContinueControl = jqf_resource::ContinueControl;
        jqf_resource::ResourceContext::new(
            jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u64::MAX,
                u32::MAX,
            ))
            .expect("account"),
            &CONTROL,
            jqf_resource::WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn scan_err(bytes: &[u8], grammar: Grammar) {
        let mut resources = test_resources();
        let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4096);
        assert!(
            scan(test_source(bytes), grammar, &mut context).is_err(),
            "expected a terminal failure for {bytes:?}"
        );
    }

    #[test]
    fn properties_corpus_clauses() {
        // clause: a `=` separator.
        let skeleton = scan_ok(b"a=b\n", Grammar::Properties);
        assert_eq!(skeleton.entries.len(), 1);
        assert_eq!(skeleton.entries[0].key, "a");
        assert_eq!(skeleton.entries[0].value, "b");
        // clause: a `:` separator.
        let skeleton = scan_ok(b"a:b\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].key, "a");
        assert_eq!(skeleton.entries[0].value, "b");
        // clause: a whitespace separator (space and tab).
        let skeleton = scan_ok(b"a b\nc\tb\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].key, "a");
        assert_eq!(skeleton.entries[0].value, "b");
        assert_eq!(skeleton.entries[1].key, "c");
        assert_eq!(skeleton.entries[1].value, "b");
        // clause: backslash continuation joins the next natural line.
        let skeleton = scan_ok(b"a=b\\\nc\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "bc");
        // clause: leading whitespace before the key is skipped.
        let skeleton = scan_ok(b"  a=b\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].key, "a");
        // clause: whitespace after the key is skipped, separator optional.
        let skeleton = scan_ok(b"a   b\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].key, "a");
        assert_eq!(skeleton.entries[0].value, "b");
        // clause: an escaped separator is a key character.
        let skeleton = scan_ok(b"a\\=b=1\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].key, "a=b");
        assert_eq!(skeleton.entries[0].value, "1");
        // clause: an escaped space is a key character.
        let skeleton = scan_ok(b"a\\ b=1\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].key, "a b");
        // clause: \uXXXX is cooked in the value.
        let skeleton = scan_ok(b"a=\\u0042\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "B");
        // clause: \t \n \r \f escapes.
        let skeleton = scan_ok(b"a=b\\tc\\nd\\re\\ff\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "b\tc\nd\re\u{000c}f");
        // clause: \\ is a literal backslash.
        let skeleton = scan_ok(b"a=b\\\\c\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "b\\c");
        // clause: a surrogate pair cooks to the supplementary character.
        let skeleton = scan_ok(b"a=\\uD83D\\uDE00\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "\u{1f600}");
        // clause: a lone low surrogate decodes to U+FFFD.
        let skeleton = scan_ok(b"a=\\uDC00\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "\u{fffd}");
        // clause: an empty key.
        let skeleton = scan_ok(b"=value\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].key, "");
        assert_eq!(skeleton.entries[0].value, "value");
        // clause: an empty value.
        let skeleton = scan_ok(b"key=\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].key, "key");
        assert_eq!(skeleton.entries[0].value, "");
        // clause: trailing raw whitespace is value text (only the whitespace after the separator is skipped).
        let skeleton = scan_ok(b"a=b   \n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "b   ");
        // clause: an escaped tail cooks before anything could drop it — the backslash-space is a literal space, not a
        // continuation and not a lone-backslash drop.
        let skeleton = scan_ok(b"a=v\\ \n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "v ");
        // clause: a `\u0020` tail cooks to the blank.
        let skeleton = scan_ok(b"a=b\\u0020\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].value, "b ");
    }

    #[test]
    fn properties_terminal_failures() {
        // clause: a malformed \u escape is a terminal failure.
        scan_err(b"a=\\u00\n", Grammar::Properties);
        // clause: a high surrogate not followed by a low one raises.
        scan_err(b"a=\\uD800x\n", Grammar::Properties);
        // clause: non-UTF-8 bytes fail terminally.
        scan_err(b"a=\xff\n", Grammar::Properties);
        // clause: a BOM is refused.
        scan_err(b"\xef\xbb\xbfa=b\n", Grammar::Properties);
    }

    #[test]
    fn comments_attach_to_the_following_entry() {
        let skeleton = scan_ok(b"# hello\n! world\na=b\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].leading_comments, ["hello", "world"]);
        // A blank line between the comment run and the entry keeps the run.
        let skeleton = scan_ok(b"# hello\n\na=b\n", Grammar::Properties);
        assert_eq!(skeleton.entries[0].leading_comments, ["hello"]);
    }

    #[test]
    fn properties_comment_trailing_backslash_does_not_continue() {
        // A comment is one natural line. A trailing `\` is literal, so the next key is not swallowed into the comment.
        // See also the corpus `properties_grammar_corpus_is_green` rows for the same clause.
        let hash = scan_ok(b"# comment \\\nkey=value\n", Grammar::Properties);
        assert_eq!(hash.entries.len(), 1);
        assert_eq!(hash.entries[0].key, "key");
        assert_eq!(hash.entries[0].value, "value");
        assert_eq!(hash.entries[0].leading_comments, ["comment \\"]);

        let bang = scan_ok(b"! comment \\\nkey=value\n", Grammar::Properties);
        assert_eq!(bang.entries.len(), 1);
        assert_eq!(bang.entries[0].key, "key");
        assert_eq!(bang.entries[0].value, "value");
        assert_eq!(bang.entries[0].leading_comments, ["comment \\"]);

        let indented = scan_ok(b"  # comment \\\nkey=value\n", Grammar::Properties);
        assert_eq!(indented.entries.len(), 1);
        assert_eq!(indented.entries[0].key, "key");

        // A real continued entry still joins.
        let continued = scan_ok(b"key=val\\\nue\n", Grammar::Properties);
        assert_eq!(continued.entries.len(), 1);
        assert_eq!(continued.entries[0].key, "key");
        assert_eq!(continued.entries[0].value, "value");

        // A `#` on a continuation of a real entry is value text, not a comment.
        let hash_in_value = scan_ok(b"key=val\\\n#tail\n", Grammar::Properties);
        assert_eq!(hash_in_value.entries.len(), 1);
        assert_eq!(hash_in_value.entries[0].value, "val#tail");
    }

    #[test]
    fn ini_clause_list() {
        // clause: [section] header; key = value; sections nest one level.
        let skeleton = scan_ok(b"[a]\nx = 1\ny=2\n", Grammar::Ini);
        assert_eq!(skeleton.sections.len(), 1);
        assert_eq!(skeleton.sections[0].name, "a");
        assert_eq!(skeleton.entries.len(), 2);
        assert_eq!(skeleton.entries[0].section, Some(0));
        assert_eq!(skeleton.entries[0].key, "x");
        assert_eq!(skeleton.entries[0].value, "1");
        // clause: a key line before any section belongs to the root object.
        let skeleton = scan_ok(b"root = 1\n[a]\nx = 2\n", Grammar::Ini);
        assert_eq!(skeleton.entries[0].section, None);
        // clause: `;` and `#` at the first non-blank byte are comments.
        let skeleton = scan_ok(b"; c1\n# c2\na = b\n", Grammar::Ini);
        assert_eq!(skeleton.entries[0].leading_comments, ["c1", "c2"]);
        // clause: an inline `;` after a value is VALUE TEXT.
        let skeleton = scan_ok(b"a = b ; not a comment\n", Grammar::Ini);
        assert_eq!(skeleton.entries[0].value, "b ; not a comment");
        // clause: the section name is the run between brackets, trimmed.
        let skeleton = scan_ok(b"[ my.sec ]\nx = 1\n", Grammar::Ini);
        assert_eq!(skeleton.sections[0].name, "my.sec");
        // clause: a bare key with no separator is a terminal failure.
        scan_err(b"bare\n", Grammar::Ini);
        // clause: an unterminated `[` is a terminal failure.
        scan_err(b"[abc\n", Grammar::Ini);
        // clause: a duplicate section header is a terminal failure.
        scan_err(b"[a]\nx=1\n[a]\n", Grammar::Ini);
        // clause: a root key colliding with a section name is a failure.
        scan_err(b"x=1\n[x]\n", Grammar::Ini);
        // clause: an empty section header is a terminal failure.
        scan_err(b"[]\n", Grammar::Ini);
        scan_err(b"[  ]\n", Grammar::Ini);
    }

    #[test]
    fn dotenv_clause_list() {
        // clause: single-quoted values are literal.
        let skeleton = scan_ok(b"a='b\\nc'\n", Grammar::Dotenv);
        assert_eq!(skeleton.entries[0].value, "b\\nc");
        // clause: double-quoted values take the standard escape set.
        let skeleton = scan_ok(b"a=\"b\\nc\\t\\\\\"\n", Grammar::Dotenv);
        assert_eq!(skeleton.entries[0].value, "b\nc\t\\");
        // clause: unquoted values are literal (no interpolation).
        let skeleton = scan_ok(b"a=b$c\n", Grammar::Dotenv);
        assert_eq!(skeleton.entries[0].value, "b$c");
        // clause: an `export ` prefix is accepted and stripped.
        let skeleton = scan_ok(b"export A=1\n", Grammar::Dotenv);
        assert_eq!(skeleton.entries[0].key, "A");
        // clause: `#` at the first non-blank byte is a comment.
        let skeleton = scan_ok(b"# hi\na=1\n", Grammar::Dotenv);
        assert_eq!(skeleton.entries[0].leading_comments, ["hi"]);
        // clause: a line without `=` is a terminal failure.
        scan_err(b"bare\n", Grammar::Dotenv);
        // clause: an unterminated quote is a terminal failure.
        scan_err(b"a='unterminated\n", Grammar::Dotenv);
        scan_err(b"a=\"unterminated\n", Grammar::Dotenv);
        // clause: double-quoted $VAR is literal (see also the corpus row).
        let skeleton = scan_ok(b"A=\"$HOME\"\n", Grammar::Dotenv);
        assert_eq!(skeleton.entries[0].value, "$HOME");
    }

    #[test]
    fn non_ascii_decodes_as_utf8_across_the_three_grammars() {
        let properties = scan_ok("a=café\nclé=x\na2=中\n".as_bytes(), Grammar::Properties);
        assert_eq!(properties.entries[0].value, "café");
        assert_eq!(properties.entries[1].key, "clé");
        assert_eq!(properties.entries[2].value, "中");

        let ini = scan_ok("a=café\nclé=x\n".as_bytes(), Grammar::Ini);
        assert_eq!(ini.entries[0].value, "café");
        assert_eq!(ini.entries[1].key, "clé");

        let double = scan_ok("A=\"café\"\n".as_bytes(), Grammar::Dotenv);
        let single = scan_ok("A='café'\n".as_bytes(), Grammar::Dotenv);
        assert_eq!(double.entries[0].value, "café");
        assert_eq!(double.entries[0].value, single.entries[0].value);

        let emoji = scan_ok("a=😀\n".as_bytes(), Grammar::Properties);
        assert_eq!(emoji.entries[0].value, "😀");
        let escaped_quote = scan_ok(b"A=\"a\\\"b\"\n", Grammar::Dotenv);
        assert_eq!(escaped_quote.entries[0].value, "a\"b");
    }

    #[test]
    fn user_facing_ini_diagnostics_name_the_clause() {
        for (bytes, needle) in [
            (b"x=1\n[x]\n" as &[u8], "collides"),
            (b"[a] leftover\n", "alone on its line"),
            (b"[a]\nx=1\n[a]\n", "duplicate section"),
            (b"bare\n", "bare key"),
        ] {
            let mut resources = test_resources();
            let mut context = jqf_codec_core::CodecRunContext::new(&mut resources);
            context.set_cooperative_credits(4096);
            let error = scan(test_source(bytes), Grammar::Ini, &mut context).expect_err("terminal");
            let message = error.diagnostic().expect("typed").message();
            assert!(message.contains(needle), "{message}");
        }
    }

    #[test]
    fn spans_slice_the_authored_bytes() {
        // Scan-only sibling of `every_value_nodes_span_slices_the_source_to_its_authored_bytes` in tests/corpus.rs
        // (that one drives the full decode path).
        let bytes = b"alpha = one two\nbeta=three\\\nfour\n";
        let skeleton = scan_ok(bytes, Grammar::Properties);
        let first = &skeleton.entries[0];
        assert_eq!(
            &bytes[first.value_span.start() as usize..first.value_span.end() as usize],
            b"one two"
        );
        let second = &skeleton.entries[1];
        assert_eq!(
            &bytes[second.value_span.start() as usize..second.value_span.end() as usize],
            b"three\\\nfour"
        );
    }

    #[test]
    fn a_continued_empty_value_span_sits_after_the_separator_not_at_eof() {
        // `k=\\n\\nother=1\\n`: the first value is empty on a joined line. Its span must be the hole after `=`, not
        // `[source_len, source_len)`.
        let bytes = b"k=\\\n\nother=1\n";
        let skeleton = scan_ok(bytes, Grammar::Properties);
        assert_eq!(skeleton.entries.len(), 2);
        assert_eq!(skeleton.entries[0].key, "k");
        assert_eq!(skeleton.entries[0].value, "");
        let span = skeleton.entries[0].value_span;
        assert_eq!(span.start(), span.end());
        assert_ne!(span.start() as usize, bytes.len());
        assert!((span.start() as usize) < bytes.len());
    }
}
