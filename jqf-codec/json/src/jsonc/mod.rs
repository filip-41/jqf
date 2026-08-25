//! The JSONC codec: JSON plus comments and trailing commas, as a grammar dial on the host crate's own machine.
//!
//! JSONC is the JSON family's first extension grammar: its accepted text is a strict SUPERSET of RFC 8259 (comment
//! trivia + a trailing comma before a closing delimiter), its value model, number model, escape automaton, materializer
//! and encoder are the host crate's, and the delta is a per-format grammar read inside the SAME parser, a field fed
//! from `Options`. The three named real-world files (`tsconfig.json`, VS Code `settings.json`, `devcontainer.json`) are
//! exactly the accepted-grammar corpus the format exists to read.
//!
//! ## Dialects
//!
//! - `jsonc.trailing@1` — comments AND trailing commas. **The default**: the corpus the format exists to read (every
//!   editor that writes the three named files permits a trailing comma) would be rejected by the other dialect.
//! - `jsonc.default@1` — comments, no trailing commas (strict JSON's comma law, plus comment trivia).
//!
//! The difference is exactly one `bool` in the grammar.
//!
//! ## Ceiling
//!
//! JSONC serves the whole-document route only. Strict JSON's scoped exact-path route and lazy-frontier deferral stay
//! strict JSON's; an exact-path program over a large `.jsonc` materializes the whole document instead of the subtree.
//! `Located` is still answered identically through core's `ExactFallbackState`.
//!
//! ## Comment facts
//!
//! `jsonc.comment@1`, the landed `<fmt>.comment@1` list-of-texts shape with a LEADING owner: one fact per value node
//! whose leading comments the decode attached. The codec-agnostic matcher (`fact_role_serves` in the SDK's edit lane)
//! serves `.@comment` with zero SDK/engine change. Inline, trailing and inner comments survive edits byte-wise but are
//! not readable through `.@comment` — exactly what TOML and YAML ship, so the family stays consistent rather than
//! growing a fourth comment model.
//!
//! ## Edit
//!
//! JSONC implements `render_edit_append`/`render_edit_remove`, reusing the host crate's structural splice with a
//! comment-aware scan: a member insertion never orphans the comment above the following member, and a removal takes its
//! own leading comment block with it. The splice rulings are written where the splice lives.
//!
//! ## Numbers
//!
//! Identical to strict JSON: exact `Decimal`. "JSONC is JSON plus comments and trailing commas" holds at every layer,
//! so the format owes the divergence catalogue no row of its own.

pub(crate) mod encode;
pub(crate) mod options;
pub(crate) mod provider;

#[cfg(test)]
mod tests;

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner,
    RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};

use crate::tag;

pub use options::{JsoncEncodeOptions, JsoncEncodeProfile};

/// Stable JSONC format identity text.
pub const FORMAT_ID: &str = "jsonc";
/// The default dialect: comments AND trailing commas.
pub const TRAILING_DIALECT_ID: &str = "jsonc.trailing@1";
/// The strict-comma dialect: comments, no trailing commas.
pub const DEFAULT_DIALECT_ID: &str = "jsonc.default@1";
/// The edit-render output dialect (the namespace law): what the edit lane's whole-document floor re-encode renders
/// with.
pub const JQF_1_0_DIALECT_ID: &str = "jsonc.jqf-1.0@1";
/// The `jsonc.trailing-jqf@1` output profile identity.
pub const TRAILING_JQF_DIALECT_ID: &str = "jsonc.trailing-jqf@1";
/// The `jsonc.default-jqf@1` output profile identity.
pub const DEFAULT_JQF_DIALECT_ID: &str = "jsonc.default-jqf@1";
/// `jsonc.source@1` is RESERVED (the identity section names it; a source echo profile is a receipt-earned fidelity
/// tier, out of v1 scope — the canonical-identity echo lane already republishes a source's own bytes when the decode
/// found them canonical: compact, comment-free, quoted-key strict JSON; everything else re-encodes from the floor).
const FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(FORMAT_ID);
const DIALECTS: [DialectIdRef<'static>; 5] = [
    DialectIdRef::from_static(TRAILING_DIALECT_ID),
    DialectIdRef::from_static(DEFAULT_DIALECT_ID),
    DialectIdRef::from_static(TRAILING_JQF_DIALECT_ID),
    DialectIdRef::from_static(DEFAULT_JQF_DIALECT_ID),
    DialectIdRef::from_static(JQF_1_0_DIALECT_ID),
];
/// The accepted dialect spellings as plain text, for request guards that check a format+dialect pair against the
/// registration.
pub(crate) const DIALECT_TEXTS: [&str; 5] = [
    TRAILING_DIALECT_ID,
    DEFAULT_DIALECT_ID,
    TRAILING_JQF_DIALECT_ID,
    DEFAULT_JQF_DIALECT_ID,
    JQF_1_0_DIALECT_ID,
];

/// The CLI-facing routes the JSONC registration serves: the single-document input model (no record framing, no
/// adjacent-text stream — one JSONC document per source, exactly as TOML) and the source-preserving edit lane: the
/// comment-aware splice binds spans and rewrites members, so `--edit` over JSONC is served by declaration.
const ROUTES: [RouteCapability; 1] = [RouteCapability::Edit];

/// Constructs the allocation-free validated JSONC registration: one registration carries both dialects, the `.jsonc`
/// extension, decode/encode/tag-validation factories, and the edit capability (declared in the same commit as its
/// receipts).
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FORMAT,
            &DIALECTS,
            CodecOperations::new(true, true, true),
            &ROUTES,
            &["jsonc"],
            // Input dialects (trailing/default) retain their edit document's trailing byte; output profiles
            // (trailing-jqf/default-jqf/ jqf-1.0) have the facade supply the item newline.
            &[
                ItemByteOwner::Codec,
                ItemByteOwner::Codec,
                ItemByteOwner::Facade,
                ItemByteOwner::Facade,
                ItemByteOwner::Facade,
            ],
            &[],
            // Edit-only registration: a single-document grammar never reaches the sequence drives' inter-value
            // separator scan, so no value separators are declared (empty-unless-AdjacentValues is enforced by the
            // registration constructor).
            &[],
        ),
        Some(DecoderFactoryRecord::new(provider::create_provider)),
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_jsonc_validator)),
        None,
    )
}
