//! The cbor-seq codec: RFC 8742 concatenated CBOR items, an adjacent-value format over the `cbor` payload codec.
//!
//! RFC 8742 framing is NO framing: items are concatenated with no delimiter, no terminator, no blank unit, and no
//! physical unit distinct from the value. So cbor-seq is not a record framer: it declares
//! [`RouteCapability::AdjacentValues`] and rides the SDK's adjacent-value drive, whose cut is the CBOR decoder's own
//! cursor — the decoder stops at the item's end and reports the real consumed offset. The drive's value-separator set
//! must be EMPTY for cbor-seq, because `0x20` is a complete CBOR item (`-1`), never insignificant whitespace: the
//! default RFC 8259 set would silently DROP items.
//!
//! The registered surface is deliberately narrow: one input dialect, `cbor-seq.rfc8742-generic@1` (the payload law is
//! cbor's generic dialect, the framing law is RFC 8742), and one output dialect, `cbor-seq.jqf@1`, carrying the payload
//! profile as an option. There is NO recovering dialect, and there never can be: with no delimiter, the next item's
//! start is knowable only by having decoded the previous one, so a malformed item leaves no resynchronization point.
//! Registration advertises only executable operations — withheld under portfolio §3.12.
//!
//! The format's media type is `application/cbor-seq` (RFC 8742 §3.1), recorded here for reference; the CLI-facing
//! extensions are `cborseq` and `cbors`.
//!
//! The route table is CBOR'S OWN whole + located pair: slot 0 Whole/`CompleteDocument`, slot 1 Exact/`Located`. Both
//! stop at one top-level item under the adjacent opt-in.

mod options;
mod render;

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner,
    RegistrationError, RouteCapability,
};
use jqf_data::{DialectIdRef, FormatIdRef};

const FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(options::FORMAT_ID);
// The descriptor's dialect set is what the catalog matches: the generic input identity and the jqf output identity. A
// recovering dialect is deliberately absent (recovery is impossible in principle).
const DIALECTS: [DialectIdRef<'static>; 2] = [
    DialectIdRef::from_static(options::RFC8742_GENERIC_DIALECT_ID),
    DialectIdRef::from_static(options::JQF_DIALECT_ID),
];

/// The CLI-facing routes the cbor-seq registration serves: the adjacent-value input model only (never a record route
/// — there is no framing to frame). `pub(crate)` for the ruling pin in `crate::tests`.
pub(crate) const ROUTES: [RouteCapability; 1] = [RouteCapability::AdjacentValues];

/// Registers the `cbor-seq` format's decode and encode sides.
///
/// Decode is the CBOR access provider opened under the adjacent opt-in — cbor's own factory, with no seq-specific
/// code on the decode path (the request itself carries `allow_adjacent_values: true` and the empty value-separator
/// set). Encode is the cbor encoder at the request's payload profile (see [`render`]). Both halves live through the
/// descriptor's dialect list, so the catalog matches `cbor-seq.rfc8742-generic@1` to this registration and never to
/// `cbor`'s.
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FORMAT,
            &DIALECTS,
            CodecOperations::new(true, true, false),
            &ROUTES,
            &["cborseq", "cbors"],
            // Each item is one self-framed binary item; the facade appends nothing.
            &[ItemByteOwner::Codec, ItemByteOwner::Codec],
            &[],
            // No insignificant inter-value bytes: every byte reaches the decoder.
            &[],
        ),
        Some(DecoderFactoryRecord::new(crate::decode::create_provider)),
        Some(EncoderFactoryRecord::new(render::create_registered_factory)),
        None,
        None,
    )
}

pub use options::{CborSeqEncodeOptions, CborSeqPayloadProfile, FORMAT_ID, JQF_DIALECT_ID, RFC8742_GENERIC_DIALECT_ID};

#[cfg(test)]
mod tests {
    use super::DIALECTS;

    #[test]
    fn the_registration_dialect_set_has_no_duplicates() {
        let mut seen: alloc::vec::Vec<&str> = alloc::vec::Vec::new();
        for dialect in DIALECTS.iter().map(|d| d.as_str()) {
            assert!(
                !seen.contains(&dialect),
                "dialect {dialect} appears twice in the cbor-seq set"
            );
            seen.push(dialect);
        }
    }
}
