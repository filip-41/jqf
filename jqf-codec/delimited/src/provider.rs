//! The RFC 4180 record-stream provider and its single advertised route.
//!
//! The provider opens one framer session per source. The framer charges NO input bytes — the payload provider the
//! consumer opens over the same retained source owns the request's single input charge (the law at
//! [`create_record_provider`]). Sibling: the payload decode route (`decode.rs`), which narrows the same retained source
//! per record range.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessResultKind, CodecError, CodecFailureKind, DiagnosticPolicy,
    ErasedRecordStreamProvider, ErasedRecordStreamSession, PhysicalRouteId, ProviderInput, RECORD_ROUTE_SLOT,
    RecordProviderOpen, RecordStreamProvider, RouteDescription, RouteSlot,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use super::stream::CsvRecordSession;
use crate::CsvDecodeOptions;

/// Stable physical identity of serial RFC 4180 record framing.
pub(crate) const RECORD_PHYSICAL_ROUTE_ID: PhysicalRouteId = match PhysicalRouteId::derive(crate::FORMAT_ID, 4, 1) {
    Some(id) => id,
    None => panic!("nonzero route identity"),
};

/// Stable physical identity of deterministic RFC 4180 encoding.
pub(crate) const ENCODE_PHYSICAL_ROUTE_ID: PhysicalRouteId = match PhysicalRouteId::derive(crate::FORMAT_ID, 3, 1) {
    Some(id) => id,
    None => panic!("nonzero route identity"),
};

pub(crate) struct CsvRecordProvider {
    routes: Vec<RouteDescription>,
    options: CsvDecodeOptions,
}

impl CsvRecordProvider {
    pub(crate) fn try_new(
        options: CsvDecodeOptions,
        diagnostics: DiagnosticPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::strict(diagnostics);
        let routes = RouteDescription::try_table(
            &[(
                RECORD_ROUTE_SLOT,
                // A record stream frames the WHOLE retained source; it selects nothing, so its footprint is Whole and
                // its result is the record-stream kind and nothing else.
                AccessFootprintKind::Whole,
                AccessResultKind::RecordStream,
            )],
            guarantees,
            resources,
        )?;
        Ok(Self { routes, options })
    }
}

impl RecordStreamProvider for CsvRecordProvider {
    fn record_route_descriptions(&self) -> &[RouteDescription] {
        self.routes.as_slice()
    }

    fn open_record_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        provider_id: u64,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedRecordStreamSession<'source>, CodecError> {
        if slot != RECORD_ROUTE_SLOT {
            return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
        }
        let options = self.options;
        ErasedRecordStreamSession::try_new_with_route::<CsvRecordSession, _>(
            input.source(),
            RECORD_PHYSICAL_ROUTE_ID,
            provider_id,
            slot,
            || Ok(CsvRecordSession::new(options)),
        )
    }
}

/// The registered record-provider factory: downcasts the codec- neutral open envelope to this codec's own grammar
/// options and opens the typed provider.
pub(crate) fn create_registered_provider<'source>(
    source: ResolvedSource<'source>,
    open: RecordProviderOpen,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedRecordStreamProvider<'source>, CodecError> {
    let RecordProviderOpen::Delimited {
        delimiter,
        header,
        quote,
        max_record_bytes,
    } = open
    else {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    };
    let options = CsvDecodeOptions::try_from_open(delimiter, header, quote, max_record_bytes)?;
    create_record_provider(source, options, DiagnosticPolicy::ErrorsOnly, resources)
}

/// Opens one RFC 4180 record-stream provider over contiguous retained input.
///
/// The framer charges NO input bytes: the payload provider the consumer opens over the same retained source owns the
/// request's single input charge, and charging twice would make a legal stream fail its own ceiling (module doc owns
/// this law).
pub(crate) fn create_record_provider<'source>(
    source: ResolvedSource<'source>,
    options: CsvDecodeOptions,
    diagnostics: DiagnosticPolicy,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::ErasedRecordStreamProvider<'source>, CodecError> {
    jqf_codec_core::ErasedRecordStreamProvider::try_new_provider::<CsvRecordProvider, _>(source, || {
        CsvRecordProvider::try_new(options, diagnostics, resources)
    })
}
