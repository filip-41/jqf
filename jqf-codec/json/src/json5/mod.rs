//! The JSON5 codec: JSON plus the JSON5 grammar extensions, as a grammar dial on the host crate's own machine.
//!
//! JSON5 (json5.org) adds over strict RFC 8259: comments (`//` and `/* */`), single-quoted strings, unquoted
//! (`IdentifierName`) object keys, hex integers, leading/trailing decimal points, an explicit `+`, the `Infinity`/`NaN`
//! spellings, the `\x`/`\0`/line-continuation escapes, and the U+2028/U+2029/U+FEFF whitespace. Its value model, number
//! model, escape automaton, materializer and encoder are the host crate's; the delta is grammar arms on the SAME
//! parser, gated by `JsonGrammar::json5`, the same dial the leniency bit rides.
//!
//! ## Dialect
//!
//! - `json5.document@1` — the one input dialect: the complete JSON5 grammar (comments AND trailing commas are JSON5
//!   features, so both arms ride).
//!
//! ## Numbers
//!
//! Decimal spellings decode to exact `Decimal` (identical to strict JSON); hex spellings (`0x…`) decode to exact
//! `Integer` — there is no rounding step to specify. `Infinity`/`-Infinity`/`NaN`/`-NaN` decode to `Float` with the
//! pinned non-finite bit patterns. **Named divergence from the JSON5 spec** (ECMAScript numbers are binary64): `1e400`
//! decodes exactly here where a spec reader yields `Infinity` — identical in kind to the YAML/TOML exact-decimal
//! divergence the slice already shipped.
//!
//! ## Identifiers
//!
//! The `IdentifierName` key grammar is the ASCII fast path (`[A-Za-z_$]` start, `[A-Za-z0-9_$]` continue) — a
//! documented narrowing: a NON-ASCII identifier key is rejected here where the full ECMAScript 5.1 / Unicode 3.0 tables
//! would accept it. A pinned Unicode table generation is the follow-up that closes the gap; registering the dialect
//! with the ASCII arm is the recorded ceiling this lane ships.
//!
//! ## Ceiling
//!
//! JSON5 serves the whole-document route only, exactly like JSONC: no scoped exact-path route, no lazy frontier.
//! `Located` is answered identically through core's `ExactFallbackState`.
//!
//! ## Comment facts
//!
//! `json5.comment@1`, the landed `<fmt>.comment@1` list-of-texts shape with a LEADING owner — the JSONC machinery
//! reused with the role swapped. The codec-agnostic matcher (`fact_role_serves`, in the SDK's edit lane) serves
//! `.@comment` with zero SDK/engine change.
//!
//! ## Edit
//!
//! JSON5 implements `render_edit_append`/`render_edit_remove`, reusing the host crate's structural splice with the
//! comment-aware scan — a member insertion never orphans the comment above the following member, and a removal takes
//! its own leading comment block with it. The splice rulings are written where the splice lives, beside the
//! commented-JSON encoder.

pub(crate) mod encode;
pub(crate) mod provider;

#[cfg(test)]
mod tests;

use jqf_codec_core::{
    CodecDescriptor, CodecOperations, CodecRegistration, DecoderFactoryRecord, EncoderFactoryRecord, ItemByteOwner,
    RegistrationError, RouteCapability, TagValidatorFactoryRecord,
};
use jqf_data::{DialectIdRef, FormatIdRef};

use crate::tag;

/// Stable JSON5 format identity text.
pub const FORMAT_ID: &str = "json5";
/// The one input dialect: the complete JSON5 grammar.
pub const DOCUMENT_DIALECT_ID: &str = "json5.document@1";
/// The edit-render output dialect (the namespace law): what the edit lane's whole-document floor re-encode renders
/// with.
pub const JQF_1_0_DIALECT_ID: &str = "json5.jqf-1.0@1";
/// The `json5.jqf@1` output profile identity.
pub const JQF_DIALECT_ID: &str = "json5.jqf@1";
/// `json5.source@1` is RESERVED (the identity section names it; a source echo profile is a receipt-earned fidelity
/// tier, out of v1 scope — the canonical-identity echo lane already republishes a source's own bytes when the decode
/// found them canonical: compact, comment-free, quoted-key strict JSON; everything else re-encodes from the floor).
const FORMAT: FormatIdRef<'static> = FormatIdRef::from_static(FORMAT_ID);
const DIALECTS: [DialectIdRef<'static>; 3] = [
    DialectIdRef::from_static(DOCUMENT_DIALECT_ID),
    DialectIdRef::from_static(JQF_DIALECT_ID),
    DialectIdRef::from_static(JQF_1_0_DIALECT_ID),
];
/// The accepted dialect spellings as plain text, for request guards that check a format+dialect pair against the
/// registration.
pub(crate) const DIALECT_TEXTS: [&str; 3] = [DOCUMENT_DIALECT_ID, JQF_DIALECT_ID, JQF_1_0_DIALECT_ID];

/// The CLI-facing routes the JSON5 registration serves: the single-document input model and the source-preserving edit
/// lane: the comment-aware splice binds spans and rewrites members, so `--edit` over JSON5 is served by declaration.
const ROUTES: [RouteCapability; 1] = [RouteCapability::Edit];

/// Constructs the allocation-free validated JSON5 registration: the one dialect, the `.json5` extension, decode/encode/
/// tag-validation factories, and the edit capability (declared in the same commit as its receipts).
pub fn registration() -> Result<CodecRegistration<'static>, RegistrationError> {
    CodecRegistration::try_new(
        CodecDescriptor::new(
            FORMAT,
            &DIALECTS,
            CodecOperations::new(true, true, true),
            &ROUTES,
            &["json5"],
            // Input dialect (document) retains its edit document's trailing byte; output profiles (jqf/jqf-1.0) have
            // the facade supply the item newline.
            &[ItemByteOwner::Codec, ItemByteOwner::Facade, ItemByteOwner::Facade],
            &[],
            // Edit-only registration: a single-document grammar never reaches the sequence drives' inter-value
            // separator scan, so no value separators are declared (empty-unless-AdjacentValues is enforced by the
            // registration constructor).
            &[],
        ),
        Some(DecoderFactoryRecord::new(provider::create_provider)),
        Some(EncoderFactoryRecord::new(encode::create_factory)),
        Some(TagValidatorFactoryRecord::new(tag::create_json5_validator)),
        None,
    )
}
