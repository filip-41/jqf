//! The JSON5 encoder: the commented-JSON encoder factory, armed with the `json5.comment@1` fact role and the JSON5
//! route identity, and never writing trailing commas.
//!
//! The rendering, the splice rulings and the whole-document floor are the commented-JSON ones — see
//! [`crate::jsonc::encode`]. The one JSON5 note is about the seams the shared scan declines: an unquoted key whose
//! token does not back up to a key, or a region the scan cannot name (an unterminated string or comment), yields an
//! empty cut set, so the caller falls back to the whole-document floor rather than writing wrong bytes. Single-quoted
//! strings ARE named — the scan skips both quote spellings, so a `]` inside a quoted member cannot truncate its
//! region. The floor re-encodes every leading comment from the facts, so the fallback is not lossy.

use jqf_codec_core::{CodecError, EncodeRequest, ErasedEncoderFactory};
use jqf_resource::ResourceContext;

use crate::ENCODE_PHYSICAL_ROUTE_ID;
use crate::encode::encode_options;
use crate::jsonc::encode::CommentedEncoderFactory;

use super::provider::COMMENT_FACT_ROLE;

pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    // The decoder strictly checks its dialect; the factory guards the same format+dialect pair so both directions of
    // the registration decline symmetrically.
    request.expect_target(super::FORMAT_ID, &super::DIALECT_TEXTS)?;
    let style = encode_options(request)?;
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || {
        Ok(CommentedEncoderFactory::new(
            style,
            COMMENT_FACT_ROLE,
            // JSON5 has one rendering and it never writes a trailing comma, however the input spelled its own.
            false,
            ENCODE_PHYSICAL_ROUTE_ID,
        ))
    })
}
