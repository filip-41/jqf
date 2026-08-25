//! The HTML decoder front end: the WHATWG encoding determination.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::Namespace;

/// The HTML diagnostics namespace.
const HTML: Namespace = Namespace::new("html");

/// The bounded meta charset prescan length (the WHATWG rule: the first 1024 bytes, in a specific scanning order).
const PRESCAN_LIMIT: usize = 1024;

/// The WHATWG encoding determination: BOM, then the bounded `meta charset` prescan, then the deterministic windows-1252
/// fallback.
///
/// v1 decoding supports UTF-8 and windows-1252; a prescan label that resolves to a multi-byte encoding is refused with
/// a named `UnsupportedRepresentation` rather than silently misdecoded (the WHATWG index tables for the East Asian
/// encodings are not implemented).
pub(crate) fn determine_and_decode(bytes: &[u8]) -> Result<String, CodecError> {
    // 1. The BOM.
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(crate::tokenize::decode_utf8(&bytes[3..]));
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        // UTF-16LE is refused in v1.
        return Err(unsupported("UTF-16 input is not supported by html.document@1 v1"));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Err(unsupported("UTF-16 input is not supported by html.document@1 v1"));
    }
    // 2. The bounded meta charset prescan.
    let prescan = &bytes[..bytes.len().min(PRESCAN_LIMIT)];
    if let Some(label) = prescan_meta_charset(prescan) {
        // WHATWG strips leading/trailing ASCII whitespace from the label before the label lookup (`charset=" utf-8 "`
        // is `utf-8`).
        let label = trim_ascii_whitespace(&label).to_ascii_lowercase();
        match label.as_str() {
            "utf-8" | "utf8" | "unicode-1-1-utf-8" | "unicode11utf8" | "unicode20utf8" | "x-unicode20utf8" => {
                return Ok(crate::tokenize::decode_utf8(bytes));
            }
            // The legacy single-byte labels the Encoding Standard maps onto windows-1252.
            "windows-1252"
            | "iso-8859-1"
            | "iso8859-1"
            | "iso88591"
            | "latin1"
            | "ascii"
            | "us-ascii"
            | "iso-8859-1-windows-3.0-latin-1"
            | "iso-ir-100"
            | "l1"
            | "cp819"
            | "csisolatin1"
            | "ibm819"
            | "ansi_x3.4-1968"
            | "iso646-us"
            | "iso_646.irv:1991"
            | "us"
            | "cp1252"
            | "x-cp1252" => {
                return Ok(crate::tokenize::decode_windows_1252(bytes));
            }
            other => {
                return Err(unsupported(&format!(
                    "the meta charset label {other:?} resolves to an encoding html.document@1 v1 \
                         does not implement"
                )));
            }
        }
    }
    // 3. The deterministic windows-1252 fallback.
    Ok(crate::tokenize::decode_windows_1252(bytes))
}

/// The bounded `meta charset` prescan: the first 1024 bytes, scanning per the WHATWG rules.
fn prescan_meta_charset(bytes: &[u8]) -> Option<String> {
    let mut position = 0usize;
    loop {
        if position + 3 >= bytes.len() {
            return None;
        }
        if bytes[position] != b'<' {
            position += 1;
            continue;
        }
        if bytes[position..].starts_with(b"<!--") {
            position += 4;
            while position + 2 < bytes.len() && &bytes[position..position + 3] != b"-->" {
                position += 1;
            }
            if position + 2 < bytes.len() {
                position += 3;
            }
            continue;
        }
        if is_meta_tag(bytes, position) {
            position += 5;
            let mut attributes: Vec<(String, String)> = Vec::new();
            loop {
                // WHATWG "get an attribute" step 1 skips `/` (0x2F) along with the ASCII whitespace. The skip is what
                // guarantees progress: a lone `/` inside the tag (`<meta /x>`) is consumed by nothing else, and the
                // loop would spin forever pushing empty pairs. `/>` needs no separate exit — the `/` is skipped and the
                // `>` breaks below.
                while position < bytes.len() && (bytes[position].is_ascii_whitespace() || bytes[position] == b'/') {
                    position += 1;
                }
                if position >= bytes.len() {
                    return None;
                }
                if bytes[position] == b'>' {
                    break;
                }
                // An attribute name.
                let name_start = position;
                while position < bytes.len()
                    && !bytes[position].is_ascii_whitespace()
                    && !matches!(bytes[position], b'=' | b'>' | b'/')
                {
                    position += 1;
                }
                let mut name = String::new();
                for byte in &bytes[name_start..position] {
                    name.push(byte.to_ascii_lowercase() as char);
                }
                position = skip_ascii_whitespace(bytes, position);
                let mut value = String::new();
                if bytes.get(position) == Some(&b'=') {
                    position += 1;
                    position = skip_ascii_whitespace(bytes, position);
                    match bytes.get(position) {
                        Some(&quote @ (b'"' | b'\'')) => {
                            position += 1;
                            let value_start = position;
                            while position < bytes.len() && bytes[position] != quote {
                                position += 1;
                            }
                            if position >= bytes.len() {
                                return None;
                            }
                            value = String::from_utf8_lossy(&bytes[value_start..position]).into_owned();
                            position += 1;
                        }
                        Some(_) => {
                            let value_start = position;
                            while position < bytes.len()
                                && !bytes[position].is_ascii_whitespace()
                                && !matches!(bytes[position], b'>' | b'/')
                            {
                                position += 1;
                            }
                            value = String::from_utf8_lossy(&bytes[value_start..position]).into_owned();
                        }
                        None => {}
                    }
                }
                attributes.push((name, value));
            }
            // The charset attribute wins; the content attribute's `charset=` prefix is the older spelling. WHATWG
            // strips leading/trailing ASCII whitespace before the label lookup, and an empty-after-trim value declares
            // no charset (the scan continues past this tag, looking for a later `<meta charset>`).
            if let Some((_, value)) = attributes.iter().find(|(name, _)| name == "charset") {
                let trimmed = trim_ascii_whitespace(value);
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            if let Some((_, value)) = attributes.iter().find(|(name, _)| name == "content") {
                for part in value.split(';') {
                    let part = trim_ascii_whitespace(part);
                    let part = part.to_ascii_lowercase();
                    if let Some(charset) = part.strip_prefix("charset=") {
                        let trimmed = trim_ascii_whitespace(charset);
                        if !trimmed.is_empty() {
                            return Some(trimmed.to_string());
                        }
                    }
                }
            }
            // No usable charset: advance past the tag's `>` and keep scanning — WHATWG continues past a no-charset
            // meta, so a viewport-first page still finds a later `<meta charset>`.
            while position < bytes.len() && bytes[position] != b'>' {
                position += 1;
            }
            if position >= bytes.len() {
                return None;
            }
            position += 1;
            continue;
        }
        if bytes[position..].starts_with(b"</") {
            let close = bytes[position..]
                .iter()
                .position(|byte| *byte == b'>')
                .map(|offset| position + offset);
            match close {
                Some(close) => {
                    position = close + 1;
                }
                None => return None,
            }
            continue;
        }
        position += 1;
    }
}

/// Whether `position` begins a `<meta` start-tag name, matched ASCII case-insensitively (WHATWG tokenizes the tag
/// name), followed by a tag boundary — whitespace, `/`, `>`, or end of the scanned window — so `<metadata>` never
/// matches a `meta` tag.
fn is_meta_tag(bytes: &[u8], position: usize) -> bool {
    const TAG: &[u8] = b"<meta";
    let after = position + TAG.len();
    if bytes.len() <= after {
        return false;
    }
    if !bytes[position..after]
        .iter()
        .zip(TAG)
        .all(|(byte, expected)| byte.to_ascii_lowercase() == *expected)
    {
        return false;
    }
    matches!(bytes[after], b'\t' | b'\n' | b'\x0C' | b'\r' | b' ' | b'/' | b'>')
}

fn skip_ascii_whitespace(bytes: &[u8], mut position: usize) -> usize {
    while position < bytes.len() && bytes[position].is_ascii_whitespace() {
        position += 1;
    }
    position
}

/// Strips leading/trailing ASCII whitespace from a label before the encoding lookup. `str::trim_ascii` is the same
/// five-byte set WHATWG uses.
fn trim_ascii_whitespace(s: &str) -> &str {
    s.trim_ascii()
}

/// Constructs an `UnsupportedRepresentation` failure carrying a message that names the encoding problem — words, not
/// the class name: an HTML input the v1 encoding determination cannot serve fails with words about the INPUT (which
/// encoding, why), not the bare class name. The diagnostic is message-only (a decode-side encoding refusal has no
/// source span to label). If diagnostic construction is refused on resource grounds the bare failure survives, so the
/// error path never makes an undecodable document worse.
fn unsupported(message: &str) -> CodecError {
    // The plain carrier builds fallibly; on refusal the bare failure survives (an undecodable document never gets
    // worse).
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) =
        jqf_source::Diagnostic::try_new(HTML.code("representation"), jqf_source::Severity::Error, message)
    else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Result<String, CodecError> {
        determine_and_decode(bytes)
    }

    #[test]
    fn viewport_meta_then_charset_meta_decodes_utf8() {
        // The first <meta> declares no charset: the prescan must CONTINUE past it and find the later <meta
        // charset="utf-8">.
        let html = b"<meta name=\"viewport\" content=\"width=device-width\">\
                     <meta charset=\"utf-8\"><p>\xF0\x9F\x98\x80</p>";
        let text = decode(html).expect("decodes");
        assert!(text.contains('\u{1F600}'), "UTF-8 body must decode as UTF-8: {text:?}");
    }

    #[test]
    fn uppercase_meta_tag_matches_case_insensitively() {
        let html = b"<META charset=utf-8><p>\xF0\x9F\x98\x80</p>";
        let text = decode(html).expect("decodes");
        assert!(text.contains('\u{1F600}'), "uppercase META must match: {text:?}");
    }

    #[test]
    fn whitespace_padded_charset_label_is_trimmed() {
        // WHATWG strips ASCII whitespace around the label; the padded spelling must decode, not raise
        // UnsupportedRepresentation.
        let html = b"<meta charset=\" utf-8 \"><p>\xF0\x9F\x98\x80</p>";
        let text = decode(html).expect("decodes");
        assert!(
            text.contains('\u{1F600}'),
            "padded label must decode as UTF-8: {text:?}"
        );
    }

    #[test]
    fn content_attribute_charset_prefix_is_trimmed() {
        let html = b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset= utf-8 \">\
                     <p>\xF0\x9F\x98\x80</p>";
        let text = decode(html).expect("decodes");
        assert!(
            text.contains('\u{1F600}'),
            "content charset= must decode as UTF-8: {text:?}"
        );
    }

    #[test]
    fn metadata_tag_does_not_false_match() {
        // <metadata ...> is not a <meta> start tag: no charset is declared, so the single byte 0xE9 falls to the
        // windows-1252 fallback (é), never a lossy UTF-8 U+FFFD.
        let text = decode(b"<metadata charset=utf-8>\xE9").expect("decodes");
        assert_eq!(text, "<metadata charset=utf-8>\u{e9}");
    }

    #[test]
    fn whitespace_only_charset_continues_scanning() {
        let html = b"<meta charset=\"   \"><meta charset=\"utf-8\"><p>\xF0\x9F\x98\x80</p>";
        let text = decode(html).expect("decodes");
        assert!(
            text.contains('\u{1F600}'),
            "whitespace-only charset must continue: {text:?}"
        );
    }

    #[test]
    fn lone_solidus_in_a_meta_tag_terminates() {
        // WHATWG "get an attribute" step 1 skips 0x2F: without that skip the attribute loop never advances and the
        // prescan spins forever.
        assert_eq!(prescan_meta_charset(b"<meta /x>"), None);
    }

    /// Encoding Standard labels are ASCII-case-insensitive.
    #[test]
    fn charset_labels_are_matched_case_insensitively() {
        let html = b"<meta charset=UTF-8><p>\xF0\x9F\x98\x80</p>";
        let text = decode(html).expect("decodes");
        assert!(
            text.contains('\u{1F600}'),
            "UTF-8 label must match case-insensitively: {text:?}"
        );
    }

    /// A commented-out meta charset must not win the prescan.
    #[test]
    fn a_commented_meta_charset_does_not_win_the_prescan() {
        let html = b"<!-- <meta charset=shift_jis> --><meta charset=utf-8><p>\xF0\x9F\x98\x80</p>";
        let text = decode(html).expect("decodes");
        assert!(
            text.contains('\u{1F600}'),
            "commented charset must be skipped: {text:?}"
        );
    }

    /// A prescan label that is not a UTF-8 or windows-1252 alias is refused.
    #[test]
    fn an_unimplemented_charset_label_is_refused() {
        let error = decode(b"<meta charset=shift_jis>").expect_err("shift_jis stays refused");
        assert!(
            matches!(error.kind(), CodecFailureKind::UnsupportedRepresentation),
            "refusal must be UnsupportedRepresentation, got {:?}",
            error.kind()
        );
    }

    #[test]
    fn utf16_boms_are_still_refused() {
        for bom in [&b"\xFF\xFE\x00"[..], &b"\xFE\xFF\x00"[..]] {
            let error = decode(bom).expect_err("UTF-16 BOMs stay refused");
            assert!(
                matches!(error.kind(), CodecFailureKind::UnsupportedRepresentation),
                "refusal must be UnsupportedRepresentation, got {:?}",
                error.kind()
            );
            let diagnostic = error.diagnostic().expect("refusal must carry a named diagnostic");
            assert!(
                diagnostic.message().contains("UTF-16"),
                "refusal must name UTF-16, got {:?}",
                diagnostic.message()
            );
        }
    }
}
