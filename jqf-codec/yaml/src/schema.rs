//! The three built-in YAML schemas: failsafe, JSON, core.
//!
//! §4.8 table: the schemas have a closed tag projection and distinct resolution failures. Resolution turns a plain
//! scalar's text plus its implicit/explicit tag into a semantic category:
//!
//! | Schema | Recognized tags | `?` resolution | jqf publication |
//! | --- | --- | --- | --- |
//! | failsafe | map, seq, str | by node kind; plain scalars resolve to `!!str` (YAML 1.2 §10.1.2) | map/seq/String |
//! | JSON | + null/bool/int/float | JSON regexes, no match is a schema error | Null/Bool/Integer/Float/String |
//! | core | the same seven | core regexes, fallback `str`, empty `null` | same categories |
//!
//! The number law lives here and is YAML 1.2.2 core, not a 1.1 hybrid: integers are `[-+]?[0-9]+` (leading zeros are
//! integers), `0o[0-7]+`, and `0x[0-9a-fA-F]+`. Underscores, binary `0b`, and uppercase radix prefixes (`0O`/`0X`/`0B`)
//! are strings. Finite floats follow the core float production (no underscores) and unify to exact decimals; overflow →
//! unrepresentable; `.inf`/`-.inf` → signed infinity; `.nan` variants → positive quiet NaN bits
//! `0x7ff8_0000_0000_0000`. The signed-zero claim for `-0.0` is the number layer's, not this codec's.
//!
//! Explicit non-core tags (`!money`, `!!binary`, `!!timestamp`, `!!set`, `!!omap`) stay non-core `Value::Tagged` around
//! the ordinary payload; v1 does not silently base64-decode, timestamp-parse, or reshape them.

use alloc::borrow::ToOwned;
use alloc::string::String;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_source::ResolvedSource;

use crate::error;
use crate::graph::{NodeId, YamlGraph, YamlNode};
use crate::provider::DialectKind;

/// The YAML 1.2.2 standard tag URIs.
pub(crate) const TAG_MAP: &str = "tag:yaml.org,2002:map";
pub(crate) const TAG_SEQ: &str = "tag:yaml.org,2002:seq";
pub(crate) const TAG_STR: &str = "tag:yaml.org,2002:str";
pub(crate) const TAG_NULL: &str = "tag:yaml.org,2002:null";
pub(crate) const TAG_BOOL: &str = "tag:yaml.org,2002:bool";
pub(crate) const TAG_INT: &str = "tag:yaml.org,2002:int";
pub(crate) const TAG_FLOAT: &str = "tag:yaml.org,2002:float";
/// The YAML 1.1 merge-key tag (yaml.org/type/merge.html). Not one of the seven core tags: the parser CONSUMES it at
/// mapping close under the core schema (see `YamlParser::expand_merge_keys`), so it never reaches scalar resolution as
/// a key; on a non-key scalar it stays an ordinary non-core tag.
pub(crate) const TAG_MERGE: &str = "tag:yaml.org,2002:merge";

/// The positive quiet NaN bits (§4.8).
pub(crate) const POSITIVE_QUIET_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

/// The semantic category a resolved scalar publishes as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScalarCategory {
    /// Core string (resolved `!!str`, or core-schema fallback).
    String,
    /// JSON-schema null (only `null`/`Null`/`NULL`/`~` under core).
    Null,
    Bool(bool),
    /// Arbitrary-precision integer.
    Integer,
    /// Binary64 float.
    Float,
}

/// The resolution outcome for one scalar node.
#[derive(Clone, Debug)]
pub(crate) enum ResolvedScalar {
    /// Publishes as a core category (with the resolved intrinsic tag).
    Core {
        category: ScalarCategory,
        /// The exact resolved tag text (e.g. `tag:yaml.org,2002:str`).
        tag: &'static str,
    },
    /// An explicit non-core tag wrapping the ordinary payload.
    Tagged {
        /// The exact non-core tag text (`!money`, `!!binary`, ...).
        tag: String,
        /// The payload category (String/sequence/mapping).
        payload: ScalarCategory,
    },
}

/// Resolves one scalar node under the schema, returning its category and tag. `style` distinguishes plain (implicit
/// resolution applies) from quoted (always a string).
pub(crate) fn resolve_scalar(
    graph: &YamlGraph,
    node: NodeId,
    dialect: DialectKind,
    source: ResolvedSource<'_>,
) -> Result<ResolvedScalar, CodecError> {
    let YamlNode::Scalar { text, tag, .. } = graph.node(node, source) else {
        return Err(CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "YAML scalar resolution over a non-scalar node",
        }));
    };
    // An explicit non-core tag wraps the ordinary payload. The bare `!` tag is NON-SPECIFIC: the spec's Example 6.28
    // resolves `! 12` to the STRING "12" (`[ "12", 12, "12" ]`), so a `!`-tagged scalar forces the string category
    // rather than falling through to the implicit rules.
    if let Some(tag) = tag {
        if tag == "!" {
            return Ok(ResolvedScalar::Core {
                category: ScalarCategory::String,
                tag: TAG_STR,
            });
        }
        if !is_standard_tag(tag) {
            return Ok(ResolvedScalar::Tagged {
                tag: tag.to_owned(),
                payload: ScalarCategory::String,
            });
        }
    }
    match tag {
        Some(TAG_STR) | None if is_quoted_or_explicit_str(graph, node, tag, source) => Ok(ResolvedScalar::Core {
            category: ScalarCategory::String,
            tag: TAG_STR,
        }),
        None => {
            // Implicit plain-scalar resolution under the schema.
            match dialect {
                DialectKind::Failsafe => {
                    // YAML 1.2 §10.1.2: the failsafe schema resolves a plain `?` (non-specific) scalar to `!!str`. The
                    // dialect is unusable otherwise — `a: 1` would fail at the plain key.
                    Ok(ResolvedScalar::Core {
                        category: ScalarCategory::String,
                        tag: TAG_STR,
                    })
                }
                DialectKind::Json => resolve_json_implicit(text, source, node, graph),
                DialectKind::Core => resolve_core_implicit(text, source, node),
            }
        }
        // An explicit `!!null` resolves like the implicit one: only the null spellings are nulls, anything else is a
        // schema error (the same law the !!bool/!!int/!!float arms enforce).
        Some(TAG_NULL) => {
            if matches!(text, "" | "~" | "null" | "Null" | "NULL") {
                Ok(ResolvedScalar::Core {
                    category: ScalarCategory::Null,
                    tag: TAG_NULL,
                })
            } else {
                Err(schema_error(source, node, graph, "invalid null literal for !!null"))
            }
        }
        Some(TAG_BOOL) => match text {
            "true" | "True" | "TRUE" => Ok(ResolvedScalar::Core {
                category: ScalarCategory::Bool(true),
                tag: TAG_BOOL,
            }),
            "false" | "False" | "FALSE" => Ok(ResolvedScalar::Core {
                category: ScalarCategory::Bool(false),
                tag: TAG_BOOL,
            }),
            _ => Err(schema_error(source, node, graph, "invalid boolean literal for !!bool")),
        },
        Some(TAG_INT) => match parse_yaml_int(text) {
            Some(()) => Ok(ResolvedScalar::Core {
                category: ScalarCategory::Integer,
                tag: TAG_INT,
            }),
            None => Err(schema_error(source, node, graph, "invalid integer literal for !!int")),
        },
        Some(TAG_FLOAT) => match parse_yaml_float(text) {
            Ok(category) => Ok(ResolvedScalar::Core {
                category,
                tag: TAG_FLOAT,
            }),
            Err(()) => Err(schema_error(source, node, graph, "invalid float literal for !!float")),
        },
        Some(TAG_STR) => Ok(ResolvedScalar::Core {
            category: ScalarCategory::String,
            tag: TAG_STR,
        }),
        Some(TAG_MAP | TAG_SEQ) => Err(schema_error(source, node, graph, "scalar carries a collection tag")),
        // Unreachable: the early returns above handle every non-standard tag and `!`, and every standard tag is matched
        // above, so this arm can only be reached by a broken caller. Minting a tag here would fabricate a fact; refuse
        // instead.
        _ => Err(CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "YAML scalar resolution over an unhandled tag",
        })),
    }
}

/// Whether a scalar is quoted (or carries an explicit `!!str`), which makes it a string under every schema.
fn is_quoted_or_explicit_str(graph: &YamlGraph, node: NodeId, tag: Option<&str>, source: ResolvedSource<'_>) -> bool {
    if tag == Some(TAG_STR) {
        return true;
    }
    matches!(
        graph.node(node, source),
        YamlNode::Scalar { style, .. } if style != crate::graph::ScalarStyle::Plain
    )
}

/// Whether a tag text is one of the seven standard URIs.
#[must_use]
pub(crate) fn is_standard_tag(tag: &str) -> bool {
    matches!(
        tag,
        TAG_MAP | TAG_SEQ | TAG_STR | TAG_NULL | TAG_BOOL | TAG_INT | TAG_FLOAT
    )
}

fn schema_error(source: ResolvedSource<'_>, node: NodeId, graph: &YamlGraph, message: &'static str) -> CodecError {
    let span = graph.node_span(node);
    error::invalid_range(source, span.start() as usize, span.end() as usize, "schema", message)
}

/// The JSON schema's implicit plain-scalar resolution (1.2.2 JSON regexes; no match — including empty — is a schema
/// error).
fn resolve_json_implicit(
    text: &str,
    source: ResolvedSource<'_>,
    node: NodeId,
    graph: &YamlGraph,
) -> Result<ResolvedScalar, CodecError> {
    let span = graph.node_span(node);
    if text.is_empty() {
        return Err(error::invalid_range(
            source,
            span.start() as usize,
            span.end() as usize,
            "schema",
            "empty plain scalar is not resolvable under the JSON schema",
        ));
    }
    match text {
        "null" => Ok(ResolvedScalar::Core {
            category: ScalarCategory::Null,
            tag: TAG_NULL,
        }),
        "true" | "false" => Ok(ResolvedScalar::Core {
            category: ScalarCategory::Bool(text == "true"),
            tag: TAG_BOOL,
        }),
        _ => {
            if let Some(()) = parse_json_int(text) {
                return Ok(ResolvedScalar::Core {
                    category: ScalarCategory::Integer,
                    tag: TAG_INT,
                });
            }
            if parse_json_float(text) {
                return Ok(ResolvedScalar::Core {
                    category: ScalarCategory::Float,
                    tag: TAG_FLOAT,
                });
            }
            Err(error::invalid_range(
                source,
                span.start() as usize,
                span.end() as usize,
                "schema",
                "plain scalar does not match any JSON-schema tag",
            ))
        }
    }
}

/// The core schema's implicit resolution (1.2.2 core regexes; fallback is `str`, empty is `null`).
#[allow(clippy::unnecessary_wraps)]
fn resolve_core_implicit(text: &str, _source: ResolvedSource<'_>, _node: NodeId) -> Result<ResolvedScalar, CodecError> {
    if text.is_empty() {
        return Ok(ResolvedScalar::Core {
            category: ScalarCategory::Null,
            tag: TAG_NULL,
        });
    }
    if matches!(text, "null" | "Null" | "NULL" | "~") {
        return Ok(ResolvedScalar::Core {
            category: ScalarCategory::Null,
            tag: TAG_NULL,
        });
    }
    if matches!(text, "true" | "True" | "TRUE") {
        return Ok(ResolvedScalar::Core {
            category: ScalarCategory::Bool(true),
            tag: TAG_BOOL,
        });
    }
    if matches!(text, "false" | "False" | "FALSE") {
        return Ok(ResolvedScalar::Core {
            category: ScalarCategory::Bool(false),
            tag: TAG_BOOL,
        });
    }
    if let Some(()) = parse_yaml_int(text) {
        return Ok(ResolvedScalar::Core {
            category: ScalarCategory::Integer,
            tag: TAG_INT,
        });
    }
    match parse_yaml_float(text) {
        Ok(category) => Ok(ResolvedScalar::Core {
            category,
            tag: TAG_FLOAT,
        }),
        Err(()) => Ok(ResolvedScalar::Core {
            category: ScalarCategory::String,
            tag: TAG_STR,
        }),
    }
}

/// Parses a YAML 1.2.2 core-schema integer. The productions are `[-+]?[0-9]+`, `0o[0-7]+`, and `0x[0-9a-fA-F]+`.
/// Leading zeros are integers. Underscores, binary `0b`, a sign on a radix form, and uppercase radix prefixes are not
/// integers — they fall through to string (or float, if that production matches). The value is materialized later by
/// the document build through jqf-data's `Integer`.
#[must_use]
pub(crate) fn parse_yaml_int(text: &str) -> Option<()> {
    let bytes = text.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' {
        match bytes[1] {
            b'x' => return parse_core_radix(&bytes[2..], 16),
            b'o' => return parse_core_radix(&bytes[2..], 8),
            _ => {}
        }
    }
    let digits = if bytes.first().is_some_and(|b| matches!(b, b'-' | b'+')) {
        &bytes[1..]
    } else {
        bytes
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(())
}

/// One 1.2.2 core radix body: at least one digit, no underscores, only digits legal for `radix`.
fn parse_core_radix(digits: &[u8], radix: u32) -> Option<()> {
    if digits.is_empty() {
        return None;
    }
    for &byte in digits {
        let value = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' if radix == 16 => u32::from(byte - b'a' + 10),
            b'A'..=b'F' if radix == 16 => u32::from(byte - b'A' + 10),
            _ => return None,
        };
        if value >= radix {
            return None;
        }
    }
    Some(())
}

/// Parses a YAML core-schema integer in its JSON-schema restricted form: the 1.2.2 JSON int regex `[-+]?[0-9]+` —
/// decimal only, no underscores, optional `-`/`+` sign, leading zeros allowed (`007` resolves to int 7, mirroring the
/// float side's acceptance of `007.5`).
#[must_use]
fn parse_json_int(text: &str) -> Option<()> {
    let bytes = text.as_bytes();
    let mut i = 0;
    if bytes.first().is_some_and(|b| matches!(b, b'-' | b'+')) {
        i = 1;
    }
    if i >= bytes.len() || !bytes[i].is_ascii_digit() {
        return None;
    }
    for &byte in &bytes[i..] {
        if !byte.is_ascii_digit() {
            return None;
        }
    }
    Some(())
}

/// The JSON schema's float check: the 1.2.2 JSON float regex `[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?([eE][-+]?[0-9]+)?)` —
/// finite decimal and exponent spellings only. `.inf`/`.nan` are NOT part of the JSON schema's float tag (the core
/// schema owns those spellings); a plain `.inf` under the JSON dialect is a schema error, not a float.
fn parse_json_float(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    if bytes.first().is_some_and(|b| matches!(b, b'-' | b'+')) {
        i = 1;
    }
    let mut digits = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    let mut has_dot = false;
    if i < bytes.len() && bytes[i] == b'.' {
        has_dot = true;
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    let mut has_exp = false;
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        has_exp = true;
        i += 1;
        if i < bytes.len() && matches!(bytes[i], b'-' | b'+') {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == bytes.len() && digits > 0 && (has_dot || has_exp)
}

/// Parses a YAML float: the 1.2.2 core float production `[-+]? (\.[0-9]+ | [0-9]+ (\.[0-9]*)? ([eE][-+]?[0-9]+)?)`,
/// plus the `.inf`/`-.inf`/`.nan` spellings. Underscores are not part of the core float production. A mantissa with no
/// digit (`.`) carries no value and is not a float, and the int production is tried first, so a bare digit spelling
/// (`123`, `007`) still resolves as an int.
pub(crate) fn parse_yaml_float(text: &str) -> Result<ScalarCategory, ()> {
    // Infinity and NaN spellings.
    match text {
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF" | ".nan" | ".NaN"
        | ".NAN" => return Ok(ScalarCategory::Float),
        _ => {}
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    if bytes.first().is_some_and(|b| matches!(b, b'-' | b'+')) {
        i = 1;
    }
    // The mantissa: `\.[0-9_]+`, or `[0-9_]+` with an optional `.`+digits.
    let mut digits = 0usize;
    let dot_first = bytes.get(i) == Some(&b'.');
    if dot_first {
        i += 1;
    }
    while let Some(&byte) = bytes.get(i) {
        if byte.is_ascii_digit() {
            digits += 1;
            i += 1;
        } else {
            break;
        }
    }
    if digits == 0 {
        return Err(());
    }
    if !dot_first && bytes.get(i) == Some(&b'.') {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    // The optional exponent: digits only, never underscores.
    if bytes.get(i).is_some_and(|b| matches!(b, b'e' | b'E')) {
        i += 1;
        if bytes.get(i).is_some_and(|b| matches!(b, b'-' | b'+')) {
            i += 1;
        }
        let exp_digits = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == exp_digits {
            return Err(());
        }
    }
    if i != bytes.len() {
        return Err(());
    }
    Ok(ScalarCategory::Float)
}

/// Whether a float spelling is the fixed positive quiet NaN.
#[must_use]
pub(crate) fn is_nan_spelling(text: &str) -> bool {
    matches!(text, ".nan" | ".NaN" | ".NAN")
}

/// Whether a float spelling is a signed infinity.
#[must_use]
pub(crate) fn is_infinity_spelling(text: &str) -> bool {
    matches!(
        text,
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF"
    )
}

/// Whether a float spelling is negative infinity.
#[must_use]
pub(crate) fn is_negative_infinity(text: &str) -> bool {
    matches!(text, "-.inf" | "-.Inf" | "-.INF")
}

#[cfg(test)]
mod tests {
    use super::{ScalarCategory, parse_yaml_float, parse_yaml_int};

    fn is_int(text: &str) -> bool {
        parse_yaml_int(text).is_some()
    }

    fn is_float(text: &str) -> bool {
        parse_yaml_float(text) == Ok(ScalarCategory::Float)
    }

    fn is_neither(text: &str) -> bool {
        !is_int(text) && !is_float(text)
    }

    #[test]
    fn leading_zero_decimals_are_integers() {
        assert!(is_int("0"));
        assert!(is_int("007"));
        assert!(is_int("07030"));
        assert!(is_int("+010"));
        assert!(is_int("-0100"));
        assert!(is_int("0123456789012345678901234567890"));
        // The float production also matches a bare digit run; implicit resolution tries int first, so `07030` publishes
        // as an integer.
        assert!(is_float("07030"));
    }

    #[test]
    fn core_radix_is_lowercase_prefix_only() {
        assert!(is_int("0x1F"));
        assert!(is_int("0o17"));
        assert!(is_neither("0X1F"));
        assert!(is_neither("0O17"));
        assert!(is_neither("0b101"));
        assert!(is_neither("0B101"));
        assert!(is_neither("-0x1F"));
        assert!(is_neither("+0o17"));
    }

    #[test]
    fn underscores_are_not_core_numbers() {
        assert!(is_neither("1_000"));
        assert!(is_neither("1_2_3"));
        assert!(is_neither("0_5"));
        assert!(is_neither("1_0.5"));
        assert!(is_neither("0.5_0"));
        assert!(is_neither("1e1_0"));
    }

    #[test]
    fn signed_and_plain_decimals_stay_integers() {
        assert!(is_int("42"));
        assert!(is_int("-7"));
        assert!(is_int("+5"));
        assert!(is_float("1.5"));
        assert!(is_float("1e2"));
        assert!(is_float(".5"));
        assert!(is_float(".inf"));
    }
}
