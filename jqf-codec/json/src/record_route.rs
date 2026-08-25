//! Shared record-route plumbing for the NDJSON and json-seq providers.
//!
//! Both framing dialects advertise exactly one record route over the whole retained source and reject the same two
//! mistakes before any byte is read: a wrong slot, and a framer profile that contradicts the request's validation mode.
//! The per-dialect providers keep their typed profiles, options, sessions, and physical route identities; only this
//! identical plumbing lives here.

use alloc::vec::Vec;

use jqf_codec_core::{
    AccessFootprintKind, AccessGuarantees, AccessResultKind, CodecError, CodecFailureKind, RouteDescription, RouteSlot,
    ValidationMode,
};
use jqf_resource::ResourceContext;

/// Builds the one-row route table both record providers advertise: the whole retained source framed into records. A
/// record stream selects nothing, so its footprint is Whole and its result is the record-stream kind and nothing else;
/// `try_table`'s minimal demand expands to exactly the `SemanticRoot` + `ValueShape` clauses, so both record routes
/// advertise the same demand.
pub(crate) fn record_route_table(
    guarantees: AccessGuarantees,
    resources: &ResourceContext<'_>,
) -> Result<Vec<RouteDescription>, CodecError> {
    RouteDescription::try_table(
        &[(
            jqf_codec_core::RECORD_ROUTE_SLOT,
            AccessFootprintKind::Whole,
            AccessResultKind::RecordStream,
        )],
        guarantees,
        resources,
    )
}

/// Rejects an open naming any slot other than this provider's one route.
pub(crate) fn check_slot(slot: RouteSlot) -> Result<(), CodecError> {
    if slot != jqf_codec_core::RECORD_ROUTE_SLOT {
        return Err(CodecError::new(CodecFailureKind::ProviderRouteMismatch));
    }
    Ok(())
}

/// Rejects a recovering framer under a strict request, or the reverse: it would silently rewrite the request's own
/// contract.
pub(crate) fn check_profile_matches_request(
    profile_validation: ValidationMode,
    request_validation: ValidationMode,
) -> Result<(), CodecError> {
    if profile_validation != request_validation {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    Ok(())
}
