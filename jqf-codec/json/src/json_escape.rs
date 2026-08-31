//! The JSON string escape law this codec owns.
//!
//! Quote, backslash, `\b \f \n \r \t`, remaining C0 as `\u00XX`, and DEL as `\u007f`. Everything else (UTF-8 included)
//! passes through raw.

use alloc::vec::Vec;

/// One byte's escape under this crate's JSON string law: `(length, text)` for a byte that must be escaped — `\"`,
/// `\\`, the `\b \f \n \r \t` short forms, every C0 control as `\u00XX`, and DEL as `\u007f` — and a zero length for
/// a byte that passes through raw (UTF-8 included).
#[must_use]
pub fn json_escape_byte(byte: u8) -> (u8, [u8; 6]) {
    JSON_ESCAPE[usize::from(byte)]
}

/// The six JSON simple escapes plus solidus: the unescape twin of [`json_escape_byte`]'s short forms. `None` for `\u`
/// and every non-escape byte.
#[must_use]
pub(crate) fn json_simple_unescape(byte: u8) -> Option<char> {
    match byte {
        b'"' => Some('"'),
        b'\\' => Some('\\'),
        b'/' => Some('/'),
        b'b' => Some('\u{8}'),
        b'f' => Some('\u{c}'),
        b'n' => Some('\n'),
        b'r' => Some('\r'),
        b't' => Some('\t'),
        _ => None,
    }
}

const fn escape_entry(byte: u8) -> (u8, [u8; 6]) {
    match byte {
        b'"' => (2, [b'\\', b'"', 0, 0, 0, 0]),
        b'\\' => (2, [b'\\', b'\\', 0, 0, 0, 0]),
        0x08 => (2, [b'\\', b'b', 0, 0, 0, 0]),
        0x09 => (2, [b'\\', b't', 0, 0, 0, 0]),
        0x0a => (2, [b'\\', b'n', 0, 0, 0, 0]),
        0x0c => (2, [b'\\', b'f', 0, 0, 0, 0]),
        0x0d => (2, [b'\\', b'r', 0, 0, 0, 0]),
        0x00..=0x1f | 0x7f => {
            const HEX: [u8; 16] = *b"0123456789abcdef";
            (
                6,
                [
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[((byte >> 4) & 15) as usize],
                    HEX[(byte & 15) as usize],
                ],
            )
        }
        _ => (0, [0; 6]),
    }
}

const JSON_ESCAPE: [(u8, [u8; 6]); 256] = {
    let mut table = [(0_u8, [0_u8; 6]); 256];
    let mut i = 0_u8;
    loop {
        table[i as usize] = escape_entry(i);
        if i == 255 {
            break;
        }
        i += 1;
    }
    table
};

/// Appends `bytes` escaped for a JSON string body; see [`json_escape_byte`]. The quote, the backslash, every C0 control
/// (short forms `\b \f \n \r \t`, the rest as `\u00XX`), and DEL as `\u007f`; everything else (UTF-8 included) passes
/// through raw.
pub fn push_json_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &byte in bytes {
        let (len, escape) = json_escape_byte(byte);
        if len > 0 {
            out.extend_from_slice(&escape[..len as usize]);
        } else {
            out.push(byte);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{json_escape_byte, json_simple_unescape};

    #[test]
    fn json_simple_unescape_covers_the_short_forms() {
        assert_eq!(json_simple_unescape(b'"'), Some('"'));
        assert_eq!(json_simple_unescape(b'\\'), Some('\\'));
        assert_eq!(json_simple_unescape(b'/'), Some('/'));
        assert_eq!(json_simple_unescape(b'b'), Some('\u{8}'));
        assert_eq!(json_simple_unescape(b'f'), Some('\u{c}'));
        assert_eq!(json_simple_unescape(b'n'), Some('\n'));
        assert_eq!(json_simple_unescape(b'r'), Some('\r'));
        assert_eq!(json_simple_unescape(b't'), Some('\t'));
        assert_eq!(json_simple_unescape(b'u'), None);
        assert_eq!(json_simple_unescape(0x7f), None);
    }

    /// The table is the law: every byte's escape matches the documented short forms, C0 `\u00XX`, and DEL `\u007f`.
    #[test]
    fn json_escape_table_matches_the_reference_law() {
        for byte in 0_u8..=u8::MAX {
            let (len, text) = json_escape_byte(byte);
            let expected: &[u8] = match byte {
                b'"' => b"\\\"",
                b'\\' => b"\\\\",
                0x08 => b"\\b",
                0x09 => b"\\t",
                0x0a => b"\\n",
                0x0c => b"\\f",
                0x0d => b"\\r",
                0x00..=0x1f | 0x7f => {
                    const HEX: &[u8; 16] = b"0123456789abcdef";
                    let hex = [
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[((byte >> 4) & 15) as usize],
                        HEX[(byte & 15) as usize],
                    ];
                    assert_eq!(len, 6, "byte {byte:#04x}");
                    assert_eq!(&text[..6], &hex, "byte {byte:#04x}");
                    continue;
                }
                _ => {
                    assert_eq!(len, 0, "byte {byte:#04x} passes through");
                    continue;
                }
            };
            assert_eq!(len as usize, expected.len(), "byte {byte:#04x}");
            assert_eq!(&text[..expected.len()], expected, "byte {byte:#04x}");
        }
    }

    #[test]
    fn jqft_escape_table_matches_json() {
        for byte in 0_u8..=u8::MAX {
            assert_eq!(
                json_escape_byte(byte),
                jqf_codec_jqft::json_escape_byte(byte),
                "byte {byte:#04x}"
            );
        }
    }
}
