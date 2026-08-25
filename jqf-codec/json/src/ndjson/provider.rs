//! The NDJSON record-stream provider and its single advertised route.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessGuarantees, CodecError, CodecFailureKind, DiagnosticPolicy, ErasedRecordStreamProvider,
    ErasedRecordStreamSession, PhysicalRouteId, ProviderInput, RecordProviderOpen, RecordStreamProvider,
    RouteDescription, RouteSlot, ValidationMode,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use super::stream::NdjsonRecordSession;
use super::{NdjsonDecodeOptions, NdjsonProfile};
use crate::record_route::{check_profile_matches_request, check_slot, record_route_table};

/// Stable physical identity of serial strict NDJSON record framing.
const STRICT_RECORD_PHYSICAL_ROUTE_ID: PhysicalRouteId = PhysicalRouteId::derive_or_panic(super::FORMAT_ID, 4, 1);

/// Stable physical identity of serial recovering NDJSON record framing.
const RECOVERING_RECORD_PHYSICAL_ROUTE_ID: PhysicalRouteId = PhysicalRouteId::derive_or_panic(super::FORMAT_ID, 5, 1);

/// Stable physical identity of framed NDJSON encoding: the crate-root record-encoder identity, re-exported so the
/// derivation lives in exactly one place.
pub(crate) use crate::ENCODE_PHYSICAL_ROUTE_ID;

pub(crate) struct NdjsonRecordProvider {
    routes: Vec<RouteDescription>,
    profile: NdjsonProfile,
    options: NdjsonDecodeOptions,
}

impl NdjsonRecordProvider {
    pub(crate) fn try_new(
        profile: NdjsonProfile,
        options: NdjsonDecodeOptions,
        diagnostics: DiagnosticPolicy,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, CodecError> {
        let guarantees = AccessGuarantees::new(profile.validation(), diagnostics);
        let routes = record_route_table(guarantees, resources)?;
        Ok(Self {
            routes,
            profile,
            options,
        })
    }

    /// Physical route identity this provider's profile executes.
    pub(crate) const fn physical_route(profile: NdjsonProfile) -> PhysicalRouteId {
        match profile {
            NdjsonProfile::Strict => STRICT_RECORD_PHYSICAL_ROUTE_ID,
            NdjsonProfile::Recovering => RECOVERING_RECORD_PHYSICAL_ROUTE_ID,
        }
    }
}

impl RecordStreamProvider for NdjsonRecordProvider {
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
        check_slot(slot)?;
        let profile = self.profile;
        let options = self.options;
        ErasedRecordStreamSession::try_new_with_route::<NdjsonRecordSession, _>(
            input.source(),
            Self::physical_route(profile),
            provider_id,
            slot,
            || Ok(NdjsonRecordSession::new(profile, options)),
        )
    }
}

/// The registered record-provider factory: downcasts the codec- neutral open envelope to this codec's own profile and
/// options and opens the typed provider.
pub(crate) fn create_registered_provider<'source>(
    source: ResolvedSource<'source>,
    open: RecordProviderOpen,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedRecordStreamProvider<'source>, CodecError> {
    let RecordProviderOpen::Ndjson {
        recovering,
        max_record_bytes,
    } = open
    else {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    };
    let profile = NdjsonProfile::from_recovering(recovering);
    // The envelope is normalized against the ceiling it carries, so this `try_new` can only catch zero. The runtime
    // pre-normalizes both against the INPUT ceiling (`jqf-runtime/src/records`), which this provider cannot see — the
    // shrink-only law is trusted from there.
    let options = NdjsonDecodeOptions::try_new(Some(max_record_bytes), max_record_bytes)?;
    create_record_provider(
        source,
        profile,
        options,
        DiagnosticPolicy::ErrorsOnly,
        profile.validation(),
        resources,
    )
}

/// Opens one NDJSON record-stream provider over contiguous retained input.
///
/// The framer charges NO input bytes: the payload provider the consumer opens over the same retained source owns the
/// request's single input charge, and charging twice would make a legal stream fail its own ceiling.
pub(crate) fn create_record_provider<'source>(
    source: ResolvedSource<'source>,
    profile: NdjsonProfile,
    options: NdjsonDecodeOptions,
    diagnostics: DiagnosticPolicy,
    validation: ValidationMode,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::ErasedRecordStreamProvider<'source>, CodecError> {
    // A recovering framer under a strict request, or the reverse, would silently rewrite the request's own contract.
    // Reject before any byte of the source is read.
    check_profile_matches_request(profile.validation(), validation)?;
    jqf_codec_core::ErasedRecordStreamProvider::try_new_provider::<NdjsonRecordProvider, _>(source, || {
        NdjsonRecordProvider::try_new(profile, options, diagnostics, resources)
    })
}
