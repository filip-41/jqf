//! Physical NDJSON boundary discovery over contiguous retained input.
//!
//! A JSON string cannot contain a raw line feed or carriage return, so record boundaries need no quote-state machine at
//! all: the first LF or CR at or after the record's start decides the record's fate. One scan finds both, which is what
//! keeps framing at well under 1 % of the lane (design doc §7.1's revised per-record budget).

use jqf_codec_core::byte_scan::{NdjsonFrame, prefix_len};

/// What the first framing byte at or after a record's start proves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Frame {
    /// A line feed terminates the record; `lf` is its index.
    Lf { lf: usize },
    /// A carriage return immediately followed by a line feed terminates the record; `cr` is the carriage return's
    /// index.
    CrLf { cr: usize },
    /// A carriage return that is NOT immediately followed by a line feed: a framing fault at `cr`. Strict JSON
    /// accepting CR as whitespace does not weaken this rule — CR is a PHYSICAL byte here.
    BareCr { cr: usize },
    /// No framing byte before end of input: the record is unterminated.
    Unterminated,
}

const LF: u8 = b'\n';

/// Index of the first `LF` or `CR` in `bytes`, or `None`.
fn find_framing_byte(bytes: &[u8]) -> Option<usize> {
    let admitted = prefix_len::<NdjsonFrame>(bytes);
    (admitted < bytes.len()).then_some(admitted)
}

/// Classifies the record that starts at `start` within `bytes`.
pub(crate) fn frame_at(bytes: &[u8], start: usize) -> Frame {
    let Some(found) = find_framing_byte(&bytes[start..]) else {
        return Frame::Unterminated;
    };
    let index = start + found;
    if bytes[index] == LF {
        return Frame::Lf { lf: index };
    }
    if bytes.get(index + 1) == Some(&LF) {
        Frame::CrLf { cr: index }
    } else {
        Frame::BareCr { cr: index }
    }
}

#[cfg(test)]
mod tests {
    use super::{Frame, frame_at};

    #[test]
    fn a_line_feed_terminates_a_record() {
        assert_eq!(frame_at(b"{\"a\":1}\n{}", 0), Frame::Lf { lf: 7 });
    }

    #[test]
    fn a_carriage_return_before_the_line_feed_is_part_of_the_terminator() {
        assert_eq!(frame_at(b"{\"a\":1}\r\n", 0), Frame::CrLf { cr: 7 });
    }

    #[test]
    fn any_other_carriage_return_is_a_framing_fault_at_its_own_offset() {
        assert_eq!(frame_at(b"{\"a\":1}\r{}\n", 0), Frame::BareCr { cr: 7 });
        assert_eq!(frame_at(b"\r", 0), Frame::BareCr { cr: 0 });
    }

    #[test]
    fn no_framing_byte_leaves_the_record_unterminated() {
        assert_eq!(frame_at(b"{\"a\":1}", 0), Frame::Unterminated);
        assert_eq!(frame_at(b"", 0), Frame::Unterminated);
    }

    #[test]
    fn the_word_scan_agrees_with_a_byte_scan_across_every_alignment() {
        // The word-at-a-time loop must never skip a framing byte that a naive scan would find, at any offset within a
        // word.
        let len = 64;
        for position in 0..len {
            let mut input = [b'x'; 64];
            input[position] = b'\n';
            assert_eq!(frame_at(&input, 0), Frame::Lf { lf: position });
            input[position] = b'\r';
            assert_eq!(frame_at(&input, 0), Frame::BareCr { cr: position });
        }
    }
}
