//! Structured source-aware JSON diagnostics.
//!
//! Shared by the whole-document parser and the scoped validator so both routes emit the same codes and labels.

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::{Namespace, ResolvedSource};

const JSON: Namespace = Namespace::new("json");

/// The byte-exact diagnostic vocabulary the whole-document parser and the scoped validator share. Stderr parity between
/// the routes is a hard law, so both cite these consts — a one-sided edit now fails to compile instead of silently
/// diverging one route's diagnostics.
pub(crate) mod diag {
    pub(crate) const EXPECTED_COMMA: &str = "expected-comma";
    pub(crate) const MSG_COMMA_OR_OBJECT: &str = "expected `,` or `}`";
    pub(crate) const MSG_COMMA_OR_ARRAY: &str = "expected `,` or `]`";

    pub(crate) const EXPECTED_COLON: &str = "expected-colon";
    pub(crate) const MSG_COLON: &str = "expected `:`";

    pub(crate) const TRAILING_COMMA: &str = "trailing-comma";
    pub(crate) const MSG_TRAILING_COMMA: &str = "trailing comma is not permitted";

    pub(crate) const TRAILING_CONTENT: &str = "trailing-content";
    pub(crate) const MSG_TRAILING_CONTENT: &str = "trailing content after JSON value";

    pub(crate) const INVALID_LITERAL: &str = "invalid-literal";
    pub(crate) const MSG_INVALID_LITERAL: &str = "invalid JSON literal";

    pub(crate) const EXPECTED_VALUE: &str = "expected-value";
    pub(crate) const MSG_EXPECTED_VALUE: &str = "expected one complete JSON value";

    pub(crate) const UNTERMINATED_STRING: &str = "unterminated-string";
    pub(crate) const MSG_UNTERMINATED_STRING: &str = "unterminated JSON string";

    pub(crate) const INVALID_NUMBER: &str = "invalid-number";
    pub(crate) const MSG_INVALID_NUMBER: &str = "invalid JSON number";

    pub(crate) const INVALID_ESCAPE: &str = "invalid-escape";
    pub(crate) const MSG_INVALID_ESCAPE: &str = "invalid JSON escape";
    pub(crate) const MSG_INVALID_ESCAPE_JSON5: &str = "invalid JSON5 escape";

    pub(crate) const INVALID_UTF8: &str = "invalid-utf8";
    pub(crate) const MSG_INVALID_UTF8: &str = "input is not valid UTF-8";

    /// Whether the bytes at `cursor` open a `//` or `/*` comment.
    fn opens_jsonc_comment(bytes: &[u8], cursor: usize) -> bool {
        matches!(bytes.get(cursor), Some(b'/')) && matches!(bytes.get(cursor + 1), Some(b'/' | b'*'))
    }

    /// The expected-key diagnostic text. Both decode routes raise the same code at the same offset, so the message must
    /// match too: when the offending bytes open a comment, the text names the dialect that would accept it (the
    /// `tsconfig.json` user story). Detection is a hint, never auto-selection.
    pub(crate) fn expected_key_message(bytes: &[u8], cursor: usize) -> &'static str {
        if opens_jsonc_comment(bytes, cursor) {
            "expected an object string key (a `//` or `/*` comment needs --input-format jsonc)"
        } else {
            "expected an object string key"
        }
    }

    /// The expected-value twin of [`expected_key_message`].
    pub(crate) fn expected_value_message(bytes: &[u8], cursor: usize) -> &'static str {
        if opens_jsonc_comment(bytes, cursor) {
            "expected one complete JSON value (a `//` or `/*` comment needs --input-format jsonc)"
        } else {
            MSG_EXPECTED_VALUE
        }
    }
}

pub(crate) fn invalid(
    source: ResolvedSource<'_>,
    offset: usize,
    code: &'static str,
    message: &'static str,
) -> CodecError {
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        JSON,
        source,
        offset,
        offset.saturating_add(usize::from(offset < source.bytes().len())),
        code,
        message,
    )
}

pub(crate) fn unsupported_number(source: ResolvedSource<'_>, start: usize, end: usize) -> CodecError {
    // A decode refusal must render as a decode-class failure: the input is grammar-valid but its exponent places the
    // exact decimal scale outside the supported range (the reference clamps to binary64; jqf's exact arithmetic refuses
    // — a catalogued divergence, compat row `intdiff-jqferr`).
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        JSON,
        source,
        start,
        end,
        "number-scale-out-of-range",
        "JSON number exponent is outside the supported exact decimal range",
    )
}

/// The hex twin of [`unsupported_number`]: a JSON5 hex literal longer than [`crate::parse::MAX_HEX_DIGITS`] is
/// grammar-valid but beyond the exact conversion bound, so it is refused at decode on every route (the scoped validator
/// raises the same refusal) rather than silently rounded.
pub(crate) fn unsupported_hex_number(source: ResolvedSource<'_>, start: usize, end: usize) -> CodecError {
    jqf_codec_core::diagnosed(
        CodecFailureKind::InvalidInput,
        JSON,
        source,
        start,
        end,
        "number-scale-out-of-range",
        "JSON5 hex literal is outside the supported exact conversion range",
    )
}

pub(crate) fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("strict JSON authoritative document construction")
}
