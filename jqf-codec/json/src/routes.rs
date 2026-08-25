//! Stable physical route identities for the strict-JSON access routes.
//!
//! Each constant names one physical route the codec's sessions can publish through. The identities are DERIVED from
//! (format, kind, specialization) by [`jqf_codec_core::PhysicalRouteId::derive`] — no hand-assigned magic numbers.
//! They are STABLE: receipts, route inventories, and the force-route differential compare them, so a constant never
//! changes after it ships.

/// Stable physical identity of strict full parse and authoritative materialization.
pub const FULL_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    jqf_codec_core::PhysicalRouteId::derive_or_panic(crate::FORMAT_ID, 1, 1);

/// Stable physical identity of strict scoped exact-path decode: whole-input validation with subtree-only
/// materialization.
pub const SCOPED_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    jqf_codec_core::PhysicalRouteId::derive_or_panic(crate::FORMAT_ID, 2, 1);

/// Stable physical identity of strict semantic JSON encoding.
pub const ENCODE_PHYSICAL_ROUTE_ID: jqf_codec_core::PhysicalRouteId =
    jqf_codec_core::PhysicalRouteId::derive_or_panic(crate::FORMAT_ID, 3, 1);
