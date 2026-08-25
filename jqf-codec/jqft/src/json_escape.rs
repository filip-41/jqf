//! jqft's copy of the JSON string escape table.
//!
//! Quote, backslash, `\b \f \n \r \t`, remaining C0 as `\u00XX`, and DEL as `\u007f`. Everything else (UTF-8 included)
//! passes through raw.
//!
//! The owner is `jqf-codec-json`. This crate does not depend on that crate; json's `jqft_escape_table_matches_json`
//! receipt byte-compares both tables.

use alloc::vec::Vec;

/// One byte's escape under the JSON string law. Public so json's receipt can compare this copy to the owner table.
#[must_use]
pub fn json_escape_byte(byte: u8) -> (u8, [u8; 6]) {
    JSON_ESCAPE[usize::from(byte)]
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

/// Appends `bytes` escaped for a JSON string body.
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
