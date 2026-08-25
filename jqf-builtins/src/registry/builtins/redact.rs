//! The jqf REDACT family.
//!
//! Four pure value laws: `redact/0` replaces the WHOLE input value with the marker string `"[REDACTED]"`; `redact/1`
//! redacts only the substrings of a string matching a reference-flavored regex, leaving the rest intact; `redact/2` is
//! the same partial redaction with a caller-supplied marker; and `redact_keyed/1` derives a DETERMINISTIC pseudonym
//! token (`[REDACTED:<hex>]`) from HMAC-SHA256 of the value under the caller's key.
//!
//! Redaction is a VALUE operation: it never knows about keys, paths, or documents — the user reaches values with
//! `walk`/`paths`/`|=`, which already exist, so `redact` owns redaction and traversal owns traversal.
//! A redacted value is ALWAYS a string: a redacted number becoming a string is honest — pretending the original type
//! survived redaction would be a lie. Under `redact/1` and `redact/2` a NON-string input has no substrings to match, so
//! the whole value becomes the marker.
//!
//! The regex half routes through the engine's ONE compiled-regex cache ([`super::regex::substitution_matches`]), never
//! a per-call `Regex::new` — that mistake cost 154× once in this tree. The keyed mode reuses the extension family's
//! [`super::extension::hmac_sha256`] machinery and keeps NO stored mapping table: a table next to the redacted data is
//! a reversal oracle, which is worse than no redaction because it looks safe. Same input + same key → same token,
//! across runs, files, and machines; lose the key and the token is irreversible.

use alloc::format;
use alloc::string::String;
use alloc::string::ToString;

use jqf_data::Value;
use jqf_resource::ResourceContext;

use super::extension::hmac_sha256;
use super::id;
use super::regex::{self, RegexLaw};
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::path::raise;

/// The redaction law discriminants, one per evaluator shape.
#[derive(Clone, Copy, Debug)]
pub enum RedactLaw {
    /// `redact/0` — the whole value becomes the default marker.
    Whole,
    /// `redact/1` — matching substrings of a string become the default marker; a non-string input becomes the marker.
    Pattern,
    /// `redact/2` — like `Pattern`, with a caller-supplied marker.
    PatternMarker,
    /// `redact_keyed/1` — the value's canonical text under HMAC-SHA256 of the caller's key, truncated to a readable
    /// `[REDACTED:<hex>]` token.
    Keyed,
}

/// The default marker a whole-value redaction (and a partial redaction without a caller-supplied marker) publishes.
const DEFAULT_MARKER: &str = "[REDACTED]";

/// The number of HMAC-SHA256 digest bytes kept in a keyed token — 8 bytes is 64 bits of keyed pseudonym, enough to
/// group and correlate without turning the token into a payload.
const KEYED_TOKEN_BYTES: usize = 8;

/// One argument's string text, or a catchable refusal naming the argument.
fn expect_string_argument<'a>(
    args: &'a [Value],
    position: usize,
    refusal: &str,
    resources: &ResourceContext<'_>,
) -> Result<&'a str, EngineRunError> {
    match args.get(position) {
        Some(Value::String(text)) => Ok(text.as_str()),
        _ => Err(raise(refusal, resources)),
    }
}

/// One marker string from the /2 law's second argument (a non-string marker is a refusal, never a silent default —
/// the caller asked for a marker and the answer must be reproducible).
fn expect_marker<'a>(args: &'a [Value], resources: &ResourceContext<'_>) -> Result<&'a str, EngineRunError> {
    expect_string_argument(args, 1, "redact marker must be a string", resources)
}

/// The partial-redaction core: every reference-flavored regex match of `pattern` in the string input is replaced by
/// `marker`. A non-string input has no substrings to match, so the whole value becomes the marker.
fn redact_matches(
    subject: &Value,
    pattern: &Value,
    marker: &str,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let Value::String(text) = subject.untagged() else {
        // The "value operation" framing: a redacted value is not the original type, so a non-string becomes the marker
        // string outright.
        return Value::try_string(marker).map_err(|_| EngineRunError::allocation_failure());
    };
    // `Gsub2` forces the global flag, so every match is replaced, and the compile is the CACHED reference-flavored
    // compile (the `gsub` machinery) — never a per-call `Regex::new`.
    let matches = regex::substitution_matches(RegexLaw::Gsub2, subject, pattern, &Value::Null, resources)?;
    if matches.is_empty() {
        return Ok(Value::String(text.clone_shared()));
    }
    let mut out = String::new();
    let mut previous_end = 0;
    for matched in &matches {
        out.push_str(&text[previous_end..matched.start]);
        out.push_str(marker);
        previous_end = matched.end;
    }
    out.push_str(&text[previous_end..]);
    Value::try_string(&out).map_err(|_| EngineRunError::allocation_failure())
}

/// The canonical text a keyed token hashes: a string is itself, and any other value is its `tostring` rendering — the
/// same total-on-values rule the value operation promises, deterministic across runs and machines.
fn keyed_message(subject: &Value, resources: &ResourceContext<'_>) -> Result<String, EngineRunError> {
    match subject.untagged() {
        Value::String(text) => Ok(text.as_str().to_string()),
        other => match super::text::tostring(other, resources)? {
            Value::String(text) => Ok(text.as_str().to_string()),
            _ => Err(EngineRunError::internal_contract("tostring rendered a non-string")),
        },
    }
}

/// One redact-law evaluation for exactly one tuple: the piped `subject` (its whole value) and the argument tuple
/// `args`. The caller owns argument EVALUATION — it runs each parameter's filter over the call's input and calls this
/// law once per combination — so this function never reasons about cardinality.
///
/// # Errors
///
/// Returns a catchable refusal for a non-string pattern (`redact/1`, `redact/2`), a non-string marker (`redact/2`), a
/// non-string key (`redact_keyed/1`), a regex that fails to compile, or an allocation failure.
pub fn redact_law(
    law: RedactLaw,
    subject: &Value,
    args: &[Value],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match law {
        RedactLaw::Whole => Value::try_string(DEFAULT_MARKER).map_err(|_| EngineRunError::allocation_failure()),
        RedactLaw::Pattern => {
            let pattern = expect_string_argument(args, 0, "redact requires a pattern string argument", resources)?;
            let pattern_value = Value::try_string(pattern).map_err(|_| EngineRunError::allocation_failure())?;
            redact_matches(subject, &pattern_value, DEFAULT_MARKER, resources)
        }
        RedactLaw::PatternMarker => {
            let pattern = expect_string_argument(args, 0, "redact requires a pattern string argument", resources)?;
            let pattern_value = Value::try_string(pattern).map_err(|_| EngineRunError::allocation_failure())?;
            let marker = expect_marker(args, resources)?;
            redact_matches(subject, &pattern_value, marker, resources)
        }
        RedactLaw::Keyed => {
            let key = expect_string_argument(args, 0, "redact_keyed key must be a string", resources)?;
            let message = keyed_message(subject, resources)?;
            let digest = hmac_sha256(key.as_bytes(), message.as_bytes());
            let token = super::extension::hex_encode_bytes(&digest[..KEYED_TOKEN_BYTES]);
            let text = format!("[REDACTED:{token}]");
            Value::try_string(&text).map_err(|_| EngineRunError::allocation_failure())
        }
    }
}

// ------------------------------------------------------------------------
// Registry records.

const ONE_FILTER: &[ParameterKind] = &[ParameterKind::Filter];
const TWO_FILTERS: &[ParameterKind] = &[ParameterKind::Filter, ParameterKind::Filter];

const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "jqf-enrich",
        summary,
        detail,
    }
}

const fn example(program: &'static str, input: &'static str, expected: &'static str) -> BuiltinExample {
    BuiltinExample {
        program,
        input,
        expected,
    }
}

const fn overload(
    id: u16,
    family_id: u16,
    name: &'static str,
    arity: u8,
    parameters: &'static [ParameterKind],
    examples: &'static [BuiltinExample],
) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(id),
        family: BuiltinFamilyId::new(family_id),
        canonical_name: name,
        arity,
        parameters,
        execution: BuiltinExecution::Evaluator,
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: Effects::Pure,
        examples,
    }
}

const REDACT_FAMILY: BuiltinFamilyRecord = family(
    id::REDACT_FAMILY_ID,
    "redact",
    "Redacts a value: the whole value to a marker string, or matching substrings of a string.",
    "",
);
const REDACT_KEYED_FAMILY: BuiltinFamilyRecord = family(
    id::REDACT_KEYED_FAMILY_ID,
    "redact_keyed",
    "A deterministic pseudonym token ([REDACTED:hex]) keyed by HMAC-SHA256.",
    "",
);

pub const FAMILIES: &[BuiltinFamilyRecord] = &[REDACT_FAMILY, REDACT_KEYED_FAMILY];

const REDACT_0_OVERLOAD: BuiltinOverloadRecord = overload(
    id::REDACT_0,
    id::REDACT_FAMILY_ID,
    "redact",
    0,
    &[],
    &[
        example("redact", "\"alice@corp.com\"", "\"[REDACTED]\"\n"),
        // A redacted value is always a string: a number redacted to the marker string is the honest answer, never a lie
        // about the original type surviving.
        example("redact", "42", "\"[REDACTED]\"\n"),
        // The composition the value operation exists for: the user reaches the value with `|=`, `walk`, or `paths`, all
        // of which already exist.
        example(
            ".password |= redact",
            "{\"password\":\"s3cret\"}",
            "{\"password\":\"[REDACTED]\"}\n",
        ),
    ],
);
const REDACT_1_OVERLOAD: BuiltinOverloadRecord = overload(
    id::REDACT_1,
    id::REDACT_FAMILY_ID,
    "redact",
    1,
    ONE_FILTER,
    &[
        example("redact(\"^[^@]+\")", "\"alice@corp.com\"", "\"[REDACTED]@corp.com\"\n"),
        // The lookahead needs the reference-regex fallback tier, which the shared cache compiles — the family example
        // verbatim: every digit that still has four digits after it is redacted, so all but the last four become
        // markers.
        example(
            "redact(\"\\\\d(?=\\\\d{4})\")",
            "\"4111111111111111\"",
            "\"[REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED]1111\"\n",
        ),
        // A non-string input has no substrings to match; the whole value becomes the marker.
        example("redact(\"[^@]+\")", "42", "\"[REDACTED]\"\n"),
        // No match leaves the string untouched.
        example("redact(\"x\")", "\"hello\"", "\"hello\"\n"),
    ],
);
const REDACT_2_OVERLOAD: BuiltinOverloadRecord = overload(
    id::REDACT_2,
    id::REDACT_FAMILY_ID,
    "redact",
    2,
    TWO_FILTERS,
    &[
        example(
            "redact(\"^[^@]+\"; \"***\")",
            "\"alice@corp.com\"",
            "\"***@corp.com\"\n",
        ),
        example(
            "redact(\"[0-9]+\"; \"<n>\")",
            "\"order #42 for #7\"",
            "\"order #<n> for #<n>\"\n",
        ),
    ],
);
const REDACT_KEYED_OVERLOAD: BuiltinOverloadRecord = overload(
    id::REDACT_KEYED,
    id::REDACT_KEYED_FAMILY_ID,
    "redact_keyed",
    1,
    ONE_FILTER,
    &[
        // Same input + same key → same token, ALWAYS (HMAC-SHA256 of the value's canonical text under the key,
        // truncated to 64 bits).
        example(
            "redact_keyed(\"k\")",
            "\"alice@corp.com\"",
            "\"[REDACTED:21e875905b213b14]\"\n",
        ),
        // A different key is a different token — the keyed mode's whole point.
        example(
            "redact_keyed(\"other-key\")",
            "\"alice@corp.com\"",
            "\"[REDACTED:1783148f2cfa8e8c]\"\n",
        ),
        // A non-string value hashes its canonical `tostring` rendering, so the keyed mode is TOTAL on values like the
        // rest of the family.
        example("redact_keyed(\"k\")", "42", "\"[REDACTED:7955074f51169f1f]\"\n"),
    ],
);

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    REDACT_0_OVERLOAD,
    REDACT_1_OVERLOAD,
    REDACT_2_OVERLOAD,
    REDACT_KEYED_OVERLOAD,
];

/// The redact execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
/// The laws ride the extension family's argument-product drive; `registry::dispatch` wraps them into
/// `ExtensionLaw::Redact` at table build time.
pub const PAYLOADS: &[(u16, RedactLaw)] = &[
    (id::REDACT_0, RedactLaw::Whole),
    (id::REDACT_1, RedactLaw::Pattern),
    (id::REDACT_2, RedactLaw::PatternMarker),
    (id::REDACT_KEYED, RedactLaw::Keyed),
];

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(1).expect("work"),
        )
        .expect("resources")
    }

    fn string(text: &str, _resources: &ResourceContext<'static>) -> Value {
        Value::try_string(text).expect("string")
    }

    fn text(value: &Value) -> &str {
        match value.untagged() {
            Value::String(text) => text.as_str(),
            _ => panic!("expected a string"),
        }
    }

    #[test]
    fn whole_redact_is_the_marker_for_every_kind() {
        let resources = resources();
        for input in [
            string("alice@corp.com", &resources),
            Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(42))),
            Value::Bool(true),
            Value::Null,
        ] {
            let out = redact_law(RedactLaw::Whole, &input, &[], &resources).expect("redact/0");
            assert_eq!(text(&out), "[REDACTED]");
        }
    }

    #[test]
    fn pattern_redact_replaces_every_match() {
        let resources = resources();
        // The family example, with the pattern corrected: a bare `[^@]+` is GREEDY, so it also matches `corp.com` (the
        // reference's own gsub law); anchoring the local part is what redacts only the left of the `@`.
        let out = redact_law(
            RedactLaw::Pattern,
            &string("alice@corp.com", &resources),
            &[string("^[^@]+", &resources)],
            &resources,
        )
        .expect("redact/1");
        assert_eq!(text(&out), "[REDACTED]@corp.com");
        // Every maximal match is replaced: the unanchored class hits both spans.
        let out = redact_law(
            RedactLaw::Pattern,
            &string("alice@corp.com", &resources),
            &[string("[^@]+", &resources)],
            &resources,
        )
        .expect("redact/1");
        assert_eq!(text(&out), "[REDACTED]@[REDACTED]");
        // The lookahead example: every digit with four digits still to come is redacted, so all but the last four
        // become markers.
        let out = redact_law(
            RedactLaw::Pattern,
            &string("4111111111111111", &resources),
            &[string("\\d(?=\\d{4})", &resources)],
            &resources,
        )
        .expect("redact/1");
        assert_eq!(
            text(&out),
            "[REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED][REDACTED]1111"
        );
        // No match leaves the string untouched.
        let out = redact_law(
            RedactLaw::Pattern,
            &string("hello", &resources),
            &[string("x", &resources)],
            &resources,
        )
        .expect("redact/1");
        assert_eq!(text(&out), "hello");
        // A non-string input becomes the marker.
        let out = redact_law(
            RedactLaw::Pattern,
            &Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(42))),
            &[string("[^@]+", &resources)],
            &resources,
        )
        .expect("redact/1");
        assert_eq!(text(&out), "[REDACTED]");
    }

    #[test]
    fn custom_marker_is_honored() {
        let resources = resources();
        let out = redact_law(
            RedactLaw::PatternMarker,
            &string("order #42 for #7", &resources),
            &[string("[0-9]+", &resources), string("<n>", &resources)],
            &resources,
        )
        .expect("redact/2");
        assert_eq!(text(&out), "order #<n> for #<n>");
    }

    #[test]
    fn keyed_tokens_are_deterministic_and_keyed() {
        let resources = resources();
        let first = redact_law(
            RedactLaw::Keyed,
            &string("alice@corp.com", &resources),
            &[string("k", &resources)],
            &resources,
        )
        .expect("keyed");
        let second = redact_law(
            RedactLaw::Keyed,
            &string("alice@corp.com", &resources),
            &[string("k", &resources)],
            &resources,
        )
        .expect("keyed again");
        assert_eq!(text(&first), text(&second));
        assert_eq!(text(&first), "[REDACTED:21e875905b213b14]");
        // A different key is a different token.
        let other = redact_law(
            RedactLaw::Keyed,
            &string("alice@corp.com", &resources),
            &[string("other-key", &resources)],
            &resources,
        )
        .expect("keyed with another key");
        assert_eq!(text(&other), "[REDACTED:1783148f2cfa8e8c]");
        assert_ne!(text(&first), text(&other));
    }

    #[test]
    fn keyed_token_covers_non_string_values() {
        let resources = resources();
        let out = redact_law(
            RedactLaw::Keyed,
            &Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(42))),
            &[string("k", &resources)],
            &resources,
        )
        .expect("keyed over a number");
        assert_eq!(text(&out), "[REDACTED:7955074f51169f1f]");
    }
}
