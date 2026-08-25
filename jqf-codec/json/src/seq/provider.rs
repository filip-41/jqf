//! The json-seq record-stream provider and its single advertised route.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessGuarantees, CodecError, CodecFailureKind, DiagnosticPolicy, ErasedRecordStreamProvider,
    ErasedRecordStreamSession, PhysicalRouteId, ProviderInput, RecordProviderOpen, RecordStreamProvider,
    RouteDescription, RouteSlot, ValidationMode,
};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use super::stream::JsonSeqRecordSession;
use super::{JsonSeqDecodeOptions, JsonSeqProfile};
use crate::record_route::{check_profile_matches_request, check_slot, record_route_table};

/// Stable physical identity of serial strict json-seq record framing.
const STRICT_RECORD_PHYSICAL_ROUTE_ID: PhysicalRouteId = PhysicalRouteId::derive_or_panic(super::FORMAT_ID, 4, 1);

/// Stable physical identity of serial recovering json-seq record framing.
const RECOVERING_RECORD_PHYSICAL_ROUTE_ID: PhysicalRouteId = PhysicalRouteId::derive_or_panic(super::FORMAT_ID, 5, 1);

/// Stable physical identity of framed json-seq encoding: the crate-root record-encoder identity, re-exported so the
/// derivation lives in exactly one place.
pub(crate) use crate::ENCODE_PHYSICAL_ROUTE_ID;

pub(crate) struct JsonSeqRecordProvider {
    routes: Vec<RouteDescription>,
    profile: JsonSeqProfile,
    options: JsonSeqDecodeOptions,
}

impl JsonSeqRecordProvider {
    pub(crate) fn try_new(
        profile: JsonSeqProfile,
        options: JsonSeqDecodeOptions,
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
    pub(crate) const fn physical_route(profile: JsonSeqProfile) -> PhysicalRouteId {
        match profile {
            JsonSeqProfile::Strict => STRICT_RECORD_PHYSICAL_ROUTE_ID,
            JsonSeqProfile::Recovering => RECOVERING_RECORD_PHYSICAL_ROUTE_ID,
        }
    }
}

impl RecordStreamProvider for JsonSeqRecordProvider {
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
        ErasedRecordStreamSession::try_new_with_route::<JsonSeqRecordSession, _>(
            input.source(),
            Self::physical_route(profile),
            provider_id,
            slot,
            || Ok(JsonSeqRecordSession::new(profile, options)),
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
    let RecordProviderOpen::JsonSeq {
        recovering,
        max_record_bytes,
    } = open
    else {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    };
    let profile = JsonSeqProfile::from_recovering(recovering);
    let options = JsonSeqDecodeOptions::try_new(Some(max_record_bytes), max_record_bytes)?;
    create_record_provider(
        source,
        profile,
        options,
        DiagnosticPolicy::ErrorsOnly,
        profile.validation(),
        resources,
    )
}

/// Opens one json-seq record-stream provider over contiguous retained input.
///
/// The framer charges NO input bytes: the payload provider the consumer opens over the same retained source owns the
/// request's single input charge, and charging twice would make a legal stream fail its own ceiling.
pub(crate) fn create_record_provider<'source>(
    source: ResolvedSource<'source>,
    profile: JsonSeqProfile,
    options: JsonSeqDecodeOptions,
    diagnostics: DiagnosticPolicy,
    validation: ValidationMode,
    resources: &mut ResourceContext<'_>,
) -> Result<jqf_codec_core::ErasedRecordStreamProvider<'source>, CodecError> {
    // A recovering framer under a strict request, or the reverse, would silently rewrite the request's own contract.
    // Reject before any byte of the source is read.
    check_profile_matches_request(profile.validation(), validation)?;
    jqf_codec_core::ErasedRecordStreamProvider::try_new_provider::<JsonSeqRecordProvider, _>(source, || {
        JsonSeqRecordProvider::try_new(profile, options, diagnostics, resources)
    })
}
