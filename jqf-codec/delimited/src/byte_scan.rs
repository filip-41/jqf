//! The delimited-codec byte-scan surface: the CSV-family stop sets this crate owns, driven through the
//! architecture-width kernels shared with the codec family (`jqf-codec-core::byte_scan`).

pub use jqf_codec_core::byte_scan::{NdjsonFrame, StopSet, prefix_len};

/// The comma-delimited CSV field/record terminators: `,`, `"`, `\r`, `\n` (the RFC 4180 quote law means a quote-free
/// record is one scan).
#[derive(Clone, Copy)]
pub struct Csv;
impl StopSet for Csv {
    const EQ: [u8; 8] = [b',', b'"', b'\r', b'\n', 0, 0, 0, 0];
    const EQ_LEN: u8 = 4;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// The tab-delimited CSV variant's terminators: `\t`, `"`, `\r`, `\n`.
#[derive(Clone, Copy)]
pub struct CsvTab;
impl StopSet for CsvTab {
    const EQ: [u8; 8] = [b'\t', b'"', b'\r', b'\n', 0, 0, 0, 0];
    const EQ_LEN: u8 = 4;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// CSV *record* framing stops: `"`, `\r`, `\n`. Framing does not care about the field delimiter — a comma is ordinary
/// payload — so the scan covers a whole unquoted row in one call instead of restarting at every field.
#[derive(Clone, Copy)]
pub struct CsvFrame;
impl StopSet for CsvFrame {
    const EQ: [u8; 8] = [b'"', b'\r', b'\n', 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 3;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

#[cfg(test)]
mod tests {
    use super::{Csv, CsvFrame, CsvTab, StopSet, prefix_len};
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

    /// The alignment oracle for this crate's stop sets: the wide kernel must agree with each set's scalar predicate at
    /// every alignment and length, so a wrong kernel is a test failure here.
    #[test]
    fn stop_sets_agree_with_their_scalar_predicates_at_every_alignment() {
        let mut corpus: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"\"".to_vec(),
            b"a,b\"c\r\nd".to_vec(),
            b"a\tb\"c\r\nd".to_vec(),
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
                    0 => b",\t\"\r\n"[((r >> 8) % 5) as usize],
                    _ => ((r >> 16) & 0xFF) as u8,
                });
            }
            corpus.push(bytes);
        }
        for bytes in &corpus {
            check_alignment::<Csv>(bytes);
            check_alignment::<CsvTab>(bytes);
            check_alignment::<CsvFrame>(bytes);
        }
    }
}
