//! The layout atom model: one grapheme's exact output bytes and its displayed width, and the per-renderer escaping that
//! turns a source grapheme into one unsplittable atom.
//!
//! Layout never operates on already-escaped bytes as undifferentiated text: a GFM `&#x7C;` atom displays `|` at that
//! cluster's width and an HTML `&amp;` atom displays `&`, while a visible-control atom measures its complete literal
//! `U+0009`-style text. Entity, escape, and grapheme bytes are never split, and width wrapping only ever happens
//! between atoms.
//!
//! The width law is grapheme-aware and self-contained (no generated Unicode tables): combining and continuation scalars
//! are discarded for width classification, an emoji-presentation cluster is width 2, the remaining scalars take the
//! maximum of their East-Asian-width class (`W`/`F` = 2, `N`/`Na`/`H` = 1), and the ambiguous class answers per width
//! profile. This is the documented v1 law; the exact Unicode 17 pinned table is the `render.unicode17@1` follow-on.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use jqf_codec_core::CodecError;

use super::options::WidthProfile;

/// Which renderer's escaping an atom carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EscapeStyle {
    /// GFM table escaping (`&#xHH;` punctuation, `U&#x2B;` controls).
    Gfm,
    /// HTML table escaping (`&amp;`-style entities, `\u{...}` controls).
    Html,
    /// ASCII grid escaping (`\\`, `\0`/`\t`/`\n`/`\r`, `\xHH`, `\u{...}`).
    Grid,
}

/// One unsplittable layout atom: exact output bytes and checked display width.
#[derive(Debug)]
pub(crate) struct LayoutAtom {
    /// Exact escaped output bytes.
    pub bytes: String,
    /// Checked displayed width in display cells.
    pub width: u16,
}

impl LayoutAtom {
    /// A literal ASCII run whose displayed text is its own bytes.
    fn literal(bytes: &str, width: u16) -> Self {
        Self {
            bytes: String::from(bytes),
            width,
        }
    }
}

/// One element of a cell's atom stream: a layout atom or a forced visual-line break. GFM/HTML split source newlines
/// into breaks; grid turns them into visible escape atoms instead.
#[derive(Debug)]
pub(crate) enum AtomOrBreak {
    /// One unsplittable atom.
    Atom(LayoutAtom),
    /// A forced visual-line break.
    Break,
}

/// Atomizes one source cell's plain text under one renderer's escape law.
///
/// # Errors
///
/// Returns an allocation failure when reserving the output buffers fails.
pub(crate) fn atomize(text: &str, style: EscapeStyle, width: WidthProfile) -> Result<Vec<AtomOrBreak>, CodecError> {
    let mut out = Vec::new();
    let mut iter = text.chars().peekable();
    // One reusable cluster buffer, reserved once and cleared per grapheme: the LayoutAtom copies its bytes out, so the
    // loop allocates once instead of once per cluster of every cell.
    let mut cluster = String::new();
    cluster
        .try_reserve(4)
        .map_err(|_| jqf_codec_core::CodecError::new(jqf_codec_core::CodecFailureKind::AllocationFailure))?;
    while let Some(base) = iter.next() {
        // CR LF is one extended grapheme cluster.
        cluster.clear();
        cluster.push(base);
        if base == '\r' && iter.peek() == Some(&'\n') {
            cluster.push('\n');
            iter.next();
        } else {
            // Attach every following extend scalar (combining marks, variation selectors, ZWJ/ZWNJ, keycap).
            while let Some(&next) = iter.peek() {
                if is_extend(next) {
                    cluster.push(next);
                    iter.next();
                } else {
                    break;
                }
            }
        }
        match cluster_atom(&cluster, style, width) {
            Some(atom) => out.push(AtomOrBreak::Atom(atom)),
            None => out.push(AtomOrBreak::Break),
        }
    }
    Ok(out)
}

/// Renders one grapheme cluster as an atom, or reports a forced break.
fn cluster_atom(cluster: &str, style: EscapeStyle, width: WidthProfile) -> Option<LayoutAtom> {
    // Newlines are forced breaks for GFM/HTML and visible escapes for grid.
    let is_newline = matches!(cluster, "\r\n" | "\r" | "\n" | "\u{2028}" | "\u{2029}");
    if is_newline {
        if style == EscapeStyle::Grid {
            return Some(escaped_controls(cluster, width));
        }
        return None;
    }
    match style {
        EscapeStyle::Gfm => Some(gfm_atom(cluster, width)),
        EscapeStyle::Html => Some(html_atom(cluster, width)),
        EscapeStyle::Grid => Some(escaped_controls(cluster, width)),
    }
}

/// One GFM atom. Escaping is decided PER SCALAR, never per cluster: a combining mark attached to `|` must not smuggle a
/// raw cell delimiter into the markdown source, so every scalar of the cluster is tested. A scalar escapes only what
/// GFM requires inside a table cell — `&`/`<`/`>` (raw HTML and entity safety) and `|` (the cell delimiter) become
/// `&#xHH;`, a control/bidi scalar becomes `U&#x2B;XXXX`, anything else copies raw. Escaping every punctuation scalar
/// (`-` as `&#x2D;`, `.` as `&#x2E;`) rendered the MARKDOWN SOURCE copy-hostile while displaying identically, so the
/// minimal law replaced it.
fn gfm_atom(cluster: &str, width_profile: WidthProfile) -> LayoutAtom {
    let mut bytes = String::new();
    for ch in cluster.chars() {
        match ch {
            '&' | '<' | '>' | '|' => {
                write!(bytes, "&#x{:02X};", ch as u32).expect("String writes are infallible");
            }
            control if is_visible_control(control) => {
                write!(bytes, "U&#x2B;{:04X}", ch as u32).expect("String writes are infallible");
            }
            other => {
                let mut scratch = [0_u8; 4];
                bytes.push_str(other.encode_utf8(&mut scratch));
            }
        }
    }
    // The atom's displayed text is the source cluster either way — an entity displays as the grapheme it represents
    // — so every branch measures the cluster's own width.
    LayoutAtom {
        width: cluster_display_width(cluster, width_profile),
        bytes,
    }
}

/// One HTML atom. Escaping is decided PER SCALAR, never per cluster, so a combining mark attached to `<` or `&` cannot
/// leak raw HTML into the cell: `&`/`<`/`>`/`"`/`'` become entities wherever they sit in the cluster, a control/bidi
/// scalar becomes `\u{...}`, anything else copies raw.
fn html_atom(cluster: &str, width_profile: WidthProfile) -> LayoutAtom {
    let mut bytes = String::new();
    for ch in cluster.chars() {
        let entity = match ch {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            '\'' => "&#39;",
            _ => "",
        };
        if !entity.is_empty() {
            bytes.push_str(entity);
        } else if is_visible_control(ch) {
            write!(bytes, "\\u{{{:04x}}}", ch as u32).expect("String writes are infallible");
        } else {
            let mut scratch = [0_u8; 4];
            bytes.push_str(ch.encode_utf8(&mut scratch));
        }
    }
    // The atom's displayed text is the source cluster either way — an entity displays as the grapheme it represents
    // — so both branches measure the cluster's own width.
    LayoutAtom {
        width: cluster_display_width(cluster, width_profile),
        bytes,
    }
}

// ponytail: per-atom construction is INFALLIBLE — write! for the GFM/HTML entities (gfm_atom/html_atom), write! for
// the grid escape runs below, String::from for literal/plain atoms, and the atomize/wrap Vec pushes — so an OOM on
// any single atom's small allocation aborts instead of surfacing AllocationFailure. Accepted narrowing, sibling of
// table.rs's emission law: each allocation is one grapheme's escaped bytes (tens of bytes max), the total escaped cell
// bytes are capped by the sampled-layout byte ceiling, and the cell TEXT itself flowed through the fallible scalar
// path. The abort window is a per-atom allocation under memory pressure while atomizing an already-bounded cell; use
// try_reserve here if that window must close.
/// One grid atom: backslash, NUL/TAB/LF/CR, other C0/C1/DEL, U+2028/U+2029, and bidi controls all become visible
/// escapes; everything else copies raw. Source newlines are VISIBLE text, so grid cells have no forced breaks.
fn escaped_controls(cluster: &str, width_profile: WidthProfile) -> LayoutAtom {
    let mut bytes = String::new();
    let mut need_display_width = false;
    for ch in cluster.chars() {
        let code = ch as u32;
        match ch {
            '\\' => {
                bytes.push_str("\\\\");
                need_display_width = true;
            }
            '\u{0000}' => {
                bytes.push_str("\\0");
                need_display_width = true;
            }
            '\t' => {
                bytes.push_str("\\t");
                need_display_width = true;
            }
            '\n' => {
                bytes.push_str("\\n");
                need_display_width = true;
            }
            '\r' => {
                bytes.push_str("\\r");
                need_display_width = true;
            }
            _ if code < 0x20 || (0x7F..=0x9F).contains(&code) => {
                write!(bytes, "\\x{code:02x}").expect("String writes are infallible");
                need_display_width = true;
            }
            _ if matches!(code, 0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069)
                || matches!(ch, '\u{2028}' | '\u{2029}') =>
            {
                write!(bytes, "\\u{{{code:04x}}}").expect("String writes are infallible");
                need_display_width = true;
            }
            other => {
                let mut scratch = [0_u8; 4];
                bytes.push_str(other.encode_utf8(&mut scratch));
            }
        }
    }
    if need_display_width {
        // The whole escape run is the atom; its displayed text is its own ASCII bytes, so the width is the byte length.
        LayoutAtom::literal(&bytes, u16::try_from(bytes.len()).unwrap_or(u16::MAX))
    } else {
        let width = cluster_display_width(cluster, width_profile);
        LayoutAtom { bytes, width }
    }
}

/// TAB, every C0/C1/DEL, and the named bidi controls are made visible.
fn is_visible_control(ch: char) -> bool {
    let code = ch as u32;
    code == 0x09
        || code < 0x20
        || code == 0x7F
        || (0x80..=0x9F).contains(&code)
        || matches!(code, 0x061C | 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069)
}

/// Whether a scalar attaches to the preceding base in an extended grapheme cluster: combining marks, variation
/// selectors, ZWJ/ZWNJ, and the keycap.
fn is_extend(ch: char) -> bool {
    let code = ch as u32;
    is_combining(code) || matches!(code, 0xFE0E | 0xFE0F | 0x200C | 0x200D | 0x20E3)
}

/// Combining/continuation scalar classes that are discarded for width.
fn is_combining(code: u32) -> bool {
    (0x0300..=0x036F).contains(&code)
        || (0x0483..=0x0489).contains(&code)
        || (0x1AB0..=0x1AFF).contains(&code)
        || (0x1DC0..=0x1DFF).contains(&code)
        || (0x20D0..=0x20FF).contains(&code)
        || (0xFE20..=0xFE2F).contains(&code)
}

/// The displayed width of one grapheme cluster under the v1 width law.
pub(crate) fn cluster_display_width(cluster: &str, profile: WidthProfile) -> u16 {
    if cluster.is_empty() {
        return 0;
    }
    // An emoji-presentation cluster — VS16, a regional-indicator pair, a keycap, or an RGI-style ZWJ emoji sequence
    // — is width 2 first.
    if is_emoji_presentation(cluster) {
        return 2;
    }
    // Discard combining, variation-selector, ZWJ, and default-ignorable continuation scalars, then take the maximum
    // width class of the rest.
    let mut width = 0_u16;
    let mut printable = false;
    for ch in cluster.chars() {
        let code = ch as u32;
        if is_combining(code) || matches!(code, 0xFE0E | 0xFE0F | 0x200C | 0x200D | 0x20E3) {
            continue;
        }
        printable = true;
        width = width.max(scalar_class_width(ch, profile));
    }
    // A nonempty cluster with no printable base has the visible fallback width 1.
    if printable { width } else { 1 }
}

/// The East-Asian-width-style class of one printable scalar.
fn scalar_class_width(ch: char, profile: WidthProfile) -> u16 {
    let code = ch as u32;
    if is_wide(code) {
        2
    } else if is_ambiguous(code) {
        match profile {
            WidthProfile::Western => 1,
            WidthProfile::Cjk => 2,
        }
    } else {
        1
    }
}

/// Wide or fullwidth scalar ranges (`W`/`F` in East Asian Width terms), a practical subset of the BMP plus the common
/// emoji/supplementary ranges.
fn is_wide(code: u32) -> bool {
    (0x1100..=0x115F).contains(&code)          // Hangul Jamo
        || (0x2329..=0x232A).contains(&code)   // angle brackets
        || (0x2E80..=0x303E).contains(&code)   // CJK radicals, punctuation, Kangxi
        || (0x3041..=0x33FF).contains(&code)   // Hiragana/Katakana/CJK compat
        || (0x3400..=0x4DBF).contains(&code)   // CJK Ext A
        || (0x4E00..=0x9FFF).contains(&code)   // CJK unified
        || (0xA000..=0xA4CF).contains(&code)   // Yi
        || (0xAC00..=0xD7A3).contains(&code)   // Hangul syllables
        || (0xF900..=0xFAFF).contains(&code)   // CJK compat ideographs
        || (0xFE10..=0xFE19).contains(&code)   // vertical forms
        || (0xFE30..=0xFE6F).contains(&code)   // CJK compat forms
        || (0xFF00..=0xFF60).contains(&code)   // fullwidth forms
        || (0xFFE0..=0xFFE6).contains(&code)   // fullwidth signs
        || (0x1F000..=0x1FAFF).contains(&code) // Mahjong..symbols+emoji
        || (0x20000..=0x2FFFD).contains(&code) // CJK Ext B..F
        || (0x30000..=0x3FFFD).contains(&code) // CJK Ext G
}

/// Ambiguous-width scalar ranges (`A`), a practical subset; the answer is the width profile's.
fn is_ambiguous(code: u32) -> bool {
    matches!(code,
        0x00A1 | 0x00A4 | 0x00A7..=0x00A8 | 0x00AA | 0x00AD | 0x00AE
        | 0x00B0..=0x00B4 | 0x00B6..=0x00BA | 0x00BC..=0x00BF | 0x00C6
        | 0x00D0 | 0x00D7..=0x00D8 | 0x00DE..=0x00E1 | 0x00E6
        | 0x00E8..=0x00EA | 0x00EC..=0x00ED | 0x00F0 | 0x00F2..=0x00F3
        | 0x00F7..=0x00FA | 0x00FC | 0x00FE | 0x0101 | 0x0111 | 0x0113
        | 0x011B | 0x0126..=0x0127 | 0x012B | 0x0131..=0x0133 | 0x0138
        | 0x013F..=0x0142 | 0x0144 | 0x0148..=0x014B | 0x014D
        | 0x0152..=0x0153 | 0x0166..=0x0167 | 0x016B | 0x01CE | 0x01D0
        | 0x01D2 | 0x01D4 | 0x01D6 | 0x01D8 | 0x01DA | 0x01DC | 0x0251
        | 0x0261 | 0x02C4 | 0x02C7 | 0x02C9..=0x02CB | 0x02CD | 0x02D0
        | 0x02D8..=0x02DB | 0x02DD | 0x02DF | 0x0387 | 0x03A9 | 0x03BC
        | 0x03C0 | 0x03E2 | 0x03F6 | 0x2010 | 0x2013..=0x2016 | 0x2018
        | 0x2019 | 0x201C | 0x201D | 0x201E | 0x2020..=0x2022 | 0x2024..=0x2027
        | 0x2030 | 0x2032..=0x2033 | 0x2035 | 0x203B | 0x203E | 0x2074
        | 0x207F | 0x2081..=0x2084 | 0x20A3..=0x20A4 | 0x20A7 | 0x20AC
        | 0x2103 | 0x2105 | 0x2109 | 0x2113 | 0x2116 | 0x2121..=0x2122
        | 0x2126 | 0x212B | 0x2153..=0x2154 | 0x215B..=0x215E | 0x2160..=0x216B
        | 0x2170..=0x2179 | 0x2190..=0x2199 | 0x21B8..=0x21B9 | 0x21D2
        | 0x21D4 | 0x21E7 | 0x2200 | 0x2202..=0x2203 | 0x2207..=0x2208
        | 0x220B | 0x220F | 0x2211 | 0x2215 | 0x221A | 0x221D..=0x2220
        | 0x2223 | 0x2225 | 0x2227..=0x222C | 0x222E | 0x2234..=0x2237
        | 0x223C..=0x223D | 0x2248 | 0x224C | 0x2252 | 0x2260..=0x2261
        | 0x2264..=0x2267 | 0x226A..=0x226B | 0x226E..=0x226F | 0x2282..=0x2283
        | 0x2286..=0x2287 | 0x2295 | 0x2299 | 0x22A5 | 0x22BF | 0x2312
        | 0x2500..=0x254B | 0x2550..=0x2573 | 0x2580..=0x258F | 0x2592..=0x2595
        | 0x25A0..=0x25A1 | 0x25A3..=0x25A9 | 0x25B2..=0x25B3 | 0x25B6..=0x25B7
        | 0x25BC..=0x25BD | 0x25C0..=0x25C1 | 0x25C6..=0x25C8 | 0x25CB
        | 0x25CE..=0x25D1 | 0x25E2..=0x25E5 | 0x25EF | 0x2605..=0x2606
        | 0x2609 | 0x260E..=0x260F | 0x261C | 0x261E | 0x2640 | 0x2642
        | 0x2660..=0x2661 | 0x2663..=0x2665 | 0x2667..=0x266A | 0x266C..=0x266D
        | 0x266F | 0x273D | 0x2776..=0x277F | 0xE000..=0xF8FF | 0xFFFD
        | 0xF0000..=0xFFFFD | 0x10_0000..=0x10_FFFD)
}

/// Whether a cluster displays with emoji presentation (width 2): a VS16, a regional-indicator flag pair, a keycap, or a
/// ZWJ-emoji sequence.
fn is_emoji_presentation(cluster: &str) -> bool {
    let mut has_vs16 = false;
    let mut has_emoji = false;
    let mut has_zwj = false;
    let mut regional_count = 0_u32;
    for ch in cluster.chars() {
        let code = ch as u32;
        match code {
            0xFE0F => has_vs16 = true,
            0x200D => has_zwj = true,
            0x1F1E6..=0x1F1FF => regional_count += 1,
            0x20E3 => return true, // keycap
            other
                if (0x1F000..=0x1FAFF).contains(&other)
                    || (0x2600..=0x26FF).contains(&other)
                    || (0x2700..=0x27BF).contains(&other) =>
            {
                has_emoji = true;
            }
            _ => {}
        }
    }
    if regional_count >= 2 {
        return true;
    }
    has_vs16 || (has_zwj && has_emoji)
}
