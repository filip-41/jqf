//! The jqf schema family: `schema_infer/1`, `schema_validate/2`, `schema_errors/2`, and `schema_diff/2`.
//!
//! The old-base implementation is a `Replace` source: its INFERENCE result shapes and recursive profile merge are
//! reused, but the validation half is rebuilt to the jqf profile contract in `docs/architecture/builtin-library.md`
//! §"Schema":
//!
//! > The jqf value-schema profile uses JSON Schema 2020-12 core and validation
//! > vocabularies, local `$ref` and `$defs`, and deterministic format
//! > annotations. The jqf value-model vocabulary
//! > ([`JQF_VALUE_MODEL_VOCABULARY`], `urn:jqf:value-model:1`) extends the
//! > `type` names with the kinds JSON cannot spell (`bytes`, the four temporal
//! > kinds, and the exact `decimal` category, the last validation-only) and
//! > adds the `tag`/`payload` keywords for tagged values; an unknown declared
//! > vocabulary is an explicit error. Remote references require an explicit
//! > extension. Validation errors are ordered objects containing
//! > `instance_path`, `schema_path`, `keyword`, and `message`.
//!
//! ## The three laws (plus the drift verb)
//!
//! - `schema_infer(VALUE)` evaluates the VALUE argument over the input and infers a JSON Schema 2020-12 document for
//!   it: a scalar's `{"type": ...}` (integer folded into `number` when an integer and a non-integer number merge), an
//!   array's `{"type":"array","items": ...}` (items only when non-empty), and an object's
//!   `{"type":"object","required": [...], "properties": {...}}` with the keys SORTED for determinism. A value that
//!   carries a value-model kind infers its STRING projection — a TOML datetime infers
//!   `{"type":"string","format":"date-time", "jqf:kind":"offset-datetime"}` — so the emitted schema stays inside
//!   standard 2020-12 vocabulary and an external validator sees `type: "string"` with annotations it ignores. A TAGGED
//!   value keeps the `$schema`/`$vocabulary` header naming the registered value-model vocabulary (its `tag`/`payload`
//!   keywords are that vocabulary's); a kind-only schema carries no header.
//! - `schema_infer(VALUE; OPTIONS)` is the same inference with opt-in
//!   dials; `schema_infer(VALUE; {})` is byte-identical to `/1`.
//!   `strings: "numeric"` types a string through the tonumber scan:
//!   every merged value an integer emits `integer`, every value a number emits `number`, any refuse stays `string`.
//!   Decode is unchanged — CSV cells remain strings; only this dial may treat their text as a number.
//! - `schema_validate(VALUE; SCHEMA)` evaluates both arguments and returns
//!   `true` when the value validates against the schema and `false` otherwise.
//! - `schema_errors(VALUE; SCHEMA)` evaluates both arguments and returns the
//!   ORDERED validation error objects — `{"instance_path", "schema_path", "keyword", "message"}` — as one array.
//! - `schema_diff(VALUE; SCHEMA)` infers the VALUE's schema, NORMALIZES both schema documents (local `$ref`s resolved,
//!   `required` sorted, `type` unions canonicalized), and runs the diff walker over the pair with the ONE schema-aware
//!   rule that `required`/`enum` arrays compare as SETS. When the FIRST argument is ITSELF a schema document (an object
//!   carrying a profile keyword), both sides are read as schemas and compared directly: evolution detection
//!   (`schema_diff(old_contract; new_contract)`) with path-keyed semantic deltas and identical documents diffing to an
//!   empty array. The output is diff/2's record shape; `added` = the second side lacks structure the first describes,
//!   `removed` = the first side promises structure the second lacks, `changed` = both describe it differently.
//!   No subsumption reasoning — that is a schema-algebra engine, deliberately out.
//!
//! Both validation laws share the walker below.
//!
//! ## The supported profile
//!
//! Supported keywords (2020-12 core + validation + applicator, with `format` and the annotation vocabulary accepted as
//! deterministic annotations):
//! `$schema`, `$id`, `$comment`, `$anchor`, `$defs`, `$ref`, `$vocabulary`, `type`, `enum`, `const`, `multipleOf`,
//! `maximum`, `exclusiveMaximum`, `minimum`, `exclusiveMinimum`, `maxLength`, `minLength`, `pattern`, `maxItems`,
//! `minItems`, `uniqueItems`, `maxContains`, `minContains`, `maxProperties`, `minProperties`, `required`,
//! `dependentRequired`, `allOf`, `anyOf`, `oneOf`, `not`, `if`, `then`, `else`, `dependentSchemas`, `prefixItems`,
//! `items`, `contains`, `properties`, `patternProperties`, `additionalProperties`, `propertyNames`, `format`, `title`,
//! `description`, `default`, `examples`, `deprecated`, `readOnly`, `writeOnly`.
//!
//! `$vocabulary` is SUPPORTED as a declaration: every declared URI must be in the registered set — the five 2020-12
//! vocabularies above plus [`JQF_VALUE_MODEL_VOCABULARY`] — and an unknown URI raises the catchable `unsupported
//! schema vocabulary` error naming the URI, never silently ignored. The value-model vocabulary extends the `type` names
//! with `bytes`, the four temporal kinds (`localdate`, `localtime`, `localdatetime`, `offsetdatetime`), and the exact
//! `decimal` category, and adds the `tag` (outermost non-core tag text) and `payload` (validate beneath the tag)
//! keywords. The extended type names are accepted whether or not the vocabulary is declared (jqf knows them).
//!
//! The kind law sharpens the `type`/`jqf:kind` split:
//! `type: "string"` accepts the PORTABLE PROJECTION — an owned string, bytes, and all four temporal kinds — so a
//! schema written against the projection keeps validating a TOML datetime or a CBOR byte string. The exact kind is
//! asserted by `jqf:kind`, never by `type`: a `{"jqf:kind":"local-date"}` schema rejects a plain string that merely
//! looks like a date, and rejects an `OffsetDateTime` outright. `format` and `contentEncoding` are `ANNOTATIONS`
//! (accepted, never asserted), so the standard keywords' 2020-12 semantics never fork.
//!
//! A KNOWN-UNSUPPORTED vocabulary keyword (`unevaluatedItems`, `unevaluatedProperties`, the dynamic/recursive-ref
//! family, and the content vocabulary MINUS `contentEncoding`, which is a supported annotation because the bytes kind's
//! inferred schema carries it) is an EXPLICIT catchable error, never silently ignored — that is the profile's
//! "unsupported vocabulary is an explicit error" law. Unknown/custom keywords are treated as annotations (JSON Schema
//! 2020-12 semantics), so a vendor extension keyword does not fail a schema.
//!
//! `format` is a DETERMINISTIC ANNOTATION, not an assertion: it is accepted, but `{"format":"date"}` passes `"nope"`
//! and an unknown format name like `"weird"` is accepted silently — both spec-correct for 2020-12, where
//! format-assertion is opt-in. This is the one place where the "unsupported vocabulary is an explicit error" law and
//! the annotation behaviour read differently: `format` is SUPPORTED as an annotation, so no unsupported vocabulary
//! error fires for it.
//!
//! `$ref` is LOCAL ONLY: `#` is the document, `#/...` is a JSON pointer into it, and `#name` is a plain-name `$anchor`.
//! Any other form (a URI, a relative reference) raises `remote $ref requires an explicit extension`.
//! Following a `$ref` replaces the schema AND keeps validating the current schema's own sibling keywords (2020-12:
//! `$ref` is an applicator, not a replacement), and `$defs` is reachable by its `#/$defs/<name>` pointer.
//!
//! ## Paths and ordering
//!
//! `instance_path` is an RFC 6901 JSON pointer with `$` for the root (`$`, `$/items/0/name`); `schema_path` is the same
//! with `#` for the root (`#`, `#/properties/name/type`). Both escape `~` → `~0` and `/` → `~1`.
//! Errors are collected in a fixed, documented keyword order at each schema node, depth-first, so the output is
//! deterministic for any input.
//!
//! Schema recursion is bounded by ONE shared depth ceiling: every recursive step increments it — applicator descent
//! into properties/items, their values, and `$ref` hops alike — so both a `$ref` cycle and an extraordinarily deeply
//! nested schema or instance raise the catchable `schema recursion too deep` rather than overflowing the stack.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec;
use alloc::vec::Vec;

use jqf_data::{Array, Number, NumberCategory, Object, ObjectBuilder, ObjectKey, Value};
use jqf_resource::ResourceContext;

use super::diff::{ArrayCompare, diff_law_with};
use super::id;
use super::regex::compile_plain_regex;
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::order::{NanLaw, compare, semantic_eq, total_cmp};
use crate::semantics::path::raise;
use crate::semantics::scan;
use jqf_data::BigInt;

/// The catchable ceiling for the schema walk's depth counter.
///
/// The counter is SHARED by every recursive step, not only `$ref` hops:
/// applicators (`properties`/`items`), their values, and ref resolution all increment it. A legitimately nested schema
/// or instance can therefore reach the same 10,000 the codec's own nesting law uses, without any `$ref` cycle — at
/// which point the walk raises the catchable `schema recursion too deep` instead of overflowing the stack.
const SCHEMA_RECURSION_LIMIT: usize = 10_000;

/// The registered value-model vocabulary: the jqf extension that names the value-model kinds JSON cannot spell —
/// `bytes`, the four temporal kinds, the exact `decimal` category, and the `tag`/`payload` keywords for tagged values.
/// A jqf value schema carrying it is still a valid JSON Schema 2020-12 document: an external validator ignores the URI
/// and validates the core keywords. jqf REFUSES any `$vocabulary` URI outside the registered set (the five 2020-12
/// vocabularies it implements plus this one), never silently ignoring a vocabulary it does not know.
pub const JQF_VALUE_MODEL_VOCABULARY: &str = "urn:jqf:value-model:1";

/// The 2020-12 vocabularies jqf implements, as `$vocabulary` keys.
const CORE_VOCABULARY: &str = "https://json-schema.org/draft/2020-12/vocab/core";
const APPLICATOR_VOCABULARY: &str = "https://json-schema.org/draft/2020-12/vocab/applicator";
const VALIDATION_VOCABULARY: &str = "https://json-schema.org/draft/2020-12/vocab/validation";
const META_DATA_VOCABULARY: &str = "https://json-schema.org/draft/2020-12/vocab/meta-data";
const FORMAT_ANNOTATION_VOCABULARY: &str = "https://json-schema.org/draft/2020-12/vocab/format-annotation";

/// The vocabulary URIs jqf knows. `$vocabulary` may declare exactly these.
fn is_known_vocabulary(uri: &str) -> bool {
    matches!(
        uri,
        CORE_VOCABULARY
            | APPLICATOR_VOCABULARY
            | VALIDATION_VOCABULARY
            | META_DATA_VOCABULARY
            | FORMAT_ANNOTATION_VOCABULARY
            | JQF_VALUE_MODEL_VOCABULARY
    )
}

/// One schema law per jqf overload.
#[derive(Clone, Copy, Debug)]
pub enum SchemaLaw {
    /// `schema_infer/1`: infer a JSON Schema 2020-12 document for a value.
    Infer,
    /// `schema_infer/2`: infer with strictness OPTIONS: each dial selects which CORE keywords inference emits —
    /// arrays `items`|`length` (adds `minItems`/`maxItems`), required `observed`|`none`, numbers `loose`|`bounds` (adds
    /// `minimum`/`maximum`), strings `type`|`enum`|`numeric`. `numeric` types a string through the same scan `tonumber`
    /// uses: every merged value an integer emits `integer`, every value a number emits `number`, any refuse stays
    /// `string`. The default option set is byte-identical to `schema_infer/1`.
    InferWithOptions,
    /// `schema_validate/2`: validate, return `true`/`false`.
    Validate,
    /// `schema_errors/2`: validate, return the ordered error objects.
    Errors,
    /// `schema_diff/2`: the drift verb — infer the VALUE's schema, normalize both schema documents (local `$ref`s
    /// resolved, `required` sorted, `type` unions canonicalized), and run the diff walker over the pair with the ONE
    /// schema-aware rule that `required` and `enum` arrays compare as SETS.
    Diff,
}

const fn example(program: &'static str, input: &'static str, expected: &'static str) -> BuiltinExample {
    BuiltinExample {
        program,
        input,
        expected,
    }
}

const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "jqf-extension",
        summary,
        detail,
    }
}

/// The `schema_infer` family record.
pub const SCHEMA_INFER_FAMILY: BuiltinFamilyRecord = family(
    id::SCHEMA_INFER_FAMILY_ID,
    "schema_infer",
    "Infer a JSON Schema 2020-12 document for a value.",
    "Evaluate the VALUE argument and build a deterministic schema describing \
     it: scalar types, array items, and object properties with sorted keys.",
);

/// The `schema_validate` family record.
pub const SCHEMA_VALIDATE_FAMILY: BuiltinFamilyRecord = family(
    id::SCHEMA_VALIDATE_FAMILY_ID,
    "schema_validate",
    "Validate a value against a JSON Schema 2020-12 value-schema document.",
    "Evaluate VALUE and SCHEMA and return true when the value satisfies the \
     schema. Local $ref and $defs are supported; remote $ref and unsupported \
     vocabulary are explicit errors. `format` is a deterministic ANNOTATION, \
     never an assertion: `{\"format\":\"date\"}` passes `\"not-a-date\"` (the \
     registered examples pin this).",
);

/// The `schema_errors` family record.
pub const SCHEMA_ERRORS_FAMILY: BuiltinFamilyRecord = family(
    id::SCHEMA_ERRORS_FAMILY_ID,
    "schema_errors",
    "Validate and return the ordered schema-validation errors.",
    "Like schema_validate, but returns the ordered error objects \
     ({instance_path, schema_path, keyword, message}) instead of a boolean. \
     `format` never contributes an error: a format-only value schema reports \
     an empty array for any value (the registered examples pin this).",
);

/// The `schema_diff` family record.
pub const SCHEMA_DIFF_FAMILY: BuiltinFamilyRecord = family(
    id::SCHEMA_DIFF_FAMILY_ID,
    "schema_diff",
    "Drift between a value and the schema that contracts it, or between two schemas.",
    "With bare data as the first argument: infer that value's schema, \
     normalize both documents (local $refs resolved, required sorted, type \
     unions canonicalized), and diff them with the ONE schema-aware rule \
     that required and enum arrays compare as SETS. With a schema document \
     as the first argument (an object carrying a profile keyword): both \
     sides are compared AS schemas — contract-evolution detection with \
     path-keyed semantic deltas; identical documents diff to an empty array. \
     The output is diff/2's record shape: added = the second side lacks \
     structure the first describes, removed = the first side promises \
     structure the second lacks, changed = both describe it differently. No \
     subsumption reasoning: {\"type\":\"number\"} vs {\"minimum\":0} is changed, \
     never compatible.",
);

/// The `schema_infer/1` overload record.
pub const SCHEMA_INFER: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SCHEMA_INFER),
    family: BuiltinFamilyId::new(id::SCHEMA_INFER_FAMILY_ID),
    canonical_name: "schema_infer",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        example(
            "schema_infer(.)",
            r#"{"b":"x","a":{"n":1,"z":null}}"#,
            concat!(
                r#"{"type":"object","required":["a","b"],"properties":{"a":{"type":"object","required":["n","z"],"properties":{"n":{"type":"integer"},"z":{"type":"null"}}},"b":{"type":"string"}}}"#,
                "\n"
            ),
        ),
        example(
            "schema_infer(.)",
            r#"[{"id":1,"name":"Ada"},{"id":2}]"#,
            concat!(
                r#"{"type":"array","items":{"type":"object","required":["id"],"properties":{"id":{"type":"integer"},"name":{"type":"string"}}}}"#,
                "\n"
            ),
        ),
        example(
            "schema_infer([1, 1.5])",
            "0",
            concat!(r#"{"type":"array","items":{"type":"number"}}"#, "\n"),
        ),
        // A numeric-looking string stays string: decode never coerces, and `/1` does not turn the tonumber scan on.
        example(
            "schema_infer(.)",
            r#"{"age":"37"}"#,
            concat!(
                r#"{"type":"object","required":["age"],"properties":{"age":{"type":"string"}}}"#,
                "\n"
            ),
        ),
    ],
};

/// The `schema_infer/2` overload record: the same inference, with strictness OPTIONS selecting which CORE keywords are
/// emitted.
pub const SCHEMA_INFER_2: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SCHEMA_INFER_2),
    family: BuiltinFamilyId::new(id::SCHEMA_INFER_FAMILY_ID),
    canonical_name: "schema_infer",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        // The default option set reproduces schema_infer/1 byte for byte.
        example(
            "schema_infer(.; {})",
            r#"{"b":"x","a":{"n":1}}"#,
            concat!(
                r#"{"type":"object","required":["a","b"],"properties":{"a":{"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}},"b":{"type":"string"}}}"#,
                "\n"
            ),
        ),
        // arrays=length adds minItems/maxItems (observed bounds).
        example(
            "schema_infer([1,2,3]; {\"arrays\":\"length\"})",
            "0",
            concat!(
                r#"{"type":"array","minItems":3,"maxItems":3,"items":{"type":"integer"}}"#,
                "\n"
            ),
        ),
        // required=none omits the required keyword.
        example(
            "schema_infer(.; {\"required\":\"none\"})",
            r#"{"a":1}"#,
            concat!(r#"{"type":"object","properties":{"a":{"type":"integer"}}}"#, "\n"),
        ),
        // numbers=bounds adds minimum/maximum; strings=enum widens a pure-string profile to the observed values.
        example(
            "schema_infer(.; {\"numbers\":\"bounds\",\"strings\":\"enum\"})",
            r#"{"n":3,"s":"b"}"#,
            concat!(
                r#"{"type":"object","required":["n","s"],"properties":{"n":{"type":"integer","minimum":3,"maximum":3},"s":{"enum":["b"]}}}"#,
                "\n"
            ),
        ),
        // strings=numeric types a tonumber-accepted string as integer/number.
        // A CSV cell is still a string at decode; this dial is the only place inference may treat its text as a number.
        example(
            "schema_infer(.; {\"strings\":\"numeric\"})",
            r#"{"age":"37"}"#,
            concat!(
                r#"{"type":"object","required":["age"],"properties":{"age":{"type":"integer"}}}"#,
                "\n"
            ),
        ),
        example(
            "schema_infer(.; {\"strings\":\"numeric\"})",
            r#"{"age":"37x"}"#,
            concat!(
                r#"{"type":"object","required":["age"],"properties":{"age":{"type":"string"}}}"#,
                "\n"
            ),
        ),
        // A mixed column — one cell refuses the scan — stays string.
        example(
            "schema_infer(.; {\"strings\":\"numeric\"})",
            r#"[{"age":"37"},{"age":"x"}]"#,
            concat!(
                r#"{"type":"array","items":{"type":"object","required":["age"],"properties":{"age":{"type":"string"}}}}"#,
                "\n"
            ),
        ),
        // Actual JSON numbers are unchanged under the dial.
        example(
            "schema_infer([1, 1.5]; {\"strings\":\"numeric\"})",
            "0",
            concat!(r#"{"type":"array","items":{"type":"number"}}"#, "\n"),
        ),
    ],
};

/// The `schema_validate/2` overload record.
pub const SCHEMA_VALIDATE: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SCHEMA_VALIDATE),
    family: BuiltinFamilyId::new(id::SCHEMA_VALIDATE_FAMILY_ID),
    canonical_name: "schema_validate",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        example(
            "schema_validate(.; {\"type\":\"object\",\"required\":[\"id\"],\"properties\":{\"id\":{\"type\":\"integer\"},\"name\":{\"type\":\"string\"}}})",
            r#"{"id":1,"name":"Ada"}"#,
            "true\n",
        ),
        example(
            "schema_validate(.; {\"type\":\"object\",\"required\":[\"id\"]})",
            r#"{"name":"Ada"}"#,
            "false\n",
        ),
        example(
            "schema_validate([1,2]; {\"type\":\"array\",\"items\":{\"type\":\"integer\"}})",
            "0",
            "true\n",
        ),
        // `format` is a deterministic ANNOTATION in the jqf value-schema profile (JSON Schema 2020-12 semantics):
        // `{"format":"date"}` passes any string, never asserted. This example pins the surprise so a change cannot
        // silently make `format` assert and diverge from every other 2020-12 validator.
        example(
            "schema_validate(\"not-a-date\"; {\"type\":\"string\",\"format\":\"date\"})",
            "0",
            "true\n",
        ),
    ],
};

/// The `schema_errors/2` overload record.
pub const SCHEMA_ERRORS: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SCHEMA_ERRORS),
    family: BuiltinFamilyId::new(id::SCHEMA_ERRORS_FAMILY_ID),
    canonical_name: "schema_errors",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        example(
            "schema_errors(.; {\"type\":\"object\",\"required\":[\"id\"],\"properties\":{\"name\":{\"type\":\"string\"}}})",
            r#"{"name":7}"#,
            concat!(
                r##"[{"instance_path":"$","schema_path":"#/required","keyword":"required","message":"missing required property \"id\""},"##,
                r##"{"instance_path":"$/name","schema_path":"#/properties/name/type","keyword":"type","message":"expected type string"}]"##,
                "\n"
            ),
        ),
        example(
            "schema_errors(3; {\"minimum\":4})",
            "0",
            concat!(
                r##"[{"instance_path":"$","schema_path":"#/minimum","keyword":"minimum","message":"must be >= 4"}]"##,
                "\n"
            ),
        ),
        example(
            "schema_errors(.; {\"type\":\"object\",\"properties\":{\"items\":{\"type\":\"array\",\"items\":{\"type\":\"object\",\"required\":[\"sku\"],\"properties\":{\"sku\":{\"type\":\"string\"}}}}}})",
            r#"{"items":[{"sku":"a"},{"sku":2}]}"#,
            concat!(
                r##"[{"instance_path":"$/items/1/sku","schema_path":"#/properties/items/items/properties/sku/type","keyword":"type","message":"expected type string"}]"##,
                "\n"
            ),
        ),
        // The annotation-only half: a format-only value schema never reports a violation, so `schema_errors` answers an
        // empty array for a value that fails no keyword except the (non-asserting) `format`.
        example(
            "schema_errors(\"not-a-date\"; {\"type\":\"string\",\"format\":\"date\"})",
            "0",
            "[]\n",
        ),
    ],
};

/// The `schema_diff/2` overload record: with bare data as the first argument, infer that VALUE argument's schema; with
/// a schema document as the first argument, compare both sides AS schemas. Both documents are normalized, then diffed
/// with the set-keyword rule. Same record shape and same demand class as `diff/2` — both read the whole value and the
/// whole schema.
pub const SCHEMA_DIFF: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::SCHEMA_DIFF),
    family: BuiltinFamilyId::new(id::SCHEMA_DIFF_FAMILY_ID),
    canonical_name: "schema_diff",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        // Additive drift: the value has a member the schema does not describe (the file's inferred schema has it; the
        // contract does not — `added`), and its required set supersets the contract's (`changed`, sets).
        example(
            "schema_diff(.; {\"type\":\"object\",\"required\":[\"id\"],\"properties\":{\"id\":{\"type\":\"integer\"}}})",
            r#"{"id":1,"name":"Ada"}"#,
            concat!(
                r##"[{"path":["properties","name"],"kind":"added","new":{"type":"string"}},{"path":["required"],"kind":"changed","old":["id"],"new":["id","name"]}]"##,
                "\n"
            ),
        ),
        // Breaking drift: the schema promises a required member the file lacks (`removed`) and a required member the
        // file's set does not carry (`changed`).
        example(
            "schema_diff(.; {\"type\":\"object\",\"required\":[\"id\",\"name\"],\"properties\":{\"id\":{\"type\":\"integer\"},\"name\":{\"type\":\"string\"}}})",
            r#"{"id":1}"#,
            concat!(
                r##"[{"path":["properties","name"],"kind":"removed","old":{"type":"string"}},{"path":["required"],"kind":"changed","old":["id","name"],"new":["id"]}]"##,
                "\n"
            ),
        ),
        // Kind drift: a plain string on the file side where the contract demands a temporal — the contract's
        // `jqf:kind` assertion has no counterpart in the file's inferred schema (`removed`; temporal ≠ string, the 2b
        // kind law).
        // The required sets agree, so no noise.
        example(
            "schema_diff(.; {\"type\":\"object\",\"required\":[\"at\"],\"properties\":{\"at\":{\"type\":\"string\",\"jqf:kind\":\"offset-datetime\"}}})",
            r#"{"at":"2024-01-01T00:00:00Z"}"#,
            concat!(
                r##"[{"path":["properties","at","jqf:kind"],"kind":"removed","old":"offset-datetime"}]"##,
                "\n"
            ),
        ),
        // The identity: a value against its own inferred schema diffs to nothing.
        example("schema_diff(.; schema_infer(.))", r#"{"id":1,"name":"Ada"}"#, "[]\n"),
        // Two schema documents compare AS schemas — identical schemas diff to an empty array, a type change is one
        // path-keyed record at the delta itself.
        example(
            r#"schema_diff({"type":"integer","required":["id"]}; {"type":"integer","required":["id"]})"#,
            "0",
            "[]\n",
        ),
        example(
            r#"schema_diff({"type":"integer"}; {"type":"string"})"#,
            "0",
            concat!(
                r##"[{"path":["type"],"kind":"changed","old":"integer","new":"string"}]"##,
                "\n"
            ),
        ),
    ],
};

/// The overload and family slices the registry aggregates.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    SCHEMA_INFER_FAMILY,
    SCHEMA_VALIDATE_FAMILY,
    SCHEMA_ERRORS_FAMILY,
    SCHEMA_DIFF_FAMILY,
];
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    SCHEMA_INFER,
    SCHEMA_INFER_2,
    SCHEMA_VALIDATE,
    SCHEMA_ERRORS,
    SCHEMA_DIFF,
];

/// The schema execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, SchemaLaw)] = &[
    (id::SCHEMA_INFER, SchemaLaw::Infer),
    (id::SCHEMA_INFER_2, SchemaLaw::InferWithOptions),
    (id::SCHEMA_VALIDATE, SchemaLaw::Validate),
    (id::SCHEMA_ERRORS, SchemaLaw::Errors),
    (id::SCHEMA_DIFF, SchemaLaw::Diff),
];

// --------------------------------------------------------------------------- schema_infer: the profile inference
// (reused result shapes + recursive merge) ---------------------------------------------------------------------------

/// One inferred type-profile for a value, before it is rendered to a schema.
///
/// The flag fields are the union of every merged value's kinds; `array` and `object` carry the merged container
/// profiles. This is the historical `SchemaProfile` with its recursive merge, ported to the jqf-data types.
#[derive(Clone, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one boolean per JSON Schema 2020-12 type in the merged profile; a state machine \
              would hide the union semantics the recursive merge needs"
)]
struct Profile {
    null: bool,
    boolean: bool,
    integer: bool,
    number: bool,
    string: bool,
    array: bool,
    array_items: Option<Box<Profile>>,
    object: Option<Box<ObjectProfile>>,
    /// The value-model kinds: `bytes` and the four temporal kinds. Each infers the STRING projection with its exact
    /// kind in a `jqf:kind` annotation, so the flags decide the annotation only when EXACTLY one kind was observed and
    /// no plain string. (The exact `decimal` type is registered as validation-only — the JSON codec produces exact
    /// decimals, so inference never emits it; see the Number arm.)
    bytes: bool,
    localdate: bool,
    localtime: bool,
    localdatetime: bool,
    offsetdatetime: bool,
    /// The OUTERMOST non-core tag text, when every merged value carries the SAME outermost tag (the `tag` keyword).
    /// `None` when no tag was observed or merged values disagree.
    tag: Option<String>,
    /// Whether any observed value carried a TAG (kinds alone no longer set it) — the flag that makes the ROOT schema
    /// carry the value-model vocabulary header, which the `tag`/`payload` keywords are declared under. A kind-only
    /// schema carries no header: its `jqf:kind` annotation is plain 2020-12 for foreign validators.
    extended: bool,
    /// The observed array-length bounds `(min, max)`, when the `arrays` option is `length` — emitted as
    /// `minItems`/`maxItems`.
    array_bounds: Option<(usize, usize)>,
    /// The observed numeric bounds `(min, max)`, when the `numbers` option is `bounds` — emitted as
    /// `minimum`/`maximum`.
    number_bounds: Option<(Number, Number)>,
    /// The observed string values, when the `strings` option is `enum` — emitted as `enum` in place of `type:
    /// "string"` for a pure-string profile.
    string_enum: Option<BTreeSet<String>>,
    /// Set when this profile (or a merge source) typed a string as integer/number because `strings` is `numeric` and
    /// the tonumber scan accepted the text. A refused sibling string then collapses the property back to `string` — a
    /// mixed column stays string.
    from_numeric_string: bool,
}

/// The merged profile of an object: properties (union, recursive merge) and required keys (INTERSECTION, so a key
/// required in every merged object stays required).
#[derive(Clone, Default)]
struct ObjectProfile {
    properties: BTreeMap<String, Profile>,
    required: Vec<String>,
}

/// The `schema_infer/2` strictness dials. Each dial selects which CORE JSON Schema keywords inference emits; no new
/// vocabulary exists.
#[derive(Clone, Copy, Debug)]
pub struct InferOptions {
    arrays: ArrayDial,
    required: RequiredDial,
    numbers: NumbersDial,
    strings: StringsDial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArrayDial {
    /// Today's behaviour: the element profile only.
    Items,
    /// Adds `minItems`/`maxItems` from the observed lengths.
    Length,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequiredDial {
    /// Today's behaviour: every observed key.
    Observed,
    /// Omits the `required` keyword entirely.
    None,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumbersDial {
    /// Today's behaviour: `integer`/`number` only.
    Loose,
    /// Adds `minimum`/`maximum` from the observed values.
    Bounds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StringsDial {
    /// Today's behaviour: `type: "string"`.
    Type,
    /// A pure-string profile becomes the observed values' `enum`.
    Enum,
    /// A string is typed through the tonumber scan: every merged value an integer emits `integer`, every value a number
    /// emits `number`, any refuse stays `string`. Decode is unchanged — CSV cells remain strings; only inference may
    /// treat their text as a number.
    Numeric,
}

impl InferOptions {
    /// Today's behaviour: `schema_infer/1` with no dials turned on.
    const DEFAULT: Self = Self {
        arrays: ArrayDial::Items,
        required: RequiredDial::Observed,
        numbers: NumbersDial::Loose,
        strings: StringsDial::Type,
    };

    /// Parses the OPTIONS argument of `schema_infer/2`. The value must be an object; a non-object, an unknown key, or
    /// an unknown dial value is a catchable error — a typo must not silently change the inferred schema.
    pub fn parse(options: &Value, resources: &ResourceContext<'_>) -> Result<Self, EngineRunError> {
        let Value::Object(object) = options.untagged() else {
            return Err(raise("schema_infer options must be an object", resources));
        };
        let mut parsed = Self::DEFAULT;
        for entry in object {
            let key = entry.key();
            let Value::String(value) = entry.value().untagged() else {
                return Err(raise(
                    &format!("schema_infer option `{key}` must be a string"),
                    resources,
                ));
            };
            match key {
                "arrays" => parsed.arrays = parse_array_dial(value, resources)?,
                "required" => parsed.required = parse_required_dial(value, resources)?,
                "numbers" => parsed.numbers = parse_numbers_dial(value, resources)?,
                "strings" => parsed.strings = parse_strings_dial(value, resources)?,
                _ => {
                    return Err(raise(
                        &format!(
                            "schema_infer options: unknown key `{key}` (known: arrays, \
                             required, numbers, strings)"
                        ),
                        resources,
                    ));
                }
            }
        }
        Ok(parsed)
    }
}

/// One dial value against its closed choice set.
fn parse_array_dial(value: &str, resources: &ResourceContext<'_>) -> Result<ArrayDial, EngineRunError> {
    match value {
        "items" => Ok(ArrayDial::Items),
        "length" => Ok(ArrayDial::Length),
        _ => Err(raise(
            "schema_infer option `arrays` must be one of items | length",
            resources,
        )),
    }
}

fn parse_required_dial(value: &str, resources: &ResourceContext<'_>) -> Result<RequiredDial, EngineRunError> {
    match value {
        "observed" => Ok(RequiredDial::Observed),
        "none" => Ok(RequiredDial::None),
        _ => Err(raise(
            "schema_infer option `required` must be one of observed | none",
            resources,
        )),
    }
}

fn parse_numbers_dial(value: &str, resources: &ResourceContext<'_>) -> Result<NumbersDial, EngineRunError> {
    match value {
        "loose" => Ok(NumbersDial::Loose),
        "bounds" => Ok(NumbersDial::Bounds),
        _ => Err(raise(
            "schema_infer option `numbers` must be one of loose | bounds",
            resources,
        )),
    }
}

fn parse_strings_dial(value: &str, resources: &ResourceContext<'_>) -> Result<StringsDial, EngineRunError> {
    match value {
        "type" => Ok(StringsDial::Type),
        "enum" => Ok(StringsDial::Enum),
        "numeric" => Ok(StringsDial::Numeric),
        _ => Err(raise(
            "schema_infer option `strings` must be one of type | enum | numeric",
            resources,
        )),
    }
}

/// Infers one schema for an owned value, with `schema_infer/1`'s behaviour.
pub fn infer_schema(value: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    infer_schema_with_options(value, InferOptions::DEFAULT, resources)
}

/// Infers one schema for an owned value under explicit strictness options.
pub fn infer_schema_with_options(
    value: &Value,
    options: InferOptions,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let profile = infer_profile(value, options, resources)?;
    profile.into_schema_inner(true)
}

/// Builds the profile for one value.
#[allow(
    clippy::too_many_lines,
    reason = "one match per value-model kind plus the tag peel and the option-gated facts; \
              splitting it would scatter the kind inventory across helpers"
)]
fn infer_profile(
    value: &Value,
    options: InferOptions,
    resources: &ResourceContext<'_>,
) -> Result<Profile, EngineRunError> {
    // A tag LAYER is peeled before the kind match: the schema records the OUTERMOST non-core tag text and infers the
    // payload beneath it. The record is exact-text; merging clears it when values disagree.
    if let Value::Tagged { tag, payload } = value {
        let mut profile = infer_profile(payload, options, resources)?;
        profile.tag = Some(tag.as_str().to_owned());
        profile.extended = true;
        return Ok(profile);
    }
    match value.untagged() {
        Value::Null => Ok(Profile {
            null: true,
            ..Profile::default()
        }),
        Value::Bool(_) => Ok(Profile {
            boolean: true,
            ..Profile::default()
        }),
        Value::Number(number) => {
            // The integer/number fold is unchanged: the JSON codec produces EXACT decimals (`1.5` is
            // `NumberCategory::Decimal`), so emitting a `decimal` type would break `schema_infer/1`'s byte-identity law
            // for plain JSON. `decimal` is therefore a VALIDATION-ONLY type name (a schema can assert it, inference
            // never emits it); inference folds exactly as before.
            let (integer, number_flag) = if is_integer_number(number) {
                (true, false)
            } else {
                (false, true)
            };
            let number_bounds = if matches!(options.numbers, NumbersDial::Bounds) {
                let value = number.clone();
                Some((value.clone(), value))
            } else {
                None
            };
            Ok(Profile {
                integer,
                number: number_flag,
                number_bounds,
                ..Profile::default()
            })
        }
        Value::String(text) => infer_string_profile(text.as_str(), options),
        Value::Bytes(_) => Ok(Profile {
            bytes: true,
            ..Profile::default()
        }),
        Value::LocalDate(_) => Ok(Profile {
            localdate: true,
            ..Profile::default()
        }),
        Value::LocalTime(_) => Ok(Profile {
            localtime: true,
            ..Profile::default()
        }),
        Value::LocalDateTime(_) => Ok(Profile {
            localdatetime: true,
            ..Profile::default()
        }),
        Value::OffsetDateTime(_) => Ok(Profile {
            offsetdatetime: true,
            ..Profile::default()
        }),
        Value::Array(array) => {
            let mut items: Option<Box<Profile>> = None;
            let mut extended = false;
            for item in array {
                let profile = infer_profile(item, options, resources)?;
                extended |= profile.extended;
                match &mut items {
                    Some(target) => merge_profile(target, profile, resources)?,
                    None => items = Some(Box::new(profile)),
                }
            }
            let array_bounds = if matches!(options.arrays, ArrayDial::Length) {
                Some((array.len(), array.len()))
            } else {
                None
            };
            Ok(Profile {
                array: true,
                array_items: items,
                array_bounds,
                extended,
                ..Profile::default()
            })
        }
        Value::Object(object) => {
            let mut properties = BTreeMap::new();
            let mut required = Vec::new();
            let mut extended = false;
            for entry in object {
                let key = entry.key().to_owned();
                let profile = infer_profile(entry.value(), options, resources)?;
                extended |= profile.extended;
                if let Some(target) = properties.get_mut(&key) {
                    merge_profile(target, profile, resources)?;
                } else {
                    if matches!(options.required, RequiredDial::Observed) {
                        required.push(key.clone());
                    }
                    properties.insert(key, profile);
                }
            }
            Ok(Profile {
                object: Some(Box::new(ObjectProfile { properties, required })),
                extended,
                ..Profile::default()
            })
        }
        Value::Tagged { .. } => Err(EngineRunError::internal_contract(
            "schema_infer reached a tagged value without unwrapping it",
        )),
    }
}

/// Types one string under the `strings` dial.
///
/// Default `type` (and `enum`) keep the string as `string`. `numeric` runs the same scan `tonumber` uses: an integer
/// spelling emits `integer`, a non-integer number spelling emits `number`, and a refuse stays `string`. Decode is
/// unchanged — a CSV cell remains a string; only this dial may treat its text as a number.
fn infer_string_profile(text: &str, options: InferOptions) -> Result<Profile, EngineRunError> {
    if !matches!(options.strings, StringsDial::Numeric) {
        let string_enum = if matches!(options.strings, StringsDial::Enum) {
            Some(BTreeSet::from([text.to_owned()]))
        } else {
            None
        };
        return Ok(Profile {
            string: true,
            string_enum,
            ..Profile::default()
        });
    }
    let number = match scan::number(text) {
        scan::NumberParse::Value(Value::Number(number)) => number,
        scan::NumberParse::Allocation => {
            return Err(EngineRunError::allocation_failure());
        }
        scan::NumberParse::Value(_) | scan::NumberParse::Refused => {
            return Ok(Profile {
                string: true,
                ..Profile::default()
            });
        }
    };
    let (integer, number_flag) = if is_integer_number(&number) {
        (true, false)
    } else {
        (false, true)
    };
    let number_bounds = if matches!(options.numbers, NumbersDial::Bounds) {
        let value = number.clone();
        Some((value.clone(), value))
    } else {
        None
    };
    Ok(Profile {
        integer,
        number: number_flag,
        number_bounds,
        from_numeric_string: true,
        ..Profile::default()
    })
}

/// Merges one source profile into a target, recursively.
///
/// The type flags OR together and an integer folded into a non-integer number becomes `number`. An array merges its
/// item profiles; an object intersects its required keys and unions its properties with a recursive merge.
fn merge_profile(target: &mut Profile, source: Profile, resources: &ResourceContext<'_>) -> Result<(), EngineRunError> {
    target.null |= source.null;
    target.boolean |= source.boolean;
    target.integer |= source.integer;
    target.number |= source.number;
    target.string |= source.string;
    target.array |= source.array;
    target.bytes |= source.bytes;
    target.localdate |= source.localdate;
    target.localtime |= source.localtime;
    target.localdatetime |= source.localdatetime;
    target.offsetdatetime |= source.offsetdatetime;
    target.extended |= source.extended;
    target.from_numeric_string |= source.from_numeric_string;
    // The tag records the OUTERMOST wrapper of every merged value; a disagreement (one tagged, one not, or two
    // different tags) clears it, so inference only asserts a tag every merged value shares.
    match (&target.tag, &source.tag) {
        (Some(target_tag), Some(source_tag)) if target_tag == source_tag => {}
        _ => target.tag = None,
    }
    if let Some(source_items) = source.array_items {
        match &mut target.array_items {
            Some(target_items) => merge_profile(target_items, *source_items, resources)?,
            None => target.array_items = Some(source_items),
        }
    }
    if let Some(source_object) = source.object {
        match &mut target.object {
            Some(target_object) => merge_object(target_object, *source_object, resources)?,
            None => target.object = Some(source_object),
        }
    }
    // The option facts merge by widening: array bounds widen to the observed span, number bounds widen the same way,
    // and a string enum is the union of the observed values. A `None` side is absent by construction (the dial was off
    // or nothing was observed), so it leaves the other side untouched.
    if let Some((min, max)) = source.array_bounds {
        match &mut target.array_bounds {
            Some((target_min, target_max)) => {
                *target_min = (*target_min).min(min);
                *target_max = (*target_max).max(max);
            }
            None => target.array_bounds = Some((min, max)),
        }
    }
    if let Some((min, max)) = source.number_bounds {
        match &mut target.number_bounds {
            Some((target_min, target_max)) => {
                if number_compare(&min, target_min, resources)? == core::cmp::Ordering::Less {
                    *target_min = min;
                }
                if number_compare(&max, target_max, resources)? == core::cmp::Ordering::Greater {
                    *target_max = max;
                }
            }
            None => target.number_bounds = Some((min, max)),
        }
    }
    if let Some(source_enum) = source.string_enum {
        match &mut target.string_enum {
            Some(target_enum) => {
                for value in source_enum {
                    target_enum.insert(value);
                }
            }
            None => target.string_enum = Some(source_enum),
        }
    }
    if target.number {
        target.integer = false;
    }
    // A numeric-string column that also saw a refused spelling is a mixed column: stay `string`. The coerce only holds
    // when every merged value of the property parsed.
    if target.string && target.from_numeric_string && (target.integer || target.number) {
        target.integer = false;
        target.number = false;
        target.number_bounds = None;
        target.from_numeric_string = false;
    }
    Ok(())
}

/// Merges one source object-profile into a target: required becomes the INTERSECTION, properties the UNION with a
/// recursive merge.
fn merge_object(
    target: &mut ObjectProfile,
    source: ObjectProfile,
    resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    target.required.retain(|key| source.properties.contains_key(key));
    for (key, profile) in source.properties {
        match target.properties.get_mut(&key) {
            Some(target_profile) => merge_profile(target_profile, profile, resources)?,
            None => {
                target.properties.insert(key, profile);
            }
        }
    }
    Ok(())
}

/// Renders a merged profile to a JSON Schema 2020-12 object.
impl Profile {
    #[allow(
        clippy::too_many_lines,
        reason = "one linear emission: the fixed keyword order (type or enum, numeric bounds, \
                  required, properties, array bounds, items) is the document's determinism law — \
                  splitting it would thread the same builder through helpers"
    )]
    fn into_schema_inner(self, root: bool) -> Result<Value, EngineRunError> {
        let mut names: Vec<&str> = Vec::new();
        if self.null {
            names.push("null");
        }
        if self.boolean {
            names.push("boolean");
        }
        if self.number {
            names.push("number");
        } else if self.integer {
            names.push("integer");
        }
        if self.string || self.bytes || self.localdate || self.localtime || self.localdatetime || self.offsetdatetime {
            // The value-model kinds are the STRING projection:
            // a bytes or temporal value infers `"string"`, with the exact kind carried by the `jqf:kind` annotation —
            // the emitted schema stays inside standard 2020-12 vocabulary, so an external validator sees `type:
            // "string"` and ignores the annotation.
            names.push("string");
        }
        if self.array {
            names.push("array");
        }
        if self.object.is_some() {
            names.push("object");
        }
        // `strings=enum`: a profile whose ONLY kind is string renders as the observed values' `enum` instead of `type`.
        // A mixed profile keeps the type union — an enum would silently restrict the non-string kinds out of
        // existence.
        let pure_string = self.string
            && !self.null
            && !self.boolean
            && !self.number
            && !self.integer
            && !self.bytes
            && !self.localdate
            && !self.localtime
            && !self.localdatetime
            && !self.offsetdatetime
            && !self.array
            && self.object.is_none();
        let mut builder = ObjectBuilder::try_with_capacity(1).map_err(|_| EngineRunError::allocation_failure())?;
        // The vocabulary header: ONLY the root schema of an EXTENDED profile carries it — a value-model kind or tag
        // was observed somewhere in the value. Plain JSON input never sets `extended`, so a schema over JSON is
        // byte-identical to the earlier output, header included.
        if root && self.extended {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("$schema").map_err(|_| EngineRunError::allocation_failure())?,
                    Value::try_string("https://json-schema.org/draft/2020-12/schema")
                        .map_err(|_| EngineRunError::allocation_failure())?,
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
            let mut vocab_builder =
                ObjectBuilder::try_with_capacity(6).map_err(|_| EngineRunError::allocation_failure())?;
            for uri in [
                CORE_VOCABULARY,
                APPLICATOR_VOCABULARY,
                VALIDATION_VOCABULARY,
                META_DATA_VOCABULARY,
                FORMAT_ANNOTATION_VOCABULARY,
                JQF_VALUE_MODEL_VOCABULARY,
            ] {
                vocab_builder
                    .try_insert_last(
                        ObjectKey::try_from_str(uri).map_err(|_| EngineRunError::allocation_failure())?,
                        Value::Bool(true),
                    )
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("$vocabulary").map_err(|_| EngineRunError::allocation_failure())?,
                    Value::Object(
                        vocab_builder
                            .try_finish()
                            .map_err(|_| EngineRunError::allocation_failure())?,
                    ),
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        if pure_string && let Some(string_enum) = self.string_enum.clone() {
            let mut items = Vec::new();
            items
                .try_reserve_exact(string_enum.len())
                .map_err(|_| EngineRunError::allocation_failure())?;
            for value in string_enum {
                items.push(Value::try_string(&value).map_err(|_| EngineRunError::allocation_failure())?);
            }
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("enum").map_err(|_| EngineRunError::allocation_failure())?,
                    Value::Array(Array::try_from_vec(items).map_err(|_| EngineRunError::allocation_failure())?),
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        } else {
            let type_value = if names.len() == 1 {
                Value::try_string(names[0]).map_err(|_| EngineRunError::allocation_failure())?
            } else {
                let mut items = Vec::new();
                items
                    .try_reserve_exact(names.len())
                    .map_err(|_| EngineRunError::allocation_failure())?;
                for name in names {
                    items.push(Value::try_string(name).map_err(|_| EngineRunError::allocation_failure())?);
                }
                Value::Array(Array::try_from_vec(items).map_err(|_| EngineRunError::allocation_failure())?)
            };
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("type").map_err(|_| EngineRunError::allocation_failure())?,
                    type_value,
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        // The value-model KIND annotation: emitted when EXACTLY one kind was observed and no plain string — a merge
        // that saw a string and a kind, or two different kinds, projects to plain `"string"` (the annotation can only
        // be exact when the kind is).
        // `jqf:kind` is the assertion; `format`/`contentEncoding` stay standard 2020-12 ANNOTATIONS, so the schema
        // validates cleanly with foreign validators that ignore unknown keywords.
        let kinds_seen = [
            self.bytes,
            self.localdate,
            self.localtime,
            self.localdatetime,
            self.offsetdatetime,
        ]
        .iter()
        .filter(|flag| **flag)
        .count();
        let kind_annotation = if kinds_seen == 1 && !self.string {
            if self.bytes {
                Some(("bytes", Some("base64"), None))
            } else if self.localdate {
                Some(("local-date", None, Some("date")))
            } else if self.localtime {
                Some(("local-time", None, Some("time")))
            } else if self.localdatetime {
                Some(("local-datetime", None, None))
            } else {
                Some(("offset-datetime", None, Some("date-time")))
            }
        } else {
            None
        };
        if let Some((kind, encoding, format)) = kind_annotation {
            if let Some(encoding) = encoding {
                builder
                    .try_insert_last(
                        ObjectKey::try_from_str("contentEncoding").map_err(|_| EngineRunError::allocation_failure())?,
                        Value::try_string(encoding).map_err(|_| EngineRunError::allocation_failure())?,
                    )
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            if let Some(format) = format {
                builder
                    .try_insert_last(
                        ObjectKey::try_from_str("format").map_err(|_| EngineRunError::allocation_failure())?,
                        Value::try_string(format).map_err(|_| EngineRunError::allocation_failure())?,
                    )
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("jqf:kind").map_err(|_| EngineRunError::allocation_failure())?,
                    Value::try_string(kind).map_err(|_| EngineRunError::allocation_failure())?,
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        // The outermost tag: emitted when every merged value carried the same non-core tag. Sits beside `type`, which
        // is payload-transparent, so a tagged value's schema reads `tag` first and its payload's kind second.
        if let Some(tag) = self.tag {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("tag").map_err(|_| EngineRunError::allocation_failure())?,
                    Value::try_string(&tag).map_err(|_| EngineRunError::allocation_failure())?,
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        if let Some((minimum, maximum)) = self.number_bounds {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("minimum").map_err(|_| EngineRunError::allocation_failure())?,
                    Value::Number(minimum),
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("maximum").map_err(|_| EngineRunError::allocation_failure())?,
                    Value::Number(maximum),
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        if let Some(object) = self.object {
            let ObjectProfile {
                properties,
                mut required,
            } = *object;
            required.sort();
            if !required.is_empty() {
                let mut names = Vec::new();
                names
                    .try_reserve_exact(required.len())
                    .map_err(|_| EngineRunError::allocation_failure())?;
                for key in required {
                    names.push(Value::try_string(&key).map_err(|_| EngineRunError::allocation_failure())?);
                }
                builder
                    .try_insert_last(
                        ObjectKey::try_from_str("required").map_err(|_| EngineRunError::allocation_failure())?,
                        Value::Array(Array::try_from_vec(names).map_err(|_| EngineRunError::allocation_failure())?),
                    )
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            if !properties.is_empty() {
                let mut property_builder = ObjectBuilder::try_with_capacity(properties.len())
                    .map_err(|_| EngineRunError::allocation_failure())?;
                for (key, profile) in properties {
                    property_builder
                        .try_insert_last(
                            ObjectKey::try_from_str(&key).map_err(|_| EngineRunError::allocation_failure())?,
                            profile.into_schema_inner(false)?,
                        )
                        .map_err(|_| EngineRunError::allocation_failure())?;
                }
                let properties = property_builder
                    .try_finish()
                    .map_err(|_| EngineRunError::allocation_failure())?;
                builder
                    .try_insert_last(
                        ObjectKey::try_from_str("properties").map_err(|_| EngineRunError::allocation_failure())?,
                        Value::Object(properties),
                    )
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
        }
        if let Some((min_items, max_items)) = self.array_bounds {
            let min_key = ObjectKey::try_from_str("minItems").map_err(|_| EngineRunError::allocation_failure())?;
            let min_value = Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(
                i64::try_from(min_items).unwrap_or(i64::MAX),
            )));
            builder
                .try_insert_last(min_key, min_value)
                .map_err(|_| EngineRunError::allocation_failure())?;
            let max_key = ObjectKey::try_from_str("maxItems").map_err(|_| EngineRunError::allocation_failure())?;
            let max_value = Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(
                i64::try_from(max_items).unwrap_or(i64::MAX),
            )));
            builder
                .try_insert_last(max_key, max_value)
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        if let Some(array_items) = self.array_items {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str("items").map_err(|_| EngineRunError::allocation_failure())?,
                    array_items.into_schema_inner(false)?,
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        builder
            .try_finish()
            .map(Value::Object)
            .map_err(|_| EngineRunError::allocation_failure())
    }
}

// --------------------------------------------------------------------------- schema_validate / schema_errors: the
// shared JSON Schema 2020-12 walker ---------------------------------------------------------------------------

/// One draft error object, before it is materialized.
struct ErrorDraft {
    instance_path: String,
    schema_path: String,
    keyword: &'static str,
    message: String,
}

/// A resolved schema document: the root value plus its `$anchor` name → pointer token map, so a `#name` ref resolves
/// without rescanning the document.
struct SchemaDoc<'a> {
    root: &'a Value,
    anchors: BTreeMap<String, Vec<String>>,
}

/// Validates one value against a schema document and returns the ordered error objects (empty when valid).
pub fn validate_errors(
    value: &Value,
    schema_doc: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Vec<Value>, EngineRunError> {
    let doc = SchemaDoc::new(schema_doc, resources)?;
    let mut errors = Vec::new();
    let mut cursor = PathCursor::new();
    validate_node(value, schema_doc, &doc, &mut cursor, 0, &mut errors, resources)?;
    let mut out = Vec::new();
    out.try_reserve_exact(errors.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for error in errors {
        out.push(error.into_value()?);
    }
    Ok(out)
}

/// The drift verb: infer the VALUE's schema, NORMALIZE both schema documents (local `$ref`s resolved, `required`
/// sorted, `type` unions canonicalized), and run the diff walker over the pair with the ONE schema-aware rule that
/// `required`/`enum` arrays compare as SETS.
///
/// The CONTRACT is the walker's OLD side and the file's inferred schema the NEW side, so diff/2's record orientation
/// matches the prose: `added` = the file has structure the schema does not describe, `removed` = the schema promises
/// structure the file lacks, `changed` = both describe it differently. No subsumption reasoning: `{"type":"number"}` vs
/// `{"minimum":0}` reports `changed`, never "compatible" — a schema-algebra engine is deliberately out.
pub fn schema_diff_law(
    value: &Value,
    schema: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    // When the FIRST argument is itself a schema document, both sides are schemas and the walk compares them AS schemas
    // — evolution detection (`schema_diff(old_contract; new_contract)`), path-keyed semantic deltas, identical
    // documents diffing to an empty array. Bare data keeps the original drift framing: its schema is INFERRED first.
    // The narrowing: a value that carries a schema keyword is read as a schema, never as data.
    if is_schema_document(value) {
        let old = normalize_schema(value, resources)?;
        let new = normalize_schema(schema, resources)?;
        return diff_law_with(old, new, ArrayCompare::SetKeywords, resources);
    }
    let inferred = infer_schema(value, resources)?;
    let contract = normalize_schema(schema, resources)?;
    let file = normalize_schema(&inferred, resources)?;
    diff_law_with(contract, file, ArrayCompare::SetKeywords, resources)
}

/// Whether a document is ITSELF a schema document rather than bare data: an object carrying at least one keyword from
/// the supported profile (validation, applicator, or `$ref`/`$defs`). Metadata-only objects (`$id`, `$comment`,
/// `$schema` alone) do not qualify — they are ordinary JSON objects as data.
fn is_schema_document(value: &Value) -> bool {
    const SCHEMA_KEYWORDS: &[&str] = &[
        "type",
        "enum",
        "const",
        "multipleOf",
        "maximum",
        "exclusiveMaximum",
        "minimum",
        "exclusiveMinimum",
        "maxLength",
        "minLength",
        "pattern",
        "maxItems",
        "minItems",
        "uniqueItems",
        "maxContains",
        "minContains",
        "maxProperties",
        "minProperties",
        "required",
        "dependentRequired",
        "dependentSchemas",
        "allOf",
        "anyOf",
        "oneOf",
        "not",
        "if",
        "then",
        "else",
        "prefixItems",
        "items",
        "contains",
        "properties",
        "patternProperties",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
        "$ref",
        "$defs",
    ];
    let Value::Object(object) = value.untagged() else {
        return false;
    };
    object.into_iter().any(|entry| SCHEMA_KEYWORDS.contains(&entry.key()))
}

/// Normalizes one schema document for comparison: local `$ref`s resolved to their targets (the walker's own
/// resolution), `required` arrays sorted, `type` unions canonicalized (single-element arrays to bare strings, unions
/// sorted and deduplicated). Everything else is carried through with ordinary value semantics, so the walker sees the
/// same keywords a validator would.
fn normalize_schema(schema: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    // One SchemaDoc per DOCUMENT (the walker's own law): a local `$ref` resolves against the whole schema's root, so
    // the doc is created once at the top and carried down, never rebuilt per node.
    let doc = SchemaDoc::new(schema, resources)?;
    normalize_schema_at(schema, &doc, resources, 0)
}

fn normalize_schema_at(
    schema: &Value,
    doc: &SchemaDoc<'_>,
    resources: &ResourceContext<'_>,
    depth: usize,
) -> Result<Value, EngineRunError> {
    if depth > SCHEMA_RECURSION_LIMIT {
        return Err(raise("schema recursion too deep", resources));
    }
    match schema.untagged() {
        Value::Object(_) => {
            // A `$ref` node resolves to its NORMALIZED target: the walker treats `$ref` as an applicator, so the
            // structural comparison reads the target's keywords. A ref cycle past the limit raises exactly as the
            // validation walker's does.
            let Value::Object(object) = schema.untagged() else {
                unreachable!("the object arm sees an object");
            };
            if let Some(reference) = object.get("$ref") {
                let target = resolve_ref(reference, doc, resources)?;
                return normalize_schema_at(target, doc, resources, depth + 1);
            }
            let mut builder =
                ObjectBuilder::try_with_capacity(object.len()).map_err(|_| EngineRunError::allocation_failure())?;
            for entry in object {
                let key = entry.key();
                let value = entry.value();
                let normalized = match key {
                    // `$defs` is REFERENCE metadata: every use of it resolves through a `$ref` (also normalized), so
                    // the block itself is consumed by resolution and dropping it keeps a ref-using contract comparable
                    // to the file's inline schema without "$defs removed" noise. An entry nothing references is dead
                    // weight either way.
                    "$defs" => continue,
                    // `required` compares as a SET at diff time, so its order is noise: sort here and the walker sees
                    // the canonical spelling on both sides.
                    "required" => normalize_required(value, resources)?,
                    // `type` unions canonicalize: `["number"]` == "number", and union order is noise.
                    "type" => canonicalize_type(value, resources)?,
                    _ => normalize_schema_at(value, doc, resources, depth + 1)?,
                };
                builder
                    .try_insert_last(
                        ObjectKey::try_from_str(key).map_err(|_| EngineRunError::allocation_failure())?,
                        normalized,
                    )
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            builder
                .try_finish()
                .map(Value::Object)
                .map_err(|_| EngineRunError::allocation_failure())
        }
        Value::Array(array) => {
            let mut items = Vec::new();
            items
                .try_reserve_exact(array.len())
                .map_err(|_| EngineRunError::allocation_failure())?;
            for item in array {
                items.push(normalize_schema_at(item, doc, resources, depth + 1)?);
            }
            Array::try_from_vec(items)
                .map(Value::Array)
                .map_err(|_| EngineRunError::allocation_failure())
        }
        _ => Ok(schema.clone()),
    }
}

/// The `required` keyword's normalized spelling: the member names sorted (the diff compares them as a SET, so authoring
/// order is not a difference).
fn normalize_required(value: &Value, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::Array(array) = value.untagged() else {
        // A malformed `required` is the validator's error; the diff compares it as-is.
        return Ok(value.clone());
    };
    let mut names: Vec<String> = Vec::new();
    names
        .try_reserve_exact(array.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for item in array {
        match item.untagged() {
            Value::String(text) => names.push(text.as_str().to_owned()),
            // Same as above: a non-string member stays as-authored.
            _ => {
                return Ok(value.clone());
            }
        }
    }
    names.sort();
    names.dedup();
    let mut items = Vec::new();
    items
        .try_reserve_exact(names.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for name in names {
        items.push(Value::try_string(&name).map_err(|_| EngineRunError::allocation_failure())?);
    }
    Array::try_from_vec(items)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// The `type` keyword's canonical spelling: a single-member union collapses to the bare string, and a multi-member
/// union sorts (and deduplicates) its names — order noise is not a difference, and `"number"` equals `["number"]`.
fn canonicalize_type(value: &Value, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Value::Array(array) = value.untagged() else {
        return Ok(value.clone());
    };
    let mut names: Vec<String> = Vec::new();
    names
        .try_reserve_exact(array.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for item in array {
        match item.untagged() {
            Value::String(text) => names.push(text.as_str().to_owned()),
            _ => {
                // A malformed union stays as-authored; the validator owns its rejection.
                return Ok(value.clone());
            }
        }
    }
    names.sort();
    names.dedup();
    if names.len() == 1 {
        return Value::try_string(&names[0]).map_err(|_| EngineRunError::allocation_failure());
    }
    let mut items = Vec::new();
    items
        .try_reserve_exact(names.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for name in names {
        items.push(Value::try_string(&name).map_err(|_| EngineRunError::allocation_failure())?);
    }
    Array::try_from_vec(items)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// Validates one value against one schema node, appending ordered errors.
///
/// The keyword order at a node is fixed and documented: `$ref` (recurse into the target first), then `type`, `enum`,
/// `const`, `multipleOf`, numeric bounds, string checks, array checks, `contains`, object checks, then the applicator
/// composition (`allOf`/`anyOf`/`oneOf`/`not`, `if`/`then`/`else`).
/// Properties and items recurse in the order their instance members appear.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the walker IS the keyword dispatch table; splitting it would hide which keywords \
              are covered and which ordering law binds the error list"
)]
fn validate_node(
    value: &Value,
    schema: &Value,
    doc: &SchemaDoc<'_>,
    cursor: &mut PathCursor,
    depth: usize,
    errors: &mut Vec<ErrorDraft>,
    resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    if depth > SCHEMA_RECURSION_LIMIT {
        return Err(raise("schema recursion too deep", resources));
    }
    // A boolean schema is 2020-12's `true`/`false` form.
    let Value::Object(object) = schema.untagged() else {
        if matches!(schema.untagged(), Value::Bool(false)) {
            errors.push(ErrorDraft::new(cursor, "false", "schema is false".to_owned()));
        }
        return Ok(());
    };
    reject_unsupported_keywords(object, resources)?;

    if let Some(reference) = object.get("$ref") {
        let target = resolve_ref(reference, doc, resources)?;
        let ref_tokens = ref_schema_path(reference, doc, resources)?;
        let saved_schema = cursor.save_schema();
        cursor.set_schema(ref_tokens);
        validate_node(value, target, doc, cursor, depth + 1, errors, resources)?;
        cursor.restore_schema(saved_schema);
    }

    // --- type -------------------------------------------------------------
    if let Some(type_schema) = object.get("type") {
        let allowed = schema_type_names(type_schema, resources)?;
        let mut matches = false;
        for name in &allowed {
            if value_matches_type(value, name, resources)? {
                matches = true;
                break;
            }
        }
        if !matches {
            errors.push(ErrorDraft::new(
                cursor,
                "type",
                format!("expected type {}", allowed.join(" or ")),
            ));
        }
    }

    // --- jqf:kind ---------------------------------------------------------- The ASSERTION keyword that carries the
    // exact value-model kind: a `jqf:kind` schema node rejects any value whose kind is not the named one — a plain
    // string that merely looks like a date, or an OffsetDateTime under `"local-date"`. `type:"string"` accepts the
    // portable projection, so `jqf:kind` is what makes inference round-trip:
    // the inferred schema's annotation is an assertion here, while `format` stays annotation-only per 2020-12 (the
    // standard keyword's semantics never fork).
    if let Some(kind_schema) = object.get("jqf:kind") {
        let Value::String(kind) = kind_schema.untagged() else {
            return Err(raise("schema jqf:kind must be a string", resources));
        };
        let matches = match kind.as_str() {
            "bytes" => matches!(value.untagged(), Value::Bytes(_)),
            "local-date" => matches!(value.untagged(), Value::LocalDate(_)),
            "local-time" => matches!(value.untagged(), Value::LocalTime(_)),
            "local-datetime" => matches!(value.untagged(), Value::LocalDateTime(_)),
            "offset-datetime" => matches!(value.untagged(), Value::OffsetDateTime(_)),
            _ => {
                return Err(raise(
                    &format!("schema jqf:kind `{}` is not supported", kind.as_str()),
                    resources,
                ));
            }
        };
        if !matches {
            errors.push(ErrorDraft::new(
                cursor,
                "jqf:kind",
                format!("expected kind {:?}", kind.as_str()),
            ));
        }
    }

    // --- tag / payload (the value-model vocabulary) ------------------------ `tag` asserts the OUTERMOST non-core
    // tag's exact text; `payload` validates the payload beneath that tag. Both are payload-exact (they read the
    // wrapper, unlike `type`, which is payload-transparent).
    if let Some(tag_schema) = object.get("tag") {
        let Value::String(expected) = tag_schema.untagged() else {
            return Err(raise("schema tag must be a string", resources));
        };
        let matches = matches!(value, Value::Tagged { tag, .. } if tag.as_str() == expected.as_str());
        if !matches {
            errors.push(ErrorDraft::new(cursor, "tag", format!("expected tag {expected:?}")));
        }
    }
    if let Some(payload_schema) = object.get("payload")
        && let Value::Tagged { payload, .. } = value
    {
        let (saved_instance, saved_schema) = cursor.save_both();
        cursor.push_schema("payload");
        validate_node(payload, payload_schema, doc, cursor, depth + 1, errors, resources)?;
        cursor.restore_both(saved_instance, saved_schema);
    }

    // --- enum / const -----------------------------------------------------
    if let Some(enum_schema) = object.get("enum") {
        let allowed = schema_array("enum", enum_schema, resources)?;
        let mut matched = false;
        for candidate in allowed {
            if semantic_eq(value, candidate).map_err(|_| too_deep(resources))? {
                matched = true;
                break;
            }
        }
        if !matched {
            errors.push(ErrorDraft::new(cursor, "enum", "value is not in enum".to_owned()));
        }
    }
    if let Some(const_schema) = object.get("const")
        && !semantic_eq(value, const_schema).map_err(|_| too_deep(resources))?
    {
        errors.push(ErrorDraft::new(
            cursor,
            "const",
            "value does not match const".to_owned(),
        ));
    }

    // --- multipleOf -------------------------------------------------------
    if let Some(divisor) = object.get("multipleOf") {
        let Value::Number(divisor_number) = divisor.untagged() else {
            return Err(raise("schema multipleOf must be a number", resources));
        };
        if !is_positive(divisor_number) {
            return Err(raise("schema multipleOf must be a number greater than 0", resources));
        }
        if let Value::Number(value_number) = value.untagged()
            && !is_multiple(value_number, divisor_number)
        {
            errors.push(ErrorDraft::new(
                cursor,
                "multipleOf",
                format!("must be a multiple of {}", render_number(divisor_number)),
            ));
        }
    }

    // --- numeric bounds ---------------------------------------------------
    validate_number_bounds(value, object, cursor, errors, resources)?;

    // --- string checks ----------------------------------------------------
    if let Value::String(text) = value.untagged() {
        if let Some(limit) = object.get("maxLength") {
            let limit = non_negative_integer("maxLength", limit, resources)?;
            let len = text.chars().count();
            if len > limit {
                errors.push(ErrorDraft::new(
                    cursor,
                    "maxLength",
                    format!("length must be <= {limit}"),
                ));
            }
        }
        if let Some(limit) = object.get("minLength") {
            let limit = non_negative_integer("minLength", limit, resources)?;
            let len = text.chars().count();
            if len < limit {
                errors.push(ErrorDraft::new(
                    cursor,
                    "minLength",
                    format!("length must be >= {limit}"),
                ));
            }
        }
        if let Some(pattern) = object.get("pattern") {
            let Value::String(pattern) = pattern.untagged() else {
                return Err(raise("schema pattern must be a string", resources));
            };
            let regex = compile_plain_regex(pattern.as_str())
                .map_err(|_| raise("schema pattern is not a valid regular expression", resources))?;
            if !regex.is_match(text.as_str()) {
                errors.push(ErrorDraft::new(cursor, "pattern", "does not match pattern".to_owned()));
            }
        }
    }

    // --- array checks -----------------------------------------------------
    if let Value::Array(array) = value.untagged() {
        if let Some(limit) = object.get("maxItems") {
            let limit = non_negative_integer("maxItems", limit, resources)?;
            if array.len() > limit {
                errors.push(ErrorDraft::new(
                    cursor,
                    "maxItems",
                    format!("item count must be <= {limit}"),
                ));
            }
        }
        if let Some(limit) = object.get("minItems") {
            let limit = non_negative_integer("minItems", limit, resources)?;
            if array.len() < limit {
                errors.push(ErrorDraft::new(
                    cursor,
                    "minItems",
                    format!("item count must be >= {limit}"),
                ));
            }
        }
        if let Some(unique) = object.get("uniqueItems") {
            let Value::Bool(unique) = unique.untagged() else {
                return Err(raise("schema uniqueItems must be a boolean", resources));
            };
            if *unique && !is_unique(array, resources)? {
                errors.push(ErrorDraft::new(
                    cursor,
                    "uniqueItems",
                    "array items must be unique".to_owned(),
                ));
            }
        }

        // prefixItems validates the leading items as a tuple; items validates the remainder (or every item when
        // prefixItems is absent).
        let prefix = object.get("prefixItems");
        let items_schema = object.get("items");
        let prefix_len = match prefix {
            Some(Value::Array(prefix)) => prefix.len(),
            Some(_) => return Err(raise("schema prefixItems must be an array", resources)),
            None => 0,
        };
        for (index, item) in array.iter().enumerate() {
            let target = if index < prefix_len {
                match prefix {
                    Some(Value::Array(prefix)) => prefix.get(index),
                    _ => None,
                }
            } else {
                items_schema
            };
            if let Some(target_schema) = target {
                let (saved_instance, saved_schema) = cursor.save_both();
                cursor.push_instance(index.to_string());
                if index < prefix_len {
                    cursor.push_schema("prefixItems");
                    cursor.push_schema_index(index);
                } else {
                    cursor.push_schema("items");
                }
                validate_node(item, target_schema, doc, cursor, depth + 1, errors, resources)?;
                cursor.restore_both(saved_instance, saved_schema);
            }
        }

        // contains: at least minContains (default 1) items must match, and at most maxContains may.
        if let Some(contains) = object.get("contains") {
            let mut matches = 0_usize;
            for item in array {
                let mut probe = Vec::new();
                let mut probe_cursor = PathCursor::new();
                validate_node(item, contains, doc, &mut probe_cursor, depth + 1, &mut probe, resources)?;
                if probe.is_empty() {
                    matches += 1;
                }
            }
            let min = object
                .get("minContains")
                .map(|v| non_negative_integer("minContains", v, resources))
                .transpose()?
                .unwrap_or(1);
            let max = object
                .get("maxContains")
                .map(|v| non_negative_integer("maxContains", v, resources))
                .transpose()?;
            if matches < min {
                errors.push(ErrorDraft::new(
                    cursor,
                    "contains",
                    format!("must contain at least {min} matching item(s)"),
                ));
            } else if let Some(max) = max
                && matches > max
            {
                errors.push(ErrorDraft::new(
                    cursor,
                    "maxContains",
                    format!("must contain at most {max} matching item(s)"),
                ));
            }
        }
    }

    // --- object checks ----------------------------------------------------
    if let Value::Object(instance) = value.untagged() {
        if let Some(limit) = object.get("maxProperties") {
            let limit = non_negative_integer("maxProperties", limit, resources)?;
            if instance.len() > limit {
                errors.push(ErrorDraft::new(
                    cursor,
                    "maxProperties",
                    format!("property count must be <= {limit}"),
                ));
            }
        }
        if let Some(limit) = object.get("minProperties") {
            let limit = non_negative_integer("minProperties", limit, resources)?;
            if instance.len() < limit {
                errors.push(ErrorDraft::new(
                    cursor,
                    "minProperties",
                    format!("property count must be >= {limit}"),
                ));
            }
        }
        if let Some(required) = object.get("required") {
            let required = string_array("required", required, resources)?;
            for key in required {
                if instance.get(&key).is_none() {
                    errors.push(ErrorDraft::new(
                        cursor,
                        "required",
                        format!("missing required property {key:?}"),
                    ));
                }
            }
        }
        if let Some(dependent) = object.get("dependentRequired") {
            validate_dependent_required(instance, dependent, cursor, errors, resources)?;
        }

        // properties / patternProperties / additionalProperties.
        let properties = object.get("properties");
        if let Some(properties) = properties {
            let Value::Object(properties) = properties.untagged() else {
                return Err(raise("schema properties must be an object", resources));
            };
            for entry in properties {
                let key = entry.key().to_owned();
                if let Some(property_value) = instance.get(&key) {
                    let (saved_instance, saved_schema) = cursor.save_both();
                    cursor.push_instance(&key);
                    cursor.push_schema("properties");
                    cursor.push_schema(&key);
                    validate_node(property_value, entry.value(), doc, cursor, depth + 1, errors, resources)?;
                    cursor.restore_both(saved_instance, saved_schema);
                }
            }
        }

        // patternProperties: any key matching a pattern is validated against that pattern's subschema.
        let mut covered_by_pattern = Vec::new();
        if let Some(pattern_properties) = object.get("patternProperties") {
            let Value::Object(pattern_properties) = pattern_properties.untagged() else {
                return Err(raise("schema patternProperties must be an object", resources));
            };
            for entry in pattern_properties {
                let pattern = entry.key();
                let regex = compile_plain_regex(pattern).map_err(|_| {
                    raise(
                        "schema patternProperties key is not a valid regular expression",
                        resources,
                    )
                })?;
                for key in instance_keys(instance)? {
                    if regex.is_match(&key) {
                        covered_by_pattern.push(key.clone());
                        if let Some(property_value) = instance.get(&key) {
                            let (saved_instance, saved_schema) = cursor.save_both();
                            cursor.push_instance(&key);
                            cursor.push_schema("patternProperties");
                            cursor.push_schema(pattern);
                            validate_node(property_value, entry.value(), doc, cursor, depth + 1, errors, resources)?;
                            cursor.restore_both(saved_instance, saved_schema);
                        }
                    }
                }
            }
        }

        if let Some(additional) = object.get("additionalProperties") {
            validate_additional(
                instance,
                properties,
                &covered_by_pattern,
                additional,
                doc,
                cursor,
                depth,
                errors,
                resources,
            )?;
        }

        if let Some(names_schema) = object.get("propertyNames") {
            for key in instance_keys(instance)? {
                let key_value = Value::try_string(&key).map_err(|_| EngineRunError::allocation_failure())?;
                let (saved_instance, saved_schema) = cursor.save_both();
                cursor.push_instance(&key);
                cursor.push_schema("propertyNames");
                validate_node(&key_value, names_schema, doc, cursor, depth + 1, errors, resources)?;
                cursor.restore_both(saved_instance, saved_schema);
            }
        }
        if let Some(dependent) = object.get("dependentSchemas") {
            let Value::Object(dependent) = dependent.untagged() else {
                return Err(raise("schema dependentSchemas must be an object", resources));
            };
            for entry in dependent {
                if instance.get(entry.key()).is_some() {
                    let (saved_instance, saved_schema) = cursor.save_both();
                    cursor.push_schema("dependentSchemas");
                    cursor.push_schema(entry.key());
                    validate_node(value, entry.value(), doc, cursor, depth + 1, errors, resources)?;
                    cursor.restore_both(saved_instance, saved_schema);
                }
            }
        }
    }

    // --- applicator composition -------------------------------------------
    if let Some(all_of) = object.get("allOf") {
        let schemas = schema_array("allOf", all_of, resources)?;
        for schema in schemas {
            validate_node(value, schema, doc, cursor, depth + 1, errors, resources)?;
        }
    }
    if let Some(any_of) = object.get("anyOf") {
        let schemas = schema_array("anyOf", any_of, resources)?;
        if !any_valid(value, &schemas, doc, depth, resources)? {
            errors.push(ErrorDraft::new(
                cursor,
                "anyOf",
                "value does not match any schema".to_owned(),
            ));
        }
    }
    if let Some(one_of) = object.get("oneOf") {
        let schemas = schema_array("oneOf", one_of, resources)?;
        let count = valid_count(value, &schemas, doc, depth, resources)?;
        if count != 1 {
            errors.push(ErrorDraft::new(
                cursor,
                "oneOf",
                if count > 1 {
                    "value matches more than one schema".to_owned()
                } else {
                    "value does not match exactly one schema".to_owned()
                },
            ));
        }
    }
    if let Some(not) = object.get("not") {
        let mut probe = Vec::new();
        let mut probe_cursor = PathCursor::new();
        validate_node(value, not, doc, &mut probe_cursor, depth + 1, &mut probe, resources)?;
        if probe.is_empty() {
            errors.push(ErrorDraft::new(
                cursor,
                "not",
                "value must not match the schema".to_owned(),
            ));
        }
    }
    if let Some(if_schema) = object.get("if") {
        let mut probe = Vec::new();
        let mut probe_cursor = PathCursor::new();
        validate_node(
            value,
            if_schema,
            doc,
            &mut probe_cursor,
            depth + 1,
            &mut probe,
            resources,
        )?;
        let branch = if probe.is_empty() {
            object.get("then")
        } else {
            object.get("else")
        };
        if let Some(branch) = branch {
            validate_node(value, branch, doc, cursor, depth + 1, errors, resources)?;
        }
    }

    Ok(())
}

/// Rejects a KNOWN-UNSUPPORTED vocabulary keyword with a catchable error.
fn reject_unsupported_keywords(object: &Object, resources: &ResourceContext<'_>) -> Result<(), EngineRunError> {
    for entry in object {
        let keyword = entry.key();
        if is_known_unsupported(keyword) {
            return Err(raise(&format!("unsupported schema keyword `{keyword}`"), resources));
        }
    }
    Ok(())
}

/// The KNOWN-UNSUPPORTED vocabulary keywords: standard JSON Schema 2020-12 keywords from vocabularies jqf does not
/// implement. Anything not in this set and not a supported keyword is an annotation (2020-12 unknown-keyword law).
///
/// `contentEncoding` is DELIBERATELY absent: the bytes kind's inferred schema carries `contentEncoding: "base64"`, so
/// the keyword is a supported ANNOTATION — accepted and ignored for assertion, exactly like `format`. The rest of the
/// content vocabulary stays unsupported.
fn is_known_unsupported(keyword: &str) -> bool {
    matches!(
        keyword,
        "unevaluatedItems"
            | "unevaluatedProperties"
            | "$dynamicRef"
            | "$dynamicAnchor"
            | "$recursiveRef"
            | "$recursiveAnchor"
            | "contentMediaType"
            | "contentSchema"
    )
}

// --------------------------------------------------------------------------- Keyword helpers
// ---------------------------------------------------------------------------

/// The allowed type names of a `type` keyword (string or array of strings), rejecting an unknown type name as a
/// schema-shape error.
fn schema_type_names(value: &Value, resources: &ResourceContext<'_>) -> Result<Vec<String>, EngineRunError> {
    let names = match value.untagged() {
        Value::String(name) => vec![name.as_str().to_owned()],
        Value::Array(array) => {
            let mut names = Vec::new();
            for item in array {
                let Value::String(name) = item.untagged() else {
                    return Err(raise("schema type must be a string or array of strings", resources));
                };
                names.push(name.as_str().to_owned());
            }
            names
        }
        _ => {
            return Err(raise("schema type must be a string or array of strings", resources));
        }
    };
    for name in &names {
        if !is_supported_type_name(name) {
            return Err(raise(&format!("schema type `{name}` is not supported"), resources));
        }
    }
    Ok(names)
}

/// The 2020-12 type names jqf implements, plus the value-model vocabulary's:
/// `integer` is the integral-number subtype of `number`; `decimal` is the exact-decimal subtype (the value model's
/// `NumberCategory::Decimal`); `bytes` and the four temporal kinds name the value-model kinds JSON cannot spell.
fn is_supported_type_name(name: &str) -> bool {
    matches!(
        name,
        "null"
            | "boolean"
            | "object"
            | "array"
            | "number"
            | "string"
            | "integer"
            | "decimal"
            | "bytes"
            | "localdate"
            | "localtime"
            | "localdatetime"
            | "offsetdatetime"
    )
}

/// Whether a value matches one JSON Schema type name.
///
/// The `string` arm is the portable-projection law: `"string"` accepts an owned string AND the value-model kinds that
/// PROJECT as strings — bytes and all four temporals — so a schema written against the portable projection keeps
/// validating a TOML datetime or CBOR byte string. The exact kind is asserted by the `jqf:kind` keyword, never by
/// `type`; the value-model vocabulary's extended type names (`bytes`, `localdate`, …) remain accepted for schemas
/// written against the earlier shape.
fn value_matches_type(value: &Value, name: &str, resources: &ResourceContext<'_>) -> Result<bool, EngineRunError> {
    let value = value.untagged();
    Ok(match name {
        "null" => matches!(value, Value::Null),
        "boolean" => matches!(value, Value::Bool(_)),
        "object" => matches!(value, Value::Object(_)),
        "array" => matches!(value, Value::Array(_)),
        "number" => matches!(value, Value::Number(_)),
        "integer" => match value {
            Value::Number(number) => is_integer_number(number),
            _ => false,
        },
        "decimal" => match value {
            Value::Number(number) => {
                matches!(number.category(), NumberCategory::Decimal)
            }
            _ => false,
        },
        "string" => matches!(value, Value::String(_) | Value::Bytes(_)) || value.is_temporal(),
        "bytes" => matches!(value, Value::Bytes(_)),
        "localdate" => matches!(value, Value::LocalDate(_)),
        "localtime" => matches!(value, Value::LocalTime(_)),
        "localdatetime" => matches!(value, Value::LocalDateTime(_)),
        "offsetdatetime" => matches!(value, Value::OffsetDateTime(_)),
        _ => {
            return Err(raise(&format!("schema type `{name}` is not supported"), resources));
        }
    })
}

/// The members of an `enum` keyword, rejecting a non-array shape.
/// Reads a required array of strings.
fn string_array(keyword: &str, value: &Value, resources: &ResourceContext<'_>) -> Result<Vec<String>, EngineRunError> {
    let Value::Array(array) = value.untagged() else {
        return Err(raise(
            &format!("schema {keyword} must be an array of strings"),
            resources,
        ));
    };
    let mut strings = Vec::new();
    for item in array {
        let Value::String(text) = item.untagged() else {
            return Err(raise(
                &format!("schema {keyword} must be an array of strings"),
                resources,
            ));
        };
        strings.push(text.as_str().to_owned());
    }
    Ok(strings)
}

/// Reads an array of schema values.
fn schema_array<'a>(
    keyword: &str,
    value: &'a Value,
    resources: &ResourceContext<'_>,
) -> Result<Vec<&'a Value>, EngineRunError> {
    let Value::Array(array) = value.untagged() else {
        return Err(raise(&format!("schema {keyword} must be an array"), resources));
    };
    let mut out = Vec::new();
    for item in array {
        out.push(item);
    }
    Ok(out)
}

/// Reads a non-negative integer keyword value.
fn non_negative_integer(
    keyword: &str,
    value: &Value,
    resources: &ResourceContext<'_>,
) -> Result<usize, EngineRunError> {
    let Value::Number(number) = value.untagged() else {
        return Err(raise(
            &format!("schema {keyword} must be a non-negative integer"),
            resources,
        ));
    };
    if !is_integer_number(number) || is_negative(number) {
        return Err(raise(
            &format!("schema {keyword} must be a non-negative integer"),
            resources,
        ));
    }
    let text = render_number(number);
    text.parse::<usize>()
        .map_err(|_| raise(&format!("schema {keyword} is too large"), resources))
}

/// The numeric-bounds keywords, applied in a fixed order.
fn validate_number_bounds(
    value: &Value,
    object: &Object,
    cursor: &PathCursor,
    errors: &mut Vec<ErrorDraft>,
    resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    let Value::Number(value_number) = value.untagged() else {
        // Bounds only apply to numbers; a non-number instance is untouched.
        return Ok(());
    };
    // A NaN instance needs no special arm: under the engine's ONE NaN law it ranks BELOW every number, so the
    // comparisons below answer that it violates both lower-bound keywords (it is below those bounds) and satisfies both
    // upper-bound keywords (it never ranks above a bound) — the same answers the engine's own `>=`/`<=` operators
    // give.
    // Deriving the four keywords from anything else would be a second NaN fact.
    for (keyword, op) in [
        ("minimum", BoundOp::AtLeast),
        ("exclusiveMinimum", BoundOp::Greater),
        ("maximum", BoundOp::AtMost),
        ("exclusiveMaximum", BoundOp::Less),
    ] {
        let Some(bound) = object.get(keyword) else {
            continue;
        };
        let Value::Number(bound_number) = bound.untagged() else {
            return Err(raise(&format!("schema {keyword} must be a finite number"), resources));
        };
        if !is_finite(bound_number) {
            return Err(raise(&format!("schema {keyword} must be a finite number"), resources));
        }
        let ordering = number_compare(value_number, bound_number, resources)?;
        let violated = match op {
            BoundOp::AtLeast => ordering == core::cmp::Ordering::Less,
            BoundOp::Greater => ordering != core::cmp::Ordering::Greater,
            BoundOp::AtMost => ordering == core::cmp::Ordering::Greater,
            BoundOp::Less => ordering != core::cmp::Ordering::Less,
        };
        if violated {
            errors.push(ErrorDraft::new(
                cursor,
                keyword,
                format!("must be {} {}", op.message_symbol(), render_number(bound_number)),
            ));
        }
    }
    Ok(())
}

/// One numeric-bound operator.
#[derive(Clone, Copy)]
enum BoundOp {
    AtLeast,
    Greater,
    AtMost,
    Less,
}

impl BoundOp {
    fn message_symbol(self) -> &'static str {
        match self {
            Self::AtLeast => ">=",
            Self::Greater => ">",
            Self::AtMost => "<=",
            Self::Less => "<",
        }
    }
}

/// Validates `dependentRequired`: for each present property, its listed dependents must also be present.
fn validate_dependent_required(
    instance: &Object,
    dependent: &Value,
    cursor: &PathCursor,
    errors: &mut Vec<ErrorDraft>,
    resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    let Value::Object(dependent) = dependent.untagged() else {
        return Err(raise("schema dependentRequired must be an object", resources));
    };
    for entry in dependent {
        let prop = entry.key().to_owned();
        if instance.get(&prop).is_none() {
            continue;
        }
        let required = string_array("dependentRequired", entry.value(), resources)?;
        for key in required {
            if instance.get(&key).is_none() {
                errors.push(ErrorDraft::new(
                    cursor,
                    "dependentRequired",
                    format!("missing required property {key:?} (required by {prop:?})"),
                ));
            }
        }
    }
    Ok(())
}

/// Validates `additionalProperties` over the instance keys not covered by `properties` or `patternProperties`.
#[allow(
    clippy::too_many_arguments,
    reason = "the additional-properties walk hands the walker, its coverage sets, and the \
              current schema's own keyword object to one law"
)]
fn validate_additional(
    instance: &Object,
    properties: Option<&Value>,
    covered_by_pattern: &[String],
    additional: &Value,
    doc: &SchemaDoc<'_>,
    cursor: &mut PathCursor,
    depth: usize,
    errors: &mut Vec<ErrorDraft>,
    resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    let mut covered = Vec::new();
    if let Some(properties) = properties {
        let Value::Object(properties) = properties.untagged() else {
            return Err(raise("schema properties must be an object", resources));
        };
        covered.extend(instance_keys(properties)?);
    }
    covered.extend(covered_by_pattern.iter().cloned());
    let Value::Bool(allow) = additional.untagged() else {
        // A schema: validate each uncovered key's value against it.
        for key in instance_keys(instance)? {
            if covered.iter().any(|k| k == &key) {
                continue;
            }
            let Some(property_value) = instance.get(&key) else {
                continue;
            };
            let (saved_instance, saved_schema) = cursor.save_both();
            cursor.push_instance(&key);
            cursor.push_schema("additionalProperties");
            validate_node(property_value, additional, doc, cursor, depth + 1, errors, resources)?;
            cursor.restore_both(saved_instance, saved_schema);
        }
        return Ok(());
    };
    if !*allow {
        for key in instance_keys(instance)? {
            if covered.iter().any(|k| k == &key) {
                continue;
            }
            errors.push(ErrorDraft::new(
                cursor,
                "additionalProperties",
                format!("additional property {key:?} is not allowed"),
            ));
        }
    }
    Ok(())
}

/// Whether all items of an array are pairwise distinct.
///
/// Answered in O(n log n), not the O(n²) pairwise sweep: items are ranked by
/// the internal total order — the same law every sort runs on, which answers
/// `Equal` exactly where deep equality holds (NaN-against-NaN included) — so
/// semantic duplicates land adjacent and only adjacent pairs face the equality
/// probe.
fn is_unique(array: &Array, resources: &ResourceContext<'_>) -> Result<bool, EngineRunError> {
    if array.len() < 2 {
        return Ok(true);
    }
    let mut order: Vec<usize> = Vec::with_capacity(array.len());
    order.extend(0..array.len());
    // A comparison that refuses is latched and raised after the sort; a sort
    // comparator must answer, and an unrefused pair never decides uniqueness
    // on its own.
    let refused = core::cell::Cell::new(false);
    order.sort_unstable_by(|&left, &right| {
        compare(
            array.get(left).expect("sort indices stay in bounds"),
            array.get(right).expect("sort indices stay in bounds"),
            0,
            NanLaw::Total,
        )
        .unwrap_or_else(|_| {
            refused.set(true);
            core::cmp::Ordering::Equal
        })
    });
    if refused.get() {
        return Err(too_deep(resources));
    }
    for pair in order.windows(2) {
        if semantic_eq(
            array.get(pair[0]).expect("window indices stay in bounds"),
            array.get(pair[1]).expect("window indices stay in bounds"),
        )
        .map_err(|_| too_deep(resources))?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether at least one schema in a list validates the value.
fn any_valid(
    value: &Value,
    schemas: &[&Value],
    doc: &SchemaDoc<'_>,
    depth: usize,
    resources: &ResourceContext<'_>,
) -> Result<bool, EngineRunError> {
    for schema in schemas {
        let mut probe = Vec::new();
        let mut probe_cursor = PathCursor::new();
        validate_node(value, schema, doc, &mut probe_cursor, depth + 1, &mut probe, resources)?;
        if probe.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Counts how many schemas in a list validate the value.
fn valid_count(
    value: &Value,
    schemas: &[&Value],
    doc: &SchemaDoc<'_>,
    depth: usize,
    resources: &ResourceContext<'_>,
) -> Result<usize, EngineRunError> {
    let mut count = 0;
    for schema in schemas {
        let mut probe = Vec::new();
        let mut probe_cursor = PathCursor::new();
        validate_node(value, schema, doc, &mut probe_cursor, depth + 1, &mut probe, resources)?;
        if probe.is_empty() {
            count += 1;
        }
    }
    Ok(count)
}

// --------------------------------------------------------------------------- $ref resolution
// ---------------------------------------------------------------------------

impl<'a> SchemaDoc<'a> {
    /// Collects the document's `$anchor` plain-name locations and validates the root's `$vocabulary` against the
    /// registered set.
    fn new(root: &'a Value, resources: &ResourceContext<'_>) -> Result<Self, EngineRunError> {
        check_vocabulary(root, resources)?;
        let mut anchors = BTreeMap::new();
        collect_anchors(root, &mut Vec::new(), &mut anchors, resources)?;
        Ok(Self { root, anchors })
    }
}

/// Validates the root's `$vocabulary`: every declared URI must be in the registered set — the 2020-12 vocabularies
/// jqf implements plus [`JQF_VALUE_MODEL_VOCABULARY`]. An unknown URI is a catchable error, never silently ignored (the
/// "unsupported vocabulary is an explicit error" law, now applied to the vocabulary's VALUES rather than the keyword
/// itself). A boolean schema or a document without `$vocabulary` has nothing to check.
fn check_vocabulary(root: &Value, resources: &ResourceContext<'_>) -> Result<(), EngineRunError> {
    let Value::Object(object) = root.untagged() else {
        return Ok(());
    };
    let Some(vocabulary) = object.get("$vocabulary") else {
        return Ok(());
    };
    let Value::Object(vocabulary) = vocabulary.untagged() else {
        return Err(raise("schema $vocabulary must be an object", resources));
    };
    for entry in vocabulary {
        let uri = entry.key();
        if !is_known_vocabulary(uri) {
            return Err(raise(&format!("unsupported schema vocabulary `{uri}`"), resources));
        }
    }
    Ok(())
}

/// Recursively collects `$anchor: "name"` → pointer tokens.
fn collect_anchors(
    value: &Value,
    tokens: &mut Vec<String>,
    anchors: &mut BTreeMap<String, Vec<String>>,
    resources: &ResourceContext<'_>,
) -> Result<(), EngineRunError> {
    match value.untagged() {
        Value::Object(object) => {
            if let Some(anchor) = object.get("$anchor") {
                let Value::String(name) = anchor.untagged() else {
                    return Err(raise("schema $anchor must be a string", resources));
                };
                anchors.insert(name.as_str().to_owned(), tokens.clone());
            }
            for entry in object {
                tokens.push(entry.key().to_owned());
                collect_anchors(entry.value(), tokens, anchors, resources)?;
                tokens.pop();
            }
        }
        Value::Array(array) => {
            for (index, item) in array.iter().enumerate() {
                tokens.push(index.to_string());
                collect_anchors(item, tokens, anchors, resources)?;
                tokens.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

/// Resolves one `$ref` value to the schema it names.
/// One parsed `$ref` string, before resolution: the whole schema (`#`), a pointer below it, or a named `$anchor`.
enum RefTarget<'a> {
    Root,
    Pointer(&'a str),
    Anchor(&'a str),
}

/// The shared `$ref`-string grammar both ref consumers walk — the string check, the root form, the pointer form and
/// the refusal for a remote reference live here once.
fn parse_ref<'a>(reference: &'a Value, resources: &ResourceContext<'_>) -> Result<RefTarget<'a>, EngineRunError> {
    let Value::String(reference) = reference.untagged() else {
        return Err(raise("schema $ref must be a string", resources));
    };
    let text = reference.as_str();
    if text == "#" {
        return Ok(RefTarget::Root);
    }
    let rest = text
        .strip_prefix('#')
        .ok_or_else(|| raise("remote $ref requires an explicit extension", resources))?;
    if let Some(pointer) = rest.strip_prefix('/') {
        return Ok(RefTarget::Pointer(pointer));
    }
    Ok(RefTarget::Anchor(rest))
}

fn resolve_ref<'a>(
    reference: &'a Value,
    doc: &SchemaDoc<'a>,
    resources: &ResourceContext<'_>,
) -> Result<&'a Value, EngineRunError> {
    match parse_ref(reference, resources)? {
        RefTarget::Root => Ok(doc.root),
        RefTarget::Pointer(pointer) => {
            let tokens = pointer_tokens(pointer, resources)?;
            navigate(doc.root, &tokens, resources)
        }
        RefTarget::Anchor(rest) => {
            let tokens = doc
                .anchors
                .get(rest)
                .ok_or_else(|| raise(&format!("schema $ref names an unknown anchor `{rest}`"), resources))?;
            navigate(doc.root, tokens, resources)
        }
    }
}

/// The `schema_path` for a `$ref`: the ref's own target pointer tokens, so errors inside the referenced schema carry
/// the `#/$defs/...` location.
fn ref_schema_path(
    reference: &Value,
    doc: &SchemaDoc<'_>,
    resources: &ResourceContext<'_>,
) -> Result<Vec<String>, EngineRunError> {
    match parse_ref(reference, resources)? {
        RefTarget::Root => Ok(Vec::new()),
        RefTarget::Pointer(pointer) => pointer_tokens(pointer, resources),
        RefTarget::Anchor(rest) => doc
            .anchors
            .get(rest)
            .cloned()
            .ok_or_else(|| raise(&format!("schema $ref names an unknown anchor `{rest}`"), resources)),
    }
}

/// Splits one `/`-separated pointer into unescaped tokens.
fn pointer_tokens(pointer: &str, resources: &ResourceContext<'_>) -> Result<Vec<String>, EngineRunError> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    pointer
        .split('/')
        .map(|token| {
            let mut out = String::new();
            let mut chars = token.chars();
            while let Some(ch) = chars.next() {
                if ch == '~' {
                    match chars.next() {
                        Some('0') => out.push('~'),
                        Some('1') => out.push('/'),
                        // The pointer text is user input (the `$ref` string), so a malformed escape is a catchable
                        // raise like every sibling $ref refusal — never an internal contract violation.
                        _ => {
                            return Err(raise("schema $ref pointer has an invalid `~` escape", resources));
                        }
                    }
                } else {
                    out.push(ch);
                }
            }
            Ok(out)
        })
        .collect()
}

/// Navigates a value by pointer tokens (object keys, array indices).
fn navigate<'a>(
    value: &'a Value,
    tokens: &[String],
    resources: &ResourceContext<'_>,
) -> Result<&'a Value, EngineRunError> {
    let mut current = value;
    for token in tokens {
        match current.untagged() {
            Value::Object(object) => {
                let Some(child) = object.get(token) else {
                    return Err(raise(
                        &format!("schema $ref names a missing location `{token}`"),
                        resources,
                    ));
                };
                current = child;
            }
            Value::Array(array) => {
                let Ok(index) = token.parse::<usize>() else {
                    return Err(raise(
                        &format!("schema $ref names a non-index array location `{token}`"),
                        resources,
                    ));
                };
                let Some(child) = array.get(index) else {
                    return Err(raise(
                        &format!("schema $ref names a missing array index `{token}`"),
                        resources,
                    ));
                };
                current = child;
            }
            _ => {
                return Err(raise(
                    &format!("schema $ref names a location through a scalar `{token}`"),
                    resources,
                ));
            }
        }
    }
    Ok(current)
}

// --------------------------------------------------------------------------- Number helpers
// ---------------------------------------------------------------------------

/// Whether a number is an exact integer value.
fn is_integer_number(number: &Number) -> bool {
    if number.category() == NumberCategory::Integer {
        return true;
    }
    if let Some(decimal) = number.as_decimal() {
        let scale = decimal.scale();
        if scale <= 0 {
            return true;
        }
        // A machine coefficient checks divisibility in `i64` — the schema inference and validation paths call this
        // per number, and the `BigInt` parse below allocated on every decimal. `10^19` already exceeds `i64::MAX`, so a
        // scale of 19 or more divides only a zero coefficient.
        if let Some(coefficient) = decimal.coefficient().to_i64() {
            if scale >= 19 {
                return coefficient == 0;
            }
            let power = 10_i64.pow(u32::try_from(scale).unwrap_or(0));
            return coefficient % power == 0;
        }
        // coefficient % 10^scale == 0. A nonzero coefficient below 10^scale cannot be a multiple — decided without
        // building the divisor, which a retained literal with a whole-`i64` scale would make unbuildable.
        let Some(coefficient) = BigInt::parse(decimal.coefficient().as_str()) else {
            return false;
        };
        if coefficient.is_zero() {
            return true;
        }
        let Some(scale_usize) = usize::try_from(scale).ok() else {
            return false;
        };
        if scale_usize > coefficient.digit_count() {
            return false;
        }
        let Some(pow) = BigInt::pow10(scale_usize) else {
            return false;
        };
        match coefficient.div_rem(&pow) {
            Some((_, remainder)) => remainder.is_zero(),
            None => false,
        }
    } else if let Some(float) = number.as_float() {
        let value = float.get();
        value.is_finite() && value.fract() == 0.0
    } else {
        false
    }
}

/// Whether a number is strictly positive, under its exact kind: negative is false, and zero is false in EVERY spelling
/// — including a scaled decimal zero (`0.0`, `0E+2`), whose canonical rendering is not the text `"0"` and would slip
/// past a spelling comparison.
fn is_positive(number: &Number) -> bool {
    if is_negative(number) {
        return false;
    }
    !is_zero(number)
}

/// Whether a number's value is zero.
fn is_zero(number: &Number) -> bool {
    if let Some(integer) = number.to_integer() {
        return BigInt::parse(integer.as_str()).is_some_and(|parsed| parsed.is_zero());
    }
    if let Some(decimal) = number.as_decimal() {
        return BigInt::parse(decimal.coefficient().as_str()).is_some_and(|parsed| parsed.is_zero());
    }
    number.as_float().is_some_and(|float| float.get() == 0.0)
}

/// Whether a number is negative.
fn is_negative(number: &Number) -> bool {
    match number.to_integer() {
        Some(integer) => integer.as_str().starts_with('-'),
        None => match number.as_float() {
            Some(float) => float.get().is_finite() && float.get() < 0.0,
            None => number
                .as_decimal()
                .is_some_and(|d| d.coefficient().as_str().starts_with('-')),
        },
    }
}

/// Whether a number is finite (used by the numeric-bound shape checks).
fn is_finite(number: &Number) -> bool {
    match number.as_float() {
        Some(float) => float.get().is_finite(),
        None => true,
    }
}

/// Whether `value` is an exact integer multiple of `divisor`.
fn is_multiple(value: &Number, divisor: &Number) -> bool {
    if matches!(value.category(), NumberCategory::Float) || matches!(divisor.category(), NumberCategory::Float) {
        let v = to_f64(value);
        let d = to_f64(divisor);
        if d == 0.0 {
            return false;
        }
        let q = v / d;
        return q.is_finite() && q.fract() == 0.0;
    }
    // value / divisor = (va / da) * 10^(ds - vs). Align both to integers and check exact divisibility; an unbuildable
    // alignment (a scale gap past any materialization) declines to `false` rather than allocating an exabyte.
    let (va, vs) = exact_parts(value);
    let (da, ds) = exact_parts(divisor);
    let max_scale = vs.max(ds);
    let (Some(numerator), Some(denominator)) = (
        va.mul_pow10(usize::try_from(max_scale - vs).unwrap_or(usize::MAX)),
        da.mul_pow10(usize::try_from(max_scale - ds).unwrap_or(usize::MAX)),
    ) else {
        return false;
    };
    match numerator.div_rem(&denominator) {
        Some((_, remainder)) => remainder.is_zero(),
        None => false,
    }
}

/// The `(coefficient, scale)` exact parts of a number (scale 0 for integers).
fn exact_parts(number: &Number) -> (BigInt, i64) {
    match number.to_integer() {
        Some(integer) => (BigInt::parse(integer.as_str()).unwrap_or_else(BigInt::zero), 0),
        None => match number.as_decimal() {
            Some(decimal) => (
                BigInt::parse(decimal.coefficient().as_str()).unwrap_or_else(BigInt::zero),
                decimal.scale(),
            ),
            None => (BigInt::zero(), 0),
        },
    }
}

/// The binary64 value of a number (float-involving paths only).
fn to_f64(number: &Number) -> f64 {
    match number.category() {
        NumberCategory::Integer => number
            .to_integer()
            .and_then(|integer| integer.as_str().parse().ok())
            .unwrap_or(f64::NAN),
        NumberCategory::Decimal => crate::semantics::order::to_f64(number),
        NumberCategory::Float => number.as_float().map_or(f64::NAN, jqf_data::Float::get),
    }
}

/// Compares two numbers under the engine's exact total order.
fn number_compare(
    left: &Number,
    right: &Number,
    resources: &ResourceContext<'_>,
) -> Result<core::cmp::Ordering, EngineRunError> {
    let left = Value::Number(left.clone());
    let right = Value::Number(right.clone());
    total_cmp(&left, &right).map_err(|_| too_deep(resources))
}

/// Renders a number's canonical text (for messages), using the engine's own renderer so the spelling matches every
/// other number rendering in the tree.
fn render_number(number: &Number) -> String {
    let mut buf = String::new();
    match crate::semantics::render::write_number(&mut buf, number, usize::MAX) {
        Ok(()) => buf,
        Err(_) => number
            .to_integer()
            .map_or_else(String::new, |integer| integer.as_str().to_owned()),
    }
}

// --------------------------------------------------------------------------- Paths
// ---------------------------------------------------------------------------

/// Builds `instance_path` and `schema_path` pointers without per-node allocations: segments are pushed and popped
/// around a recursion.
struct PathCursor {
    instance: Vec<String>,
    schema: Vec<String>,
}

impl PathCursor {
    fn new() -> Self {
        Self {
            instance: Vec::new(),
            schema: Vec::new(),
        }
    }

    fn save_both(&self) -> (usize, usize) {
        (self.instance.len(), self.schema.len())
    }

    fn save_schema(&self) -> usize {
        self.schema.len()
    }

    fn restore_both(&mut self, instance: usize, schema: usize) {
        self.instance.truncate(instance);
        self.schema.truncate(schema);
    }

    fn restore_schema(&mut self, schema: usize) {
        self.schema.truncate(schema);
    }

    fn set_schema(&mut self, tokens: Vec<String>) {
        self.schema = tokens;
    }

    fn push_instance(&mut self, segment: impl AsRef<str>) {
        self.instance.push(escape_pointer(segment.as_ref()));
    }

    fn push_schema(&mut self, segment: &str) {
        self.schema.push(escape_pointer(segment));
    }

    fn push_schema_index(&mut self, index: usize) {
        self.schema.push(index.to_string());
    }

    fn render_instance(&self) -> String {
        let mut out = "$".to_owned();
        for segment in &self.instance {
            out.push('/');
            out.push_str(segment);
        }
        out
    }
}

/// Escapes one pointer token per RFC 6901 (`~` → `~0`, `/` → `~1`).
fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

impl ErrorDraft {
    fn new(cursor: &PathCursor, keyword: &'static str, message: String) -> Self {
        let mut schema_path = "#".to_owned();
        for segment in &cursor.schema {
            schema_path.push('/');
            schema_path.push_str(segment);
        }
        schema_path.push('/');
        schema_path.push_str(keyword);
        Self {
            instance_path: cursor.render_instance(),
            schema_path,
            keyword,
            message,
        }
    }

    fn into_value(self) -> Result<Value, EngineRunError> {
        let mut builder = ObjectBuilder::try_with_capacity(4).map_err(|_| EngineRunError::allocation_failure())?;
        for (key, value) in [
            ("instance_path", self.instance_path),
            ("schema_path", self.schema_path),
            ("keyword", String::from(self.keyword)),
            ("message", self.message),
        ] {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str(key).map_err(|_| EngineRunError::allocation_failure())?,
                    Value::try_string(&value).map_err(|_| EngineRunError::allocation_failure())?,
                )
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        builder
            .try_finish()
            .map(Value::Object)
            .map_err(|_| EngineRunError::allocation_failure())
    }
}

/// The instance keys in insertion order.
fn instance_keys(object: &Object) -> Result<Vec<String>, EngineRunError> {
    let mut keys = Vec::new();
    keys.try_reserve_exact(object.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for entry in object {
        keys.push(entry.key().to_owned());
    }
    Ok(keys)
}

fn too_deep(resources: &ResourceContext<'_>) -> EngineRunError {
    raise("schema comparison too deep", resources)
}

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

    fn json(text: &str, resources: &ResourceContext<'_>) -> Value {
        crate::semantics::decode::json(text, resources).expect("json")
    }

    fn options(text: &str, resources: &ResourceContext<'_>) -> InferOptions {
        InferOptions::parse(&json(text, resources), resources).expect("options")
    }

    fn infer(value: &str, opt: InferOptions, resources: &ResourceContext<'_>) -> Value {
        infer_schema_with_options(&json(value, resources), opt, resources).expect("infer")
    }

    fn schema_type(schema: &Value) -> String {
        let Value::Object(object) = schema.untagged() else {
            panic!("expected a schema object, got {schema:?}");
        };
        match object.get("type") {
            Some(Value::String(text)) => text.as_str().to_owned(),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| match item.untagged() {
                    Value::String(text) => text.as_str().to_owned(),
                    _ => panic!("expected a type name"),
                })
                .collect::<Vec<_>>()
                .join(","),
            other => panic!("expected a type, got {other:?}"),
        }
    }

    fn property_type(schema: &Value, key: &str) -> String {
        let Value::Object(object) = schema.untagged() else {
            panic!("expected a schema object");
        };
        let Some(Value::Object(properties)) = object.get("properties") else {
            panic!("expected properties");
        };
        let Some(property) = properties.get(key) else {
            panic!("missing property {key}");
        };
        schema_type(property)
    }

    fn items_property_type(schema: &Value, key: &str) -> String {
        let Value::Object(object) = schema.untagged() else {
            panic!("expected a schema object");
        };
        let Some(items) = object.get("items") else {
            panic!("expected items");
        };
        property_type(items, key)
    }

    /// JSON numbers still infer as integer / number under every dial.
    #[test]
    fn schema_infer_keeps_numeric_json_as_integer_and_number() {
        let resources = resources();
        let default = InferOptions::DEFAULT;
        let numeric = options(r#"{"strings":"numeric"}"#, &resources);
        assert_eq!(schema_type(&infer("37", default, &resources)), "integer");
        assert_eq!(schema_type(&infer("1.5", default, &resources)), "number");
        let folded = infer("[1, 1.5]", default, &resources);
        let Value::Object(object) = folded.untagged() else {
            panic!("expected object");
        };
        assert_eq!(schema_type(object.get("items").expect("items")), "number");
        let folded_numeric = infer("[1, 1.5]", numeric, &resources);
        assert!(
            semantic_eq(&folded, &folded_numeric).expect("eq"),
            "strings=numeric must not change actual JSON numbers"
        );
    }

    /// A CSV-shaped `{"age":"37"}` stays string under `/1` and under `{}`.
    #[test]
    fn schema_infer_default_keeps_numeric_looking_string() {
        let resources = resources();
        let from_one = infer_schema(&json(r#"{"age":"37"}"#, &resources), &resources).expect("one");
        let from_empty = infer(r#"{"age":"37"}"#, options("{}", &resources), &resources);
        assert_eq!(property_type(&from_one, "age"), "string");
        assert!(
            semantic_eq(&from_one, &from_empty).expect("eq"),
            "schema_infer/1 and schema_infer(VALUE; {{}}) must stay byte-identical"
        );
    }

    /// `strings: "numeric"` types a tonumber-accepted integer string as integer.
    #[test]
    fn schema_infer_numeric_string_becomes_integer() {
        let resources = resources();
        let schema = infer(
            r#"{"age":"37"}"#,
            options(r#"{"strings":"numeric"}"#, &resources),
            &resources,
        );
        assert_eq!(property_type(&schema, "age"), "integer");
    }

    /// `strings: "numeric"` types a non-integer number string as number.
    #[test]
    fn schema_infer_numeric_string_becomes_number() {
        let resources = resources();
        let schema = infer(
            r#"{"price":"37.5"}"#,
            options(r#"{"strings":"numeric"}"#, &resources),
            &resources,
        );
        assert_eq!(property_type(&schema, "price"), "number");
    }

    /// A string the tonumber scan refuses stays string.
    #[test]
    fn schema_infer_refused_numeric_string_stays_string() {
        let resources = resources();
        let schema = infer(
            r#"{"age":"37x"}"#,
            options(r#"{"strings":"numeric"}"#, &resources),
            &resources,
        );
        assert_eq!(property_type(&schema, "age"), "string");
    }

    /// A mixed column — `"37"` and `"x"` — stays string.
    #[test]
    fn schema_infer_mixed_numeric_string_column_stays_string() {
        let resources = resources();
        let schema = infer(
            r#"[{"age":"37"},{"age":"x"}]"#,
            options(r#"{"strings":"numeric"}"#, &resources),
            &resources,
        );
        assert_eq!(items_property_type(&schema, "age"), "string");
    }

    /// Every merged value a number, one of them not an integer, emits number.
    #[test]
    fn schema_infer_mixed_integer_and_number_strings_infer_number() {
        let resources = resources();
        let schema = infer(
            r#"[{"age":"37"},{"age":"38.5"}]"#,
            options(r#"{"strings":"numeric"}"#, &resources),
            &resources,
        );
        assert_eq!(items_property_type(&schema, "age"), "number");
    }

    /// An unknown `strings` value is a catchable error, never a silent no-op.
    #[test]
    fn schema_infer_unknown_strings_dial_is_an_error() {
        let resources = resources();
        let err =
            InferOptions::parse(&json(r#"{"strings":"coerce"}"#, &resources), &resources).expect_err("unknown dial");
        let EngineRunError::Raised(Value::String(text)) = err else {
            panic!("expected a catchable raise, got {err:?}");
        };
        assert!(
            text.as_str().contains("strings` must be one of type | enum | numeric"),
            "got {text:?}"
        );
    }

    // `schema_diff` over two SCHEMA documents is schema-aware — identical schemas diff to an empty array, and deltas
    // are path-keyed semantic records (properties/required/type unions), never the arguments' own shape noise.

    fn diff(left: &str, right: &str, resources: &ResourceContext<'_>) -> Value {
        schema_diff_law(&json(left, resources), &json(right, resources), resources).expect("schema_diff law")
    }

    fn assert_diff_eq(expected: &str, actual: &Value, resources: &ResourceContext<'_>) {
        let expected = json(expected, resources);
        assert!(
            semantic_eq(actual, &expected).unwrap_or(false),
            "schema_diff mismatch:\n  expected {expected:?}\n  actual   {actual:?}"
        );
    }

    #[test]
    fn schema_diff_identical_schemas_is_empty() {
        let resources = resources();
        for schema in [
            r#"{"type":"integer"}"#,
            r#"{"type":["string","null"]}"#,
            r#"{"required":["a"],"properties":{"a":{"type":"integer"}}}"#,
            r#"{"properties":{"a":{"items":{"type":"string"}},"b":{"enum":[1,2]}}}"#,
            r#"{"required":["b","a"],"properties":{"a":true,"b":false}}"#,
        ] {
            let out = diff(schema, schema, &resources);
            assert_diff_eq("[]", &out, &resources);
        }
    }

    #[test]
    fn schema_diff_required_set_delta_is_one_changed_record() {
        let resources = resources();
        let out = diff(r#"{"required":["a"]}"#, r#"{"required":["a","b"]}"#, &resources);
        assert_diff_eq(
            r#"[{"path":["required"],"kind":"changed","old":["a"],"new":["a","b"]}]"#,
            &out,
            &resources,
        );
    }

    #[test]
    fn schema_diff_type_scalar_change_is_a_type_record() {
        let resources = resources();
        let out = diff(r#"{"type":"integer"}"#, r#"{"type":"string"}"#, &resources);
        assert_diff_eq(
            r#"[{"path":["type"],"kind":"changed","old":"integer","new":"string"}]"#,
            &out,
            &resources,
        );
    }

    #[test]
    fn schema_diff_nested_property_type_change_is_path_keyed() {
        let resources = resources();
        let out = diff(
            r#"{"properties":{"a":{"type":"integer"}}}"#,
            r#"{"properties":{"a":{"type":"string"}}}"#,
            &resources,
        );
        assert_diff_eq(
            r#"[{"path":["properties","a","type"],"kind":"changed","old":"integer","new":"string"}]"#,
            &out,
            &resources,
        );
    }

    #[test]
    fn schema_diff_value_mode_against_a_contract_still_reports_drift() {
        let resources = resources();
        // The original VALUE;SCHEMA framing is unchanged: bare data on the left infers first.
        let out = diff(
            r#"{"id":1,"name":"Ada"}"#,
            r#"{"type":"object","required":["id"],"properties":{"id":{"type":"integer"}}}"#,
            &resources,
        );
        assert_diff_eq(
            r#"[{"path":["properties","name"],"kind":"added","new":{"type":"string"}},{"path":["required"],"kind":"changed","old":["id"],"new":["id","name"]}]"#,
            &out,
            &resources,
        );
    }

    fn errors(value: &str, schema: &str, resources: &ResourceContext<'_>) -> Vec<Value> {
        validate_errors(&json(value, resources), &json(schema, resources), resources).expect("validation runs")
    }

    /// A scaled decimal zero (`0.0`) is zero: the multipleOf gate refuses it with the greater-than-0 sentence at SCHEMA
    /// time, exactly as plain `0` gets — never by declining every instance through a zero denominator.
    #[test]
    fn multiple_of_scaled_zero_is_refused_like_plain_zero() {
        let resources = resources();
        for schema in [r#"{"multipleOf":0}"#, r#"{"multipleOf":0.0}"#] {
            let result = validate_errors(&json("6", &resources), &json(schema, &resources), &resources);
            let rendered = format!("{result:?}");
            assert!(
                result.is_err() && rendered.contains("greater than 0"),
                "schema {schema} must raise the greater-than-0 refusal, got {rendered}"
            );
        }
    }

    /// A NaN instance ranks BELOW every number under the engine's one NaN law, so it violates both lower-bound keywords
    /// and satisfies both upper-bound keywords — the same answers the engine's own comparison operators give. This
    /// pins that the walker derives all four from the one comparator.
    #[test]
    fn nan_instance_fails_lower_bounds_and_passes_upper_bounds() {
        let resources = resources();
        assert_eq!(
            errors("nan", r#"{"minimum":1}"#, &resources).len(),
            1,
            "NaN is below the minimum"
        );
        assert_eq!(
            errors("nan", r#"{"exclusiveMinimum":1}"#, &resources).len(),
            1,
            "NaN is not above an exclusive minimum"
        );
        assert!(
            errors("nan", r#"{"maximum":1}"#, &resources).is_empty(),
            "NaN satisfies <= under its below-everything rank"
        );
        assert!(
            errors("nan", r#"{"exclusiveMaximum":1}"#, &resources).is_empty(),
            "NaN satisfies < under its below-everything rank"
        );
    }

    /// The sort-adjacent uniqueness law answers what the pairwise sweep
    /// answers: the total order ranks duplicates adjacent (NaN-against-NaN
    /// included, via its `Equal` leaf), so a miss here would be a silent
    /// false "unique". Deterministic LCG shuffles over a pool straddling the
    /// equality edges: NaN, -0/0, retained-spelling numbers, strings that
    /// equal numbers' texts, and nested arrays.
    #[test]
    fn sort_adjacent_uniqueness_matches_the_pairwise_sweep() {
        let resources = resources();
        let pool = [
            json("0", &resources),
            Value::Number(Number::try_json_literal("-0").expect("literal")),
            nan_value(),
            nan_value(),
            json("1", &resources),
            json("1.0", &resources),
            json("\"1\"", &resources),
            json("true", &resources),
            json("null", &resources),
            json("[1,2]", &resources),
            json("[2,1]", &resources),
            json("{\"a\":1}", &resources),
            json("{\"a\":1}", &resources),
            json("{\"a\":\"1\"}", &resources),
        ];
        let pairwise = |items: &[Value], _res: &ResourceContext<'_>| -> bool {
            for (index, left) in items.iter().enumerate() {
                for right in items.iter().skip(index + 1) {
                    if semantic_eq(left, right).expect("pool stays shallow") {
                        return false;
                    }
                }
            }
            true
        };
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        for round in 0..64 {
            let mut items: Vec<Value> = Vec::new();
            let count = 2 + (round % 6);
            for _ in 0..count {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                items.push(pool[(seed >> 33) as usize % pool.len()].clone());
            }
            let array = Array::try_from_vec(items.clone()).expect("array");
            assert_eq!(
                is_unique(&array, &resources).expect("pool stays shallow"),
                pairwise(&items, &resources),
                "round {round}, items {items:?}"
            );
        }
    }

    /// The one NaN number, constructed locally so the differential does not
    /// reach into the order module's test fixtures.
    fn nan_value() -> Value {
        Value::Number(Number::float(jqf_data::Float::new(f64::NAN)))
    }
}
