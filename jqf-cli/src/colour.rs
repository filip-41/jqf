//! Colour rendering of JSON-family output bytes.
//!
//! Colour is a rendering of bytes the encoder already decided. Off, the sink writes those bytes verbatim. On, the only
//! added bytes are ANSI SGR spans around JSON tokens; whitespace, json-seq RS framing, and the facade suffix stay
//! untouched. `--edit` / `--diff` / `--in-place` never colour. Non-JSON-family formats never colour.
//!
//! # Decision
//!
//! Default: colour iff the destination is a TTY and `NO_COLOR` is unset or empty. `-C` forces colour on, even under a
//! non-empty `NO_COLOR`. `-M` forces colour off and is applied last (`-C -M -C` and `-M -C -M` are both monochrome).
//!
//! # Palette
//!
//! `JQ_COLORS` is eight `:`-separated fields: null, false, true, numbers, strings, arrays, objects, object keys. Each
//! field is a run of `[0-9;]` wrapped as `ESC[<field>m`. The reset after every span is `ESC[0m`. A bad character in
//! fields 1..7 falls the whole palette back to the defaults and prints `Failed to set $JQ_COLORS` once, even when
//! colour never engages. Garbage in the 8th field is truncated to its `[0-9;]` prefix. An empty field is `ESC[m`. A
//! trailing `:` ends the field count. Fewer than 8 fields default the rest; fields beyond 8 are ignored. Unset or empty
//! `JQ_COLORS` means all defaults.
//!
//! # Spans
//!
//! Every value is one span in its kind's colour. `{`/`}` are object-coloured and `[`/`]` array-coloured, each its own
//! span except an empty `{}`/`[]`, which is one span over both characters. `,` takes the enclosing container's colour.
//! `:` takes the object colour. A string directly inside an object after `{` or `,` is a key. Whitespace and framing
//! are never coloured.

use std::ffi::OsStr;

use crate::args::CliFormat;

/// The colour switch as parsed from `-C`/`-M`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ColourRequest {
    /// Neither switch: colour iff the destination is a TTY and `NO_COLOR` is unset or empty.
    Auto,
    /// `-C`: force colour on, even under `NO_COLOR`.
    ForceOn,
    /// `-M`: force colour off. Applied last; see the module decision law.
    ForceOff,
}

/// The palette slot indices, in `JQ_COLORS` field order.
pub(crate) const SLOT_NULL: usize = 0;
pub(crate) const SLOT_FALSE: usize = 1;
pub(crate) const SLOT_TRUE: usize = 2;
pub(crate) const SLOT_NUMBER: usize = 3;
pub(crate) const SLOT_STRING: usize = 4;
pub(crate) const SLOT_ARRAY: usize = 5;
pub(crate) const SLOT_OBJECT: usize = 6;
pub(crate) const SLOT_KEY: usize = 7;

/// Default palette fields: `0;90` null, `0;39` false/true/numbers, `0;32` strings, `1;39` arrays/objects, `1;34` keys.
/// Wrapped as full escapes at construction; see the module palette law.
const DEFAULT_COLORS: [&str; 8] = ["0;90", "0;39", "0;39", "0;39", "0;32", "1;39", "1;39", "1;34"];

/// The reset after every coloured span. Never palette-derived.
pub(crate) const RESET: &str = "\x1b[0m";

/// One parsed palette: the eight full escape prefixes, `ESC[<field>m`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Palette {
    codes: [String; 8],
}

impl Palette {
    /// The default palette, used when `JQ_COLORS` is unset, empty, or malformed. See the module palette law.
    #[must_use]
    pub(crate) fn default_palette() -> Self {
        Self {
            codes: DEFAULT_COLORS.map(|field| format!("\x1b[{field}m")),
        }
    }

    /// The full escape prefix for one slot.
    #[must_use]
    pub(crate) fn colour(&self, slot: usize) -> &str {
        &self.codes[slot]
    }
}

/// Parses `JQ_COLORS`. See the module palette law.
///
/// The 8th field skips the delimiter check (garbage truncates to `[0-9;]`). A trailing `:` with an empty 8th field ends
/// the count at seven.
pub(crate) fn parse_jq_colors(value: Option<&OsStr>) -> Result<Palette, ()> {
    let Some(value) = value else {
        return Ok(Palette::default_palette());
    };
    // A non-UTF-8 JQ_COLORS is byte-garbage; the replacement character fails the `[0-9;]` field scan.
    let bytes = value.to_string_lossy();
    let bytes = bytes.as_bytes();
    let mut codes = [0usize; 9];
    let mut num_colors = 0usize;
    let mut pos = 0usize;
    loop {
        codes[num_colors] = pos;
        while pos < bytes.len() && (bytes[pos].is_ascii_digit() || bytes[pos] == b';') {
            pos += 1;
        }
        if pos >= bytes.len() || num_colors + 1 >= 8 {
            break;
        }
        if bytes[pos] != b':' {
            return Err(());
        }
        pos += 1;
        num_colors += 1;
    }
    if codes[num_colors] != pos {
        num_colors += 1;
        // End is stored one past the scan position, so every counted field's end is `codes[ci+1] - 1` (the last field
        // ends at `pos`, possibly one past the buffer — clamped below).
        codes[num_colors] = pos + 1;
    } else if num_colors == 0 {
        // An empty JQ_COLORS: all defaults, no message.
        return Ok(Palette::default_palette());
    }
    let mut palette = Palette::default_palette();
    for ci in 0..num_colors {
        let end = codes[ci + 1].saturating_sub(1).min(bytes.len()).max(codes[ci]);
        let field = &bytes[codes[ci]..end];
        palette.codes[ci] = format!("\x1b[{}m", std::str::from_utf8(field).unwrap_or_default());
    }
    Ok(palette)
}

/// The per-item colour state the sink owns when colour is engaged.
///
/// The request facts travel here; the per-item raw-text fact arrives in the item report at `finish_item`, which is why
/// a colouring sink buffers one item at a time.
#[derive(Clone, Debug)]
pub(crate) struct ColourRender {
    pub(crate) palette: Palette,
    /// The request's raw arm is engaged (`raw_strings`, resolved from `-r`/`-j`/`--raw-output0`): a ROOT text item
    /// prints its bytes verbatim with no colour. Only JSON and json-seq output can carry the raw arm.
    pub(crate) raw_arm: bool,
    /// `-a`/`--ascii-output` precedes the raw arm: the root string is re-quoted, so it colours as an ordinary string.
    pub(crate) ascii: bool,
    /// The item bytes are `render.terminal@1` tree-shape frames, not JSON tokens: they render with [`render_terminal`]
    /// instead of [`render`]. Engaged only for terminal-tree output; the plain and table shapes have no lexical token
    /// law to colour.
    pub(crate) terminal_tree: bool,
}

/// Decides whether this request renders colour. See the module decision law.
#[allow(
    clippy::fn_params_excessive_bools,
    reason = "the decision is the four-way switch (force off, force on, tty, no-color) over two \
              jqf gates; bundling the bools would hide the law"
)]
#[must_use]
pub(crate) fn colour_engaged(
    request: ColourRequest,
    destination_is_tty: bool,
    no_color_nonempty: bool,
    data_lane: bool,
    json_family: bool,
) -> bool {
    let on = match request {
        ColourRequest::ForceOff => false,
        ColourRequest::ForceOn => !data_lane,
        ColourRequest::Auto => destination_is_tty && !no_color_nonempty,
    };
    on && json_family
}

/// Whether the output selection is JSON-family: the only formats whose bytes are JSON tokens a colour pass may render.
#[must_use]
pub(crate) fn is_json_family(format: CliFormat) -> bool {
    matches!(format, CliFormat::Json | CliFormat::JsonSeq | CliFormat::Ndjson)
}

/// The enclosing-container kind, for the comma/colon/key colour decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Container {
    Array,
    Object,
}

/// The previous significant token, for the object-key decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Prev {
    /// The previous token was `{` or `[` (or a value, a close, a literal).
    Other,
    /// The previous token was `{`: the next string inside an object is a key.
    OpenObject,
    /// The previous token was `,`: the next string inside an object is a key.
    Comma,
}

/// Renders one item's JSON tokens into `out`. See the module span law.
///
/// The bytes are valid JSON for a coloured item (the raw-arm skip happens in the sink). A quoted string ends at the
/// first unescaped quote, a number is the maximal run of `[0-9eE+-.]`, and the literals match their exact words.
#[allow(
    clippy::too_many_lines,
    reason = "one linear tokenizer pass over the item's bytes; splitting the match would \
              thread the container stack and the previous-token state through helpers"
)]
pub(crate) fn render(palette: &Palette, item: &[u8], out: &mut Vec<u8>) {
    let mut containers: Vec<Container> = Vec::with_capacity(8);
    let mut prev = Prev::Other;
    let mut i = 0usize;
    while i < item.len() {
        let byte = item[i];
        match byte {
            b'{' => {
                if item.get(i + 1) == Some(&b'}') {
                    // An empty object is one span over both characters.
                    push_span(out, palette.colour(SLOT_OBJECT), &item[i..i + 2]);
                    i += 2;
                    prev = Prev::Other;
                } else {
                    push_span(out, palette.colour(SLOT_OBJECT), &item[i..=i]);
                    containers.push(Container::Object);
                    i += 1;
                    prev = Prev::OpenObject;
                }
            }
            b'}' => {
                push_span(out, palette.colour(SLOT_OBJECT), &item[i..=i]);
                containers.pop();
                i += 1;
                prev = Prev::Other;
            }
            b'[' => {
                if item.get(i + 1) == Some(&b']') {
                    push_span(out, palette.colour(SLOT_ARRAY), &item[i..i + 2]);
                    i += 2;
                    prev = Prev::Other;
                } else {
                    push_span(out, palette.colour(SLOT_ARRAY), &item[i..=i]);
                    containers.push(Container::Array);
                    i += 1;
                    prev = Prev::Other;
                }
            }
            b']' => {
                push_span(out, palette.colour(SLOT_ARRAY), &item[i..=i]);
                containers.pop();
                i += 1;
                prev = Prev::Other;
            }
            b',' => {
                // The comma takes its ENCLOSING container's colour ( an array comma is the array colour, an object
                // comma the object colour).
                let colour = match containers.last() {
                    Some(Container::Array) => palette.colour(SLOT_ARRAY),
                    _ => palette.colour(SLOT_OBJECT),
                };
                push_span(out, colour, &item[i..=i]);
                i += 1;
                prev = Prev::Comma;
            }
            b':' => {
                push_span(out, palette.colour(SLOT_OBJECT), &item[i..=i]);
                i += 1;
                prev = Prev::Other;
            }
            b'"' => {
                let is_key =
                    matches!(prev, Prev::OpenObject | Prev::Comma) && containers.last() == Some(&Container::Object);
                let colour = if is_key {
                    palette.colour(SLOT_KEY)
                } else {
                    palette.colour(SLOT_STRING)
                };
                let end = scan_string(item, i);
                push_span(out, colour, &item[i..end]);
                i = end;
                prev = Prev::Other;
            }
            b't' if item[i..].starts_with(b"true") => {
                push_span(out, palette.colour(SLOT_TRUE), b"true");
                i += 4;
                prev = Prev::Other;
            }
            b'f' if item[i..].starts_with(b"false") => {
                push_span(out, palette.colour(SLOT_FALSE), b"false");
                i += 5;
                prev = Prev::Other;
            }
            b'n' if item[i..].starts_with(b"null") => {
                push_span(out, palette.colour(SLOT_NULL), b"null");
                i += 4;
                prev = Prev::Other;
            }
            b'-' | b'0'..=b'9' => {
                // Valid JSON makes the maximal run exact: a number cannot be followed by another number character
                // structurally.
                let start = i;
                while i < item.len() && matches!(item[i], b'-' | b'0'..=b'9' | b'e' | b'E' | b'+' | b'.') {
                    i += 1;
                }
                push_span(out, palette.colour(SLOT_NUMBER), &item[start..i]);
                prev = Prev::Other;
            }
            _ => {
                // Whitespace and every non-token byte (framing RS, LF, CR, NUL) pass through untouched: colour never
                // renders them.
                out.push(byte);
                i += 1;
            }
        }
    }
}

/// One coloured span: prefix, the token's own bytes, reset.
fn push_span(out: &mut Vec<u8>, colour: &str, token: &[u8]) {
    out.extend_from_slice(colour.as_bytes());
    out.extend_from_slice(token);
    out.extend_from_slice(RESET.as_bytes());
}

/// Scans a quoted string starting at `start` (the opening quote), returning the index one past the closing quote. A
/// backslash skips the NEXT byte (`\"`, `\\`, and `\uXXXX`'s own `\u` — the hex digits are ordinary bytes), so an
/// escaped quote never ends the token.
fn scan_string(item: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < item.len() {
        match item[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    // Defensive: a coloured item is guaranteed valid JSON by the raw-arm skip, so an unterminated string cannot occur;
    // render the rest as the token rather than panic.
    item.len()
}

/// Renders one item's `render.terminal@1` TREE-SHAPE frame into `out` with the palette's colour spans.
///
/// The same law as [`render`], over a different byte shape: colour is a rendering of bytes that are already decided, so
/// this pass only inserts `ESC[<colour>m... ESC[0m` around the tokens the tree frame owns and copies every other byte
/// untouched — stripping the spans recovers the plain frame exactly.
///
/// The tree line grammar is exact (the codec writes it, one node per line): indent spaces, a path (`$` then
/// `[`-quoted-string-or-number`]#N` segments) — coloured as a KEY — the literal ` = `, an optional `&N ` alias, and a
/// term. Terms colour by kind: strings via [`scan_string`], numbers, `true`/`false`/`null`, `array(N)`/`object(N)` in
/// their container colours, and a tag's quoted payload in the string colour. Everything else — the `=` separator,
/// aliases, punctuation — passes through uncoloured.
pub(crate) fn render_terminal(palette: &Palette, item: &[u8], out: &mut Vec<u8>) {
    let mut i = 0usize;
    while i < item.len() {
        // Indent spaces are never coloured; each node line starts at column 0.
        while i < item.len() && item[i] == b' ' {
            out.push(b' ');
            i += 1;
        }
        if i >= item.len() {
            break;
        }
        if item[i] != b'$' {
            // Not a node line start (a separator LF, or a defensive tail): copy through end of line, including the LF,
            // untouched.
            while i < item.len() {
                let byte = item[i];
                out.push(byte);
                i += 1;
                if byte == b'\n' {
                    break;
                }
            }
            continue;
        }
        // The path: `$`, then `[...]#N` segments. Keys are JSON-quoted, so the scan is exact even when a key contains `
        // = `.
        let path_start = i;
        i += 1;
        while i < item.len() && item[i] == b'[' {
            i += 1;
            if i < item.len() && item[i] == b'"' {
                i = scan_string(item, i);
            } else {
                while i < item.len() && item[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < item.len() && item[i] == b']' {
                i += 1;
            }
            if i < item.len() && item[i] == b'#' {
                i += 1;
                while i < item.len() && item[i].is_ascii_digit() {
                    i += 1;
                }
            }
        }
        push_span(out, palette.colour(SLOT_KEY), &item[path_start..i]);
        write_separator_and_alias(item, &mut i, out);
        write_term(out, palette, item, &mut i);
    }
}

/// Copies the ` = ` separator and any `&N ` anchor prefix, uncoloured.
fn write_separator_and_alias(item: &[u8], i: &mut usize, out: &mut Vec<u8>) {
    while *i < item.len() {
        match item[*i] {
            b' ' | b'=' => {
                out.push(item[*i]);
                *i += 1;
            }
            b'&' => {
                out.push(b'&');
                *i += 1;
                while *i < item.len() && item[*i].is_ascii_digit() {
                    out.push(item[*i]);
                    *i += 1;
                }
            }
            _ => break,
        }
    }
}

/// Writes one node term with its kind's span, up to (not including) the LF.
fn write_term(out: &mut Vec<u8>, palette: &Palette, item: &[u8], i: &mut usize) {
    let start = *i;
    match item.get(start) {
        Some(b'"') => {
            let end = scan_string(item, start);
            push_span(out, palette.colour(SLOT_STRING), &item[start..end]);
            *i = end;
        }
        Some(b't') if item[start..].starts_with(b"true") => {
            push_span(out, palette.colour(SLOT_TRUE), b"true");
            *i = start + 4;
        }
        Some(b'f') if item[start..].starts_with(b"false") => {
            push_span(out, palette.colour(SLOT_FALSE), b"false");
            *i = start + 5;
        }
        Some(b'n') if item[start..].starts_with(b"null") => {
            push_span(out, palette.colour(SLOT_NULL), b"null");
            *i = start + 4;
        }
        Some(b'a') if item[start..].starts_with(b"array(") => {
            *i = scan_container_term(item, start);
            push_span(out, palette.colour(SLOT_ARRAY), &item[start..*i]);
        }
        Some(b'o') if item[start..].starts_with(b"object(") => {
            *i = scan_container_term(item, start);
            push_span(out, palette.colour(SLOT_OBJECT), &item[start..*i]);
        }
        Some(b't') if item[start..].starts_with(b"tag(") => {
            // `tag(` uncoloured; the quoted payload in the string colour.
            *i = start + 4;
            out.extend_from_slice(&item[start..*i]);
            if item.get(*i) == Some(&b'"') {
                let end = scan_string(item, *i);
                push_span(out, palette.colour(SLOT_STRING), &item[*i..end]);
                *i = end;
            }
        }
        Some(b'*') => {
            // A `*N` shared-container reference: copied whole, uncoloured.
            while *i < item.len() && item[*i].is_ascii_digit() {
                *i += 1;
            }
            out.extend_from_slice(&item[start..*i]);
        }
        Some(c) if c.is_ascii_digit() || *c == b'-' => {
            while *i < item.len() && matches!(item[*i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                *i += 1;
            }
            push_span(out, palette.colour(SLOT_NUMBER), &item[start..*i]);
        }
        _ => {}
    }
    // Rest of line, verbatim (defensive tail; well-formed terms end at LF).
    while *i < item.len() && item[*i] != b'\n' {
        out.push(item[*i]);
        *i += 1;
    }
}

/// The end of an `array(N)` / `object(N)` term: the index past its `)`.
fn scan_container_term(item: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < item.len() && item[i] != b')' && item[i] != b'\n' {
        i += 1;
    }
    if i < item.len() && item[i] == b')' { i + 1 } else { i }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette_fields(value: &str) -> [String; 8] {
        parse_jq_colors(Some(OsStr::new(value))).unwrap().codes
    }

    /// The default palette, and every probe-pinned parse corner.
    #[test]
    fn palette_defaults_and_parse_law() {
        assert_eq!(
            Palette::default_palette(),
            parse_jq_colors(None).unwrap(),
            "an unset JQ_COLORS is the default palette"
        );
        assert_eq!(
            Palette::default_palette(),
            parse_jq_colors(Some(OsStr::new(""))).unwrap(),
            "an empty JQ_COLORS is the default palette"
        );
        assert_eq!(
            palette_fields("1;31:2;32:3;33:4;34:5;35:6;36:7;37:8;38"),
            [
                "\x1b[1;31m",
                "\x1b[2;32m",
                "\x1b[3;33m",
                "\x1b[4;34m",
                "\x1b[5;35m",
                "\x1b[6;36m",
                "\x1b[7;37m",
                "\x1b[8;38m",
            ]
            .map(str::to_owned),
            "the eight fields land in kind order plus keys"
        );
        assert_eq!(
            palette_fields("1;31"),
            [
                "\x1b[1;31m",
                "\x1b[0;39m",
                "\x1b[0;39m",
                "\x1b[0;39m",
                "\x1b[0;32m",
                "\x1b[1;39m",
                "\x1b[1;39m",
                "\x1b[1;34m",
            ]
            .map(str::to_owned),
            "fewer than eight fields default the rest"
        );
        // The trailing-colon law: an empty 8th field ends the count at seven and the 8th DEFAULTS (keys were default
        // 1;34).
        let seven = palette_fields("1;31:2;32:3;33:4;34:5;35:6;36:7;37:");
        assert_eq!(
            seven,
            [
                "\x1b[1;31m",
                "\x1b[2;32m",
                "\x1b[3;33m",
                "\x1b[4;34m",
                "\x1b[5;35m",
                "\x1b[6;36m",
                "\x1b[7;37m",
                "\x1b[1;34m"
            ]
            .map(str::to_owned),
        );
        // An empty MID field is a VALID field whose colour is ESC[m ( false rendered `^[[mfalse^[[0m`).
        let empty_mid = palette_fields("1;31::3;33:4;34:5;35:6;36:7;37:8;38");
        assert_eq!(empty_mid[1], "\x1b[m");
        assert_eq!(empty_mid[0], "\x1b[1;31m");
        assert_eq!(empty_mid[2], "\x1b[3;33m");
        // A bare `;` is a valid field body (`ESC[;m`); an empty leading field is the empty SGR.
        let semicolon = palette_fields(":;");
        assert_eq!(semicolon[0], "\x1b[m");
        assert_eq!(semicolon[1], "\x1b[;m");
        // Garbage in fields 1..7 rejects the WHOLE palette.
        assert!(parse_jq_colors(Some(OsStr::new("notacolor:2;32"))).is_err());
        assert!(parse_jq_colors(Some(OsStr::new("1;31 :3;33"))).is_err());
        // Garbage in the 8th field is truncated to its [0-9;] prefix and the rest ignored (`...:7;37:8x:9;39` → keys
        // are `8`, no message).
        let eighth = palette_fields("1;31:2;32:3;33:4;34:5;35:6;36:7;37:8x:9;39");
        assert_eq!(eighth[7], "\x1b[8m");
        // Fields beyond eight are ignored.
        assert_eq!(
            palette_fields("1;31:2;32:3;33:4;34:5;35:6;36:7;37:8;38:9;39:10;40"),
            palette_fields("1;31:2;32:3;33:4;34:5;35:6;36:7;37:8;38"),
        );
    }

    /// The decision law, (TTY rows via a python pty).
    #[test]
    fn colour_decision_law() {
        // Default: TTY + no NO_COLOR → on; NO_COLOR (non-empty) kills the default; an EMPTY NO_COLOR is ignored.
        assert!(colour_engaged(ColourRequest::Auto, true, false, false, true));
        assert!(!colour_engaged(ColourRequest::Auto, true, true, false, true));
        assert!(!colour_engaged(ColourRequest::Auto, false, false, false, true));
        // A non-TTY destination never auto-colours.
        assert!(!colour_engaged(ColourRequest::Auto, false, false, false, true));
        // -C forces on, even under NO_COLOR.
        assert!(colour_engaged(ColourRequest::ForceOn, false, true, false, true));
        // -M forces off and always wins (-C -M -C is monochrome).
        assert!(!colour_engaged(ColourRequest::ForceOff, true, false, false, true));
        // jqf's extensions: data lanes and non-JSON output never colour.
        assert!(!colour_engaged(ColourRequest::ForceOn, false, false, true, true));
        assert!(!colour_engaged(ColourRequest::ForceOn, false, false, false, false));
    }

    /// Renders one item and returns the coloured bytes.
    fn render_item(item: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        render(&Palette::default_palette(), item, &mut out);
        out
    }

    /// Strips every ESC[..m span, leaving the decided bytes.
    fn strip(coloured: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < coloured.len() {
            if coloured[i] == 0x1b && coloured.get(i + 1) == Some(&b'[') {
                i += 2;
                while i < coloured.len() && coloured[i] != b'm' {
                    i += 1;
                }
                i += 1;
            } else {
                out.push(coloured[i]);
                i += 1;
            }
        }
        out
    }

    /// The strip identity: colour adds spans and changes nothing else.
    #[test]
    fn strip_identity_over_representative_shapes() {
        let shapes: &[&[u8]] = &[
            b"{\"a\":1}",
            b"{\"a\":1,\"b\":[true,false,\"s\",1.5]}",
            b"{\"a\":{\"b\":{\"c\":[]}}}",
            b"[{},{\"k\":[]},[]]",
            b"{\"escaped\":\"a\\\"b\\\\c\\u0041\",\"nested\":{\"x\":null}}",
            b"[\"h\\u00e9llo\",-0.5e+3,1E2,0]",
            b"{\"a\":1}\n",
            b"{\"a\":1}\r\n",
            b"\x1e{\"a\":1}\n",
            b"{\"a\":1}\0",
            b"{}",
            b"[]",
            b"null",
            b"true",
            b"false",
            b"0",
            b"-1.25e-10",
        ];
        for shape in shapes {
            let coloured = render_item(shape);
            assert!(coloured.len() > shape.len(), "colour must add spans for {shape:?}");
            assert_eq!(strip(&coloured), *shape, "strip identity for {shape:?}");
        }
    }

    /// The span law, pinned against the probe transcript: keys use palette[7], the comma takes its enclosing
    /// container's colour, the colon the object colour, and an empty container is ONE span.
    #[test]
    fn span_law_matches_the_palette() {
        let probe = "1;31:2;32:3;33:4;34:5;35:6;36:7;37:8;38";
        let palette = parse_jq_colors(Some(OsStr::new(probe))).unwrap();
        let mut out = Vec::new();
        render(&palette, b"{\"a\":[1,2],\"b\":{\"c\":3}}", &mut out);
        assert_eq!(
            out,
            b"\x1b[7;37m{\x1b[0m\
              \x1b[8;38m\"a\"\x1b[0m\x1b[7;37m:\x1b[0m\
              \x1b[6;36m[\x1b[0m\x1b[4;34m1\x1b[0m\x1b[6;36m,\x1b[0m\x1b[4;34m2\x1b[0m\x1b[6;36m]\x1b[0m\
              \x1b[7;37m,\x1b[0m\x1b[8;38m\"b\"\x1b[0m\x1b[7;37m:\x1b[0m\
              \x1b[7;37m{\x1b[0m\x1b[8;38m\"c\"\x1b[0m\x1b[7;37m:\x1b[0m\x1b[4;34m3\x1b[0m\x1b[7;37m}\x1b[0m\x1b[7;37m}\x1b[0m"
        );
        // Empty containers: ONE span over both characters ( `^[[7;37m{}^[[0m` / `^[[6;36m[]^[[0m`), and `,` inside an
        // array is the ARRAY colour.
        let mut out = Vec::new();
        render(&palette, b"[[],{},{\"k\":{}}]", &mut out);
        assert_eq!(
            out,
            b"\x1b[6;36m[\x1b[0m\x1b[6;36m[]\x1b[0m\x1b[6;36m,\x1b[0m\x1b[7;37m{}\x1b[0m\
              \x1b[6;36m,\x1b[0m\x1b[7;37m{\x1b[0m\x1b[8;38m\"k\"\x1b[0m\x1b[7;37m:\x1b[0m\
              \x1b[7;37m{}\x1b[0m\x1b[7;37m}\x1b[0m\x1b[6;36m]\x1b[0m"
        );
    }

    /// The default palette's exact bytes, from the first probe transcript.
    #[test]
    fn default_palette_bytes() {
        let mut out = Vec::new();
        render(&Palette::default_palette(), b"{\"a\":1,\"b\":[true]}", &mut out);
        assert_eq!(
            out,
            b"\x1b[1;39m{\x1b[0m\
              \x1b[1;34m\"a\"\x1b[0m\x1b[1;39m:\x1b[0m\x1b[0;39m1\x1b[0m\x1b[1;39m,\x1b[0m\
              \x1b[1;34m\"b\"\x1b[0m\x1b[1;39m:\x1b[0m\x1b[1;39m[\x1b[0m\
              \x1b[0;39mtrue\x1b[0m\x1b[1;39m]\x1b[0m\x1b[1;39m}\x1b[0m"
        );
        // The pretty rendering keeps its whitespace UNCOLOURED: the probe's `{\"a\":1}` output was `^[[1;39m{^[[0m\n
        // ^[[1;34m\"a\"^[[0m...`.
        let mut out = Vec::new();
        render(&Palette::default_palette(), b"{\"a\": 1}\n", &mut out);
        assert_eq!(
            out,
            b"\x1b[1;39m{\x1b[0m\x1b[1;34m\"a\"\x1b[0m\x1b[1;39m:\x1b[0m \
              \x1b[0;39m1\x1b[0m\x1b[1;39m}\x1b[0m\n"
        );
    }
}
