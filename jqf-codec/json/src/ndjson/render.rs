//! NDJSON output: one codec-owned terminator per record, atomically.
//!
//! Rendering a record's payload is strict JSON's job and is delegated whole to
//! [`crate::encode::create_compact_framed_factory`]. This module owns only the framing POLICY: which terminator, that
//! there is exactly one, and that it joins the payload inside the encoder's own staging buffer so a record can never be
//! published without its terminator and a record that failed mid-encode can never publish one.
//!
//! The recovering dialect has no encode side at all. It does not emit malformed data, a byte-order mark, blank records,
//! or unterminated records: it would emit the same conforming NDJSON the strict dialect does, so the strict dialect is
//! the only registered output dialect and a request naming the recovering one for output is a target mismatch.

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeRequest, ErasedEncoderFactory};
use jqf_resource::ResourceContext;

use super::provider::ENCODE_PHYSICAL_ROUTE_ID;
use super::{FORMAT_ID, NdjsonEncodeOptions, STRICT_DIALECT_ID};

/// Registry entry point: reads the terminator policy from the request's own option payload, defaulting to LF when
/// options are omitted.
pub(crate) fn create_registered_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    let options = match request.options {
        None => NdjsonEncodeOptions::default(),
        Some(payload) => *payload
            .downcast_ref::<NdjsonEncodeOptions>()
            .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))?,
    };
    if request.format.as_str() != FORMAT_ID || request.dialect.as_str() != STRICT_DIALECT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    crate::encode::create_compact_framed_factory(
        request,
        options.canonical_terminator().bytes(),
        ENCODE_PHYSICAL_ROUTE_ID,
        resources,
    )
}
