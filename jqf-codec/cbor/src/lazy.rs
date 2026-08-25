//! The CBOR reader for deferred container spans.
//!
//! The source-retention path (the `cbor.source@1` output profile) commits containers as span-backed
//! [`ContainerSpan`](jqf_data::NodeSemantic) nodes instead of building their subtrees. When the engine touches such a
//! span, this reader re-decodes the exact validated bytes as one independent CBOR item and materializes it to an owned
//! value. The bytes are valid by construction — the decode's validating scan accepted them — so the re-read is the
//! "second read is budgeted" law, the same one the span routes use.

use jqf_codec_core::map_span_materialization_error;
use jqf_data::{DataError, LazySpanMaterializer, Value};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// Synthetic source identity for the re-read of a deferred span. The span is valid by construction — the outer decode
/// accepted these exact bytes — so no diagnostic ever renders this identity.
const SPAN_SOURCE: SourceRef = SourceRef::new(SourceId::new(0), SourceKind::Input);

/// The installed reader for every CBOR document that retains source spans.
pub(crate) static CBOR_SPAN_MATERIALIZER: CborSpanMaterializer = CborSpanMaterializer;

pub(crate) struct CborSpanMaterializer;

impl LazySpanMaterializer for CborSpanMaterializer {
    // The engine reaches a deferred span only through the BYTES arm below (spans wrap whole CBOR items, never decoded
    // text), so the trait's text arm has no caller for this format; it refuses rather than pretend to implement one.
    fn materialize_span(&self, _text: &str, _resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        Err(DataError::InvalidDocument)
    }

    fn materialize_span_bytes(&self, bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        let source = ResolvedSource::new(SPAN_SOURCE, "container-span", bytes, 0);
        let document = crate::parse::decode_span_document(source, 0, bytes.len(), resources)
            .map_err(|error| map_span_materialization_error(&error))?;
        document.materialize_root(resources)
    }
}
