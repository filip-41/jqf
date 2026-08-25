//! json-seq output: one codec-owned RS prefix and LF suffix per item, atomically.
//!
//! Rendering an item's payload is strict JSON's job and is delegated whole to
//! [`crate::encode::create_prefixed_framed_factory`]. This module owns only the framing POLICY: the RS (0x1E) prefix,
//! the LF (or `-j`/raw0 replacement) suffix, and that both join the payload inside the encoder's own staging buffer —
//! so an item can never be published without its frame and an item that failed mid-encode can never publish one. The
//! prefix is suppressed for a root string the `-r` raw arm writes verbatim — the reference's `--seq` law, which lives
//! in the JSON encoder's prefix seam.

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeRequest, ErasedEncoderFactory};
use jqf_resource::ResourceContext;

use super::provider::ENCODE_PHYSICAL_ROUTE_ID;
use super::{FORMAT_ID, JQF_DIALECT_ID, JsonSeqEncodeOptions, JsonSeqSuffix};

/// Registry entry point: reads the suffix policy from the request's own option payload, defaulting to LF when options
/// are omitted.
pub(crate) fn create_registered_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    let options = match request.options {
        None => JsonSeqEncodeOptions::default(),
        Some(payload) => *payload
            .downcast_ref::<JsonSeqEncodeOptions>()
            .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))?,
    };
    if request.format.as_str() != FORMAT_ID || request.dialect.as_str() != JQF_DIALECT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let suffix: &'static [u8] = match options.suffix {
        JsonSeqSuffix::Lf => b"\n",
        JsonSeqSuffix::NoSuffix => b"",
        JsonSeqSuffix::Nul => b"\0",
    };
    // The request's options payload is the json-seq schema; the JSON seam reads the render style from the caller
    // instead of the request, which is what lets a json-seq request carry its own options without the JSON codec
    // misreading them.
    let mut request = request;
    request.options = None;
    crate::encode::create_prefixed_framed_factory(
        request,
        b"\x1e",
        suffix,
        ENCODE_PHYSICAL_ROUTE_ID,
        options.json,
        resources,
    )
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use jqf_codec_core::{
        CodecRunContext, DiagnosticPolicy, EncodeItem, EncodeRequest, PreservationRequest, VecByteSink,
    };
    use jqf_data::{DialectId, FormatId, Number, Shared, Value};

    use super::create_registered_factory;
    use super::{FORMAT_ID, JQF_DIALECT_ID, JsonSeqEncodeOptions, JsonSeqSuffix};
    use crate::{JsonEncodeOptions, test_support};

    /// Encodes each owned value as its own item through THIS module's factory (the record drive's
    /// one-session-per-published-item shape) and returns the concatenated bytes.
    fn framed_bytes(json: JsonEncodeOptions, suffix: JsonSeqSuffix, items: &[Value]) -> Vec<u8> {
        let options = JsonSeqEncodeOptions::new(json, suffix);
        let format = FormatId::try_new(FORMAT_ID).expect("format");
        let dialect = DialectId::try_new(JQF_DIALECT_ID).expect("dialect");
        let request = EncodeRequest {
            format: &format,
            dialect: &dialect,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            options: Some(&options as &(dyn core::any::Any + Send + Sync)),
        };
        let mut resources = test_support::resources();
        let factory = create_registered_factory(request, &mut resources).expect("factory");
        let mut out = Vec::new();
        for item in items {
            let mut session = factory
                .start(EncodeItem::Owned(item), PreservationRequest::None, &mut resources)
                .expect("session");
            let mut sink = VecByteSink::new(&mut out);
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            session.encode(&mut sink, &mut run).expect("encode");
        }
        out
    }

    fn number(spelling: &str) -> Value {
        Value::Number(Number::try_json_literal(spelling).expect("literal"))
    }

    #[test]
    fn every_item_including_the_last_carries_its_rs_prefix_and_lf_suffix() {
        // RFC 7464 framing is per ITEM and lives inside the encoder's own staging buffer, so the FINAL item keeps both
        // its RS prefix and its LF suffix — nothing downstream can drop or add a frame.
        let items = [number("1"), number("2"), number("3")];
        assert_eq!(
            framed_bytes(JsonEncodeOptions::default(), JsonSeqSuffix::Lf, &items).as_slice(),
            b"\x1e1\n\x1e2\n\x1e3\n"
        );
    }

    #[test]
    fn a_raw_root_string_is_written_with_no_rs_prefix() {
        // The reference's `--seq -r` law: a root string the raw arm prints verbatim carries NO RS prefix (the LF suffix
        // still applies), while raw-off frames the quoted spelling like any other item.
        let hi = Value::String(Shared::<str>::try_from_str("hi").expect("string"));
        let items = [hi];
        let raw = JsonEncodeOptions {
            raw_strings: true,
            ..JsonEncodeOptions::default()
        };
        assert_eq!(framed_bytes(raw, JsonSeqSuffix::Lf, &items).as_slice(), b"hi\n");
        assert_eq!(
            framed_bytes(JsonEncodeOptions::default(), JsonSeqSuffix::Lf, &items).as_slice(),
            b"\x1e\"hi\"\n"
        );
    }
}
