//! `MessagePack` decoder provider factory.

use jqf_codec_core::{CodecError, CodecFailureKind, DecodeRequest, ErasedProvider};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::options::Dialect;
use crate::provider::MessagepackProvider;

/// The decoder entry point: the request must name one of the two registered input dialects (`utf8@1` advertised,
/// `wire@1` registered-unadvertised), keep strict defaults, and refuse the adjacent-value contract (one `MessagePack`
/// object per source).
pub(crate) fn create_provider<'source>(
    source: ResolvedSource<'source>,
    request: DecodeRequest<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, CodecError> {
    request.expect_strict_defaults()?;
    if request.allow_adjacent_values {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let dialect = match request.dialect.as_str() {
        crate::MESSAGEPACK_UTF8_DIALECT_ID => Dialect::Utf8,
        crate::MESSAGEPACK_WIRE_DIALECT_ID => Dialect::Wire,
        crate::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID => Dialect::KeyEquivalence,
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    let provider = MessagepackProvider::try_new(dialect, request.diagnostics, resources)?;
    ErasedProvider::try_new_provider(source, resources, || Ok(provider))
}

#[cfg(test)]
mod tests {
    use std::format;
    use std::string::String;

    use jqf_codec_core::{CodecRunContext, DecodeRequest, DiagnosticPolicy, ValidationMode};
    use jqf_data::DialectId;
    use jqf_resource::ResourceContext;
    use jqf_source::ResolvedSource;

    fn try_open(bytes: &[u8], dialect: &str) -> Result<(), String> {
        let mut resources = crate::test_support::resources();
        let source = ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(97), jqf_source::SourceKind::Input),
            "decode.test",
            bytes,
            0,
        );
        let registration = crate::registration().map_err(|error| format!("{error:?}"))?;
        registration
            .decoder()
            .expect("decoder factory")
            .create_provider(
                source,
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(dialect).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .map(|_| ())
            .map_err(|error| format!("{error:?}"))
    }

    /// The adjacent-value law pinned, transcribed from cbor's `the_adjacent_value_ruling_is_pinned`: the plain
    /// `messagepack` format stays a single-document format, so the decode side REFUSES the adjacent-value opt-in with
    /// `RequirementMismatch` (the registration declares no adjacent-value route — pinned in `lib.rs`).
    #[test]
    fn the_adjacent_value_ruling_is_pinned() {
        let mut resources = crate::test_support::resources();
        let source = ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(97), jqf_source::SourceKind::Input),
            "decode.test",
            &[0x01],
            0,
        );
        let registration = crate::registration().expect("registration");
        let result = registration
            .decoder()
            .expect("decoder factory")
            .create_provider(
                source,
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::MESSAGEPACK_UTF8_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: true,
                    value_separator: &[],
                },
                &mut resources,
            )
            .map_err(|error| error.kind());
        match result {
            Err(jqf_codec_core::CodecFailureKind::RequirementMismatch) => {}
            other => panic!("expected RequirementMismatch, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_dialect_is_refused() {
        let error = try_open(&[0x01], "messagepack.no-such@1").expect_err("unknown dialect");
        assert!(error.contains("RequirementMismatch"), "{error}");
    }

    #[test]
    fn both_registered_dialects_open() {
        try_open(&[0x01], crate::MESSAGEPACK_UTF8_DIALECT_ID).expect("utf8@1 opens");
        try_open(&[0x01], crate::MESSAGEPACK_WIRE_DIALECT_ID).expect("wire@1 opens");
        try_open(&[0x01], crate::MESSAGEPACK_KEY_EQUIVALENCE_DIALECT_ID).expect("key-equivalence@1 opens");
    }

    /// The whole-document access requirement every smoke drives.
    fn whole_requirement(resources: &ResourceContext<'_>) -> jqf_codec_core::AccessRequirement {
        use jqf_codec_core::{AccessGuarantees, AccessRequirement, CodecDemand, DemandClause};
        let mut demand = CodecDemand::try_new(resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
        demand.try_insert(&DemandClause::ValueShape).expect("value shape");
        AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            resources,
        )
        .expect("requirement")
    }

    #[test]
    fn the_whole_document_path_decodes() {
        let mut resources = crate::test_support::resources();
        let source = ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(97), jqf_source::SourceKind::Input),
            "decode.test",
            &[0x81, 0xa1, b'a', 0x01],
            0,
        );
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder factory")
            .create_provider(
                source,
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
        let requirement = whole_requirement(&resources);
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(4_096);
            let result = session.decode(&mut run).expect("decode");
            let jqf_codec_core::AccessOutcome::FullDocument(_) = result.outcome() else {
                panic!("expected a full document");
            };
        }
    }
}
