//! The `MessagePack` reader for deferred container spans.
//!
//! A nonzero `lazy_frontier` leaves container payloads below the frontier unbuilt and commits them as span-backed
//! [`ContainerSpan`](jqf_data::NodeSemantic) nodes. When the engine touches such a span, this reader re-decodes the
//! exact validated bytes as one independent `MessagePack` item and materializes it to an owned value. The bytes are
//! valid by construction — the outer scan accepted them — so the re-read is the "second read is budgeted" law, the
//! same one the located span path uses.
//!
//! `MessagePack` is binary: a span is never valid UTF-8 text, so the text arm is refused and the byte arm is the one
//! that runs.

use jqf_codec_core::map_span_materialization_error;
use jqf_data::{DataError, LazySpanMaterializer, Value};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::options::Dialect;

/// Synthetic source identity for the re-read of a deferred span. The span is valid by construction — the outer decode
/// accepted these exact bytes — so no diagnostic ever renders this identity.
const SPAN_SOURCE: SourceRef = SourceRef::new(SourceId::new(0), SourceKind::Input);

/// The installed reader for documents decoded under `messagepack.utf8@1`.
pub(crate) static UTF8_SPAN_MATERIALIZER: MessagepackSpanMaterializer =
    MessagepackSpanMaterializer { dialect: Dialect::Utf8 };

/// The installed reader for documents decoded under `messagepack.wire@1`.
pub(crate) static WIRE_SPAN_MATERIALIZER: MessagepackSpanMaterializer =
    MessagepackSpanMaterializer { dialect: Dialect::Wire };

/// The installed reader for documents decoded under `messagepack.key-equivalence@1`.
pub(crate) static KEY_EQUIVALENCE_SPAN_MATERIALIZER: MessagepackSpanMaterializer = MessagepackSpanMaterializer {
    dialect: Dialect::KeyEquivalence,
};

/// The format-owned reader for one deferred `MessagePack` container.
pub(crate) struct MessagepackSpanMaterializer {
    dialect: Dialect,
}

/// The reader installed on a document decoded under `dialect`.
#[must_use]
pub(crate) fn span_materializer(dialect: Dialect) -> &'static MessagepackSpanMaterializer {
    match dialect {
        Dialect::Utf8 => &UTF8_SPAN_MATERIALIZER,
        Dialect::Wire => &WIRE_SPAN_MATERIALIZER,
        Dialect::KeyEquivalence => &KEY_EQUIVALENCE_SPAN_MATERIALIZER,
    }
}

impl LazySpanMaterializer for MessagepackSpanMaterializer {
    fn materialize_span(&self, _text: &str, _resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        Err(DataError::InvalidDocument)
    }

    fn materialize_span_bytes(&self, bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        let source = ResolvedSource::new(SPAN_SOURCE, "container-span", bytes, 0);
        let document = crate::materialize::decode_span_document(source, self.dialect, resources)
            .map_err(|error| map_span_materialization_error(&error))?;
        document.materialize_root(resources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;
    use jqf_codec_core::{
        AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecFailureKind, CodecRunContext,
        DecodeRequest, DemandClause, DiagnosticPolicy, ValidationMode,
    };
    use jqf_data::DialectId;
    use jqf_source::{SourceId, SourceKind};

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(97), SourceKind::Input),
            "lazy.test",
            bytes,
            0,
        )
    }

    fn requirement(resources: &ResourceContext<'_>, frontier: Option<u32>) -> AccessRequirement {
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
        demand.try_insert(&DemandClause::ValueShape).expect("value shape");
        let requirement = AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement");
        match frontier {
            Some(depth) => requirement.with_lazy_frontier(depth),
            None => requirement,
        }
    }

    fn decode_document<'source>(
        bytes: &'source [u8],
        frontier: Option<u32>,
        resources: &mut ResourceContext<'_>,
    ) -> jqf_data::Document<'source> {
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::MESSAGEPACK_UTF8_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                resources,
            )
            .expect("provider");
        let requirement = requirement(resources, frontier);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, resources).expect("open");
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("decode");
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            panic!("expected a full document");
        };
        product.document().try_clone().expect("document clone")
    }

    /// An array of `n` maps `{id: i, pad: "xxxxxxxxxxxxxxxx"}`.
    fn array_of_id_pad_maps(n: u8) -> Vec<u8> {
        assert!(n < 16, "fixarray count");
        let pad = [b'x'; 16];
        let mut out = Vec::new();
        out.push(0x90 | n);
        for i in 0..n {
            out.push(0x82);
            out.extend_from_slice(&[0xa2, b'i', b'd', i]);
            out.extend_from_slice(&[0xa3, b'p', b'a', b'd', 0xb0]);
            out.extend_from_slice(&pad);
        }
        out
    }

    #[test]
    fn lazy_frontier_defers_element_payloads_on_an_array_of_maps() {
        // The pin program `[.[] | .id] | length` lowers a lazy frontier of 1 (engine default). Depth 1 defers each map;
        // the root array stays built. Element payloads (`id`, `pad`) are not nodes.
        let bytes = array_of_id_pad_maps(8);
        let mut eager_resources = crate::test_support::resources();
        let mut lazy_resources = crate::test_support::resources();
        let eager = decode_document(&bytes, Some(0), &mut eager_resources);
        let lazy = decode_document(&bytes, Some(1), &mut lazy_resources);
        assert_eq!(eager.container_span_count(), 0, "frontier 0 builds every container");
        assert_eq!(
            lazy.container_span_count(),
            8,
            "frontier 1 defers each of the eight maps"
        );
        assert!(
            lazy.node_count() < eager.node_count(),
            "deferral must build fewer nodes (lazy {} vs eager {})",
            lazy.node_count(),
            eager.node_count(),
        );
        // Eager: 1 array + 8 maps + 8 integers + 8 strings = 25. Lazy: 1 array + 8 map spans = 9.
        assert_eq!(eager.node_count(), 25);
        assert_eq!(lazy.node_count(), 9);
        let root = lazy.root_handle();
        let array = lazy
            .value_view(root)
            .expect("root view")
            .array()
            .expect("array projection")
            .expect("built root array");
        assert_eq!(array.len(), 8);
        for item in array.iter() {
            assert!(
                item.is_container_span().expect("span check"),
                "each array element is a deferred map"
            );
        }
    }

    #[test]
    fn a_lazy_array_of_maps_materializes_the_eager_value() {
        let bytes = array_of_id_pad_maps(4);
        let mut eager_resources = crate::test_support::resources();
        let mut lazy_resources = crate::test_support::resources();
        let eager = decode_document(&bytes, Some(0), &mut eager_resources)
            .materialize_root(&mut eager_resources)
            .expect("eager materialize");
        let lazy = decode_document(&bytes, Some(1), &mut lazy_resources)
            .materialize_root(&mut lazy_resources)
            .expect("lazy materialize");
        assert_eq!(
            format!("{eager:?}"),
            format!("{lazy:?}"),
            "touching every deferred map recovers the eager value"
        );
    }

    #[test]
    fn a_scalar_root_ignores_the_frontier() {
        let mut resources = crate::test_support::resources();
        let document = decode_document(&[0x01], Some(1), &mut resources);
        assert_eq!(document.container_span_count(), 0);
        assert_eq!(document.node_count(), 1);
    }

    #[test]
    fn a_nested_reserved_byte_fails_the_scan_under_the_frontier() {
        // Array of one map whose value is the reserved 0xc1 byte: the validating scan still visits every item, so the
        // frontier cannot hide a grammar fault.
        let bytes = [0x91, 0x81, 0xa1, b'a', 0xc1];
        let mut resources = crate::test_support::resources();
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(&bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::MESSAGEPACK_UTF8_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        let requirement = requirement(&resources, Some(1));
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let error = session.decode(&mut run).expect_err("reserved byte");
        assert_eq!(error.kind(), CodecFailureKind::InvalidInput);
    }
}
