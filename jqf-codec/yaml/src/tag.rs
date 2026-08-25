//! YAML target tag validation: the first non-rejecting tag validator.
//!
//! `validate(tags)` accepts every `TagId` that is grammar-valid YAML tag text and injective into a YAML node identity
//! (no two stored tags may collapse to one emitted node property). A tag that cannot be emitted injectively is
//! unrepresentable: standard tags emit through their `!!` spelling, a grammar-valid local tag keeps its exact
//! `!suffix`, and another exact URI uses `!<...>` without decoding or changing percent triplets — so two DISTINCT tag
//! texts that would emit the same property (e.g. `!a%62c` and `!abc` if `%62` were decoded) collide.

use alloc::string::String;
use alloc::vec::Vec;

use alloc::borrow::ToOwned;
use alloc::format;

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeRequest, ErasedTagValidator, TagValidator};
use jqf_data::TagId;
use jqf_resource::ResourceContext;

/// The stable identity of the YAML tag validator factory.
pub(crate) fn create_validator(
    request: EncodeRequest<'_, '_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<ErasedTagValidator, CodecError> {
    // A tag validator is built from an encode request and validates the TARGET output profile. The single registration
    // serves every dialect; the canonical output profiles share this factory.
    let dialects: [&str; 7] = crate::ALL_DIALECTS.map(jqf_data::DialectIdRef::as_str);
    request.expect_target(crate::FORMAT_ID, &dialects)?;
    ErasedTagValidator::try_new_validator(|| Ok(YamlTagValidator))
}

struct YamlTagValidator;

impl TagValidator for YamlTagValidator {
    fn validate(&self, tags: &[&TagId]) -> Result<(), CodecError> {
        for tag in tags {
            let text = tag.as_str();
            if !tag_grammar_valid(text) {
                return Err(CodecError::new(CodecFailureKind::InvalidTag));
            }
        }
        // Injectivity: two distinct stored tags must not collapse to one emitted node property. The emitted spellings
        // are SORTED and the adjacent duplicates scanned, which is linear-ish rather than a per-tag linear `contains`
        // scan (O(n^2) on a large tag set). The `CollidingTags` failure names no colliding pair, so the sorted
        // detection order is unobservable — the failure is identical whatever the pair.
        let mut emitted: Vec<String> = Vec::with_capacity(tags.len());
        emitted.extend(tags.iter().map(|tag| emit_spelling(tag.as_str())));
        emitted.sort();
        emitted.dedup();
        if emitted.len() != tags.len() {
            return Err(CodecError::new(CodecFailureKind::CollidingTags));
        }
        Ok(())
    }
}

/// Whether a tag text is grammar-valid YAML tag text: a standard tag URI, a `!suffix` local tag, or an exact URI that
/// emits as `!<...>`.
fn tag_grammar_valid(text: &str) -> bool {
    if let Some(suffix) = text.strip_prefix("tag:yaml.org,2002:") {
        return !suffix.is_empty() && tag_uri_chars_valid(suffix, false);
    }
    if text == "!" {
        return true;
    }
    if let Some(suffix) = text.strip_prefix('!') {
        if suffix.is_empty() {
            return false;
        }
        // A local tag: ns-tag-char (ns-uri-char without `!`, `,`, `[`, `]`).
        return tag_uri_chars_valid(suffix, true);
    }
    // An exact URI: emits as `!<...>`, so any printable URI is valid as long as it is not an empty angle.
    tag_uri_chars_valid(text, false)
}

/// Whether every character is a YAML URI character (the `ns-uri-char` set), with `%` restricted to a `%XX` hex triplet
/// — the identity characters are copied, never decoded, and the scanner's own escape law is the same. With `local`, the
/// four characters the grammar's local-tag production excludes (`!`, `,`, `[`, `]`) are additionally rejected. An empty
/// text is not valid.
fn tag_uri_chars_valid(text: &str, local: bool) -> bool {
    if text.is_empty() {
        return false;
    }
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let (high, low) = (chars.next(), chars.next());
            if !matches!((high, low), (Some(h), Some(l)) if h.is_ascii_hexdigit() && l.is_ascii_hexdigit()) {
                return false;
            }
            continue;
        }
        if local && matches!(c, '!' | ',' | '[' | ']') {
            return false;
        }
        if !c.is_ascii_alphanumeric()
            && !matches!(
                c,
                '-' | '_'
                    | ';'
                    | '/'
                    | '?'
                    | ':'
                    | '@'
                    | '&'
                    | '='
                    | '+'
                    | '$'
                    | '.'
                    | '#'
                    | '!'
                    | '~'
                    | '*'
                    | '\''
                    | '('
                    | ')'
            )
        {
            return false;
        }
    }
    true
}

/// The emitted spelling of a tag: the `!!suffix` form for standard tags, the exact `!suffix` for local tags, `!<uri>`
/// for other exact URIs.
pub(crate) fn emit_spelling(text: &str) -> String {
    if let Some(suffix) = text.strip_prefix("tag:yaml.org,2002:") {
        return format!("!!{suffix}");
    }
    if text.starts_with('!') {
        return text.to_owned();
    }
    format!("!<{text}>")
}

/// Whether a WRITTEN tag spelling is well-formed enough to insert verbatim before a scalar: starts with `!` (the
/// verbatim `!<...>`, the standard `!!suffix`, the local `!suffix`, or the bare `!`) or with `tag:` (a full URI,
/// emitted as `!<...>`), contains no whitespace or control characters, and a verbatim form is properly closed with no
/// interior `>`.
pub(crate) fn valid_spelling(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('!') else {
        return text.starts_with("tag:");
    };
    if rest.is_empty() {
        return true; // the bare non-specific `!`
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if let Some(uri) = rest.strip_prefix('<') {
        return uri
            .strip_suffix('>')
            .is_some_and(|inner| !inner.is_empty() && !inner.contains('>'));
    }
    if let Some(suffix) = rest.strip_prefix('!') {
        return !suffix.is_empty(); // `!!suffix`
    }
    true // `!suffix`
}
