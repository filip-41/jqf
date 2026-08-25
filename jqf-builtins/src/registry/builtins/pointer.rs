//! The jqf JSON-Pointer extension family: `json_pointer/1` and `json_pointer/2`.
//!
//! JSON Pointer (RFC 6901) addresses a value inside a JSON document by a `/`-separated reference-token string, with
//! `~0`/`~1` unescaping. It is a extension surface — the reference has no JSON Pointer — and the contract authority
//! is `docs/architecture/builtin-library.md` §"Paths, selection, and traversal":
//!
//! > JSON Pointer follows RFC 6901 exactly and returns either one match or an
//! > empty array for each source value.
//!
//! Both arities are READ laws, per that sentence and the old-base reference implementation:
//!
//! - `json_pointer(PATH)` — navigate the INPUT value by each pointer `PATH`
//!   evaluates to and emit one array per pointer: `[match]` or `[]`.
//! - `json_pointer(SOURCE; PATH)` — navigate each source value by each pointer
//!   and emit one array per source value, per pointer.
//!
//! `PATH` is an ORDINARY filter argument in both arities: every output is one pointer, so `json_pointer("/a","/b")`
//! emits two arrays and `json_pointer(empty)` emits none. It is also the RIGHTMOST argument, which makes it the outer
//! loop of the right-outer Cartesian argument law — every source is navigated by one pointer before the next pointer
//! starts.
//!
//! The evaluator collects all argument outputs before navigation begins (the engine's house pattern for owned
//! evaluators; see `emit_argument_product`).
//! Arrays collected for earlier pointer×source combinations ARE emitted before a navigation error raises — the
//! `PathEmit` frame carries both the accumulated values and the pending error — but an error during ARGUMENT
//! EVALUATION drops the prefix: the `?` propagates before any `PathEmit` frame is created.
//!
//! Navigation is exact: an object addresses a member by key (last-wins for duplicates, the `Object` law); an array
//! addresses a non-negative base-10 integer index with no leading zeros (RFC 6901 §4); the `-` token never matches a
//! read (RFC 6901's append position); a non-integer or leading-zero-prefixed array token raises; a missing member, an
//! out-of-range index, or a scalar on the way to a deeper token is no match (the empty array). An empty pointer is the
//! whole document.
//!
//! The URI-fragment representation (RFC 6901 §6) is accepted too: a leading `#` percent-decodes the rest before the
//! pointer is parsed, so `json_pointer("#/a%20b")` equals `json_pointer("/a b")`.
//!
//! The family declares [`DemandTransfer::Subtree`]: a pointer can address any location in its source, so no shallower
//! demand is honest — the same answer `getpath`/`setpath`/`delpaths` give for the same reason.

use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::{Array, Value};
use jqf_resource::ResourceContext;

use super::id;
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// One JSON-Pointer law, one evaluator shape.
#[derive(Clone, Copy, Debug)]
pub enum PointerLaw {
    /// `json_pointer/1`: navigate the input value, one `[match]`/`[]` output.
    Read,
    /// `json_pointer/2`: navigate each source value, one array per source.
    ReadSource,
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

/// The JSON Pointer family record.
pub const JSON_POINTER_FAMILY: BuiltinFamilyRecord = family(
    id::JSON_POINTER_FAMILY_ID,
    "json_pointer",
    "Address a value by an RFC 6901 JSON Pointer.",
    "`json_pointer(PATH)` navigates the input by the pointer string and emits \
     `[match]` or `[]`; `json_pointer(SOURCE; PATH)` navigates each source \
     value, one array per source. `~0`/`~1` unescaping, the `#` URI-fragment \
     form, and exact array-index validation follow RFC 6901.",
);

/// `json_pointer/1`: the PATH argument is a filter over the input and every output is one pointer (the argument law);
/// each pointer navigates the input value and emits its own `[match]`/`[]` array, in argument order.
pub const JSON_POINTER_ONE: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JSON_POINTER_1),
    family: BuiltinFamilyId::new(id::JSON_POINTER_FAMILY_ID),
    canonical_name: "json_pointer",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        example(
            "json_pointer(\"/rows/0/id\")",
            r#"{"rows":[{"id":1},{"name":"second"}],"a/b":2,"c~d":3}"#,
            "[1]\n",
        ),
        example("json_pointer(\"\")", r#"{"a":1}"#, "[{\"a\":1}]\n"),
        example("json_pointer(\"/missing\")", r#"{"a":1}"#, "[]\n"),
        example("json_pointer(\"/rows/-\")", r#"{"rows":[1]}"#, "[]\n"),
        example("json_pointer(\"/a~1b\")", r#"{"a/b":2}"#, "[2]\n"),
        example("json_pointer(\"/c~0d\")", r#"{"c~d":3}"#, "[3]\n"),
        // A MULTI-VALUED path argument iterates like every other filter argument, one output per pointer in argument
        // order. Nothing else in this record distinguishes iteration from last-wins.
        example("[json_pointer(\"/a\", \"/b\")]", r#"{"a":1,"b":2}"#, "[[1],[2]]\n"),
        example("[json_pointer(empty)]", r#"{"a":1}"#, "[]\n"),
    ],
};

/// `json_pointer/2`: both arguments are filters over the input; the RIGHTMOST (PATH) is the outer loop of the
/// right-outer Cartesian argument law, so one array is emitted per source value, per pointer.
pub const JSON_POINTER_TWO: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JSON_POINTER_2),
    family: BuiltinFamilyId::new(id::JSON_POINTER_FAMILY_ID),
    canonical_name: "json_pointer",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        example("json_pointer(.; \"/a\")", r#"{"a":1}"#, "[1]\n"),
        example(
            "[json_pointer(empty; \"/x\"), json_pointer(.; \"\"), json_pointer(.; \"/\")]",
            r#"{"":"empty-key","x":1}"#,
            "[[{\"\":\"empty-key\",\"x\":1}],[\"empty-key\"]]\n",
        ),
        // The PATH argument iterates, and it is the OUTER loop: every source is navigated by the first pointer before
        // the second pointer starts (last-wins would silently drop `/b`).
        example("[json_pointer(.; \"/b\", \"/a\")]", r#"{"a":1,"b":2}"#, "[[2],[1]]\n"),
        example(
            "[json_pointer(.x, .y; \"/a\", \"/b\")]",
            r#"{"x":{"a":1,"b":2},"y":{"a":3,"b":4}}"#,
            "[[1],[3],[2],[4]]\n",
        ),
    ],
};

/// The overload and family slices the registry aggregates.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[JSON_POINTER_FAMILY];
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[JSON_POINTER_ONE, JSON_POINTER_TWO];

/// The JSON-Pointer execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
pub const PAYLOADS: &[(u16, PointerLaw)] = &[
    (id::JSON_POINTER_1, PointerLaw::Read),
    (id::JSON_POINTER_2, PointerLaw::ReadSource),
];

/// Parses one RFC 6901 pointer VALUE into its decoded reference tokens.
///
/// A non-string path is the `json_pointer path must be a string` error. An empty pointer is the whole document (zero
/// tokens). A non-empty pointer must start with `/`; a leading `#` is the URI-fragment representation and is
/// percent-decoded first. Each token is unescaped (`~0` → `~`, `~1` → `/`).
pub fn parse_tokens(path: &Value, resources: &ResourceContext<'_>) -> Result<Vec<String>, EngineRunError> {
    let Value::String(text) = path.untagged() else {
        return Err(crate::semantics::path::raise(
            "json_pointer path must be a string",
            resources,
        ));
    };
    parse_pointer(text.as_str(), resources)
}

/// Parses one RFC 6901 pointer string into its decoded reference tokens.
fn parse_pointer(path: &str, resources: &ResourceContext<'_>) -> Result<Vec<String>, EngineRunError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    // Only the URI-fragment form needs a decoded copy; the ordinary form is parsed straight out of the already-resident
    // path string.
    let fragment;
    let pointer: &str = if let Some(rest) = path.strip_prefix('#') {
        fragment = percent_decode(rest, resources)?;
        &fragment
    } else {
        path
    };
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    if !pointer.starts_with('/') {
        return Err(crate::semantics::path::raise(
            "json_pointer path must be empty or start with `/`",
            resources,
        ));
    }
    let mut tokens = Vec::new();
    for token in pointer[1..].split('/') {
        tokens
            .try_reserve(1)
            .map_err(|_| EngineRunError::allocation_failure())?;
        tokens.push(unescape_token(token, resources)?);
    }
    Ok(tokens)
}

/// Unescapes one RFC 6901 reference token: `~0` → `~`, `~1` → `/`. Any other `~` sequence is the invalid-escape
/// error.
fn unescape_token(token: &str, resources: &ResourceContext<'_>) -> Result<String, EngineRunError> {
    // The decoded token is never longer than the source token, and the source token is already resident — so one
    // fallible reservation of that length is the whole allocation this function makes.
    let mut decoded = String::new();
    decoded
        .try_reserve(token.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut chars = token.chars();
    while let Some(ch) = chars.next() {
        if ch != '~' {
            decoded.push(ch);
            continue;
        }
        match chars.next() {
            Some('0') => decoded.push('~'),
            Some('1') => decoded.push('/'),
            _ => {
                return Err(crate::semantics::path::raise(
                    "json_pointer path contains an invalid `~` escape",
                    resources,
                ));
            }
        }
    }
    Ok(decoded)
}

/// Percent-decodes RFC 3986 encoding (`%XX` → byte), requiring the result to be valid UTF-8.
fn percent_decode(fragment: &str, resources: &ResourceContext<'_>) -> Result<String, EngineRunError> {
    let bytes = fragment.as_bytes();
    // Percent-decoding only shrinks (`%XX` becomes one byte), and the fragment is already resident, so the source
    // length is an exact upper bound and one fallible reservation covers the whole walk.
    let mut decoded: Vec<u8> = Vec::new();
    decoded
        .try_reserve(bytes.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = hex(bytes.get(index + 1), resources)?;
            let lo = hex(bytes.get(index + 2), resources)?;
            decoded.push((hi << 4) | lo);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| crate::semantics::path::raise("json_pointer path contains an invalid percent escape", resources))
}

/// One hex nibble, with the malformed-escape error on a non-hex byte.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a base-16 digit is at most 15, so the u32 -> u8 cast cannot truncate"
)]
fn hex(byte: Option<&u8>, resources: &ResourceContext<'_>) -> Result<u8, EngineRunError> {
    let Some(&byte) = byte else {
        return Err(crate::semantics::path::raise(
            "json_pointer path contains an invalid percent escape",
            resources,
        ));
    };
    char::from(byte)
        .to_digit(16)
        .map(|digit| digit as u8)
        .ok_or_else(|| crate::semantics::path::raise("json_pointer path contains an invalid percent escape", resources))
}

/// The array-index half of one reference token: `-` is the append position, which never matches a read; any other token
/// must be a non-negative base-10 integer with no leading zeros (RFC 6901 §4).
fn array_position(token: &str, resources: &ResourceContext<'_>) -> Result<Option<usize>, EngineRunError> {
    if token == "-" {
        return Ok(None);
    }
    if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(crate::semantics::path::raise(
            "json_pointer array token must be a non-negative base-10 integer",
            resources,
        ));
    }
    if token.len() > 1 && token.starts_with('0') {
        return Err(crate::semantics::path::raise(
            "json_pointer array token must be a non-negative base-10 integer with no leading zeros",
            resources,
        ));
    }
    token.parse::<usize>().map(Some).map_err(|_| {
        crate::semantics::path::raise(
            "json_pointer array token must be a non-negative base-10 integer",
            resources,
        )
    })
}

/// Navigates one value by the decoded tokens, returning `[match]` or `[]`.
///
/// Every miss — a missing member, an out-of-range or `-` array token, or a scalar on the way to a deeper token — is
/// the EMPTY array. A non-integer array token is the array-token error, not a miss.
pub fn match_at(value: &Value, tokens: &[String], resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    let Some(matched) = navigate(value, tokens, resources)? else {
        // The path-miss cell's pointer form: the RFC 6901 read law answers the EMPTY ARRAY for a miss, where `getpath`
        // answers `null`. Both are the same frozen cell; the cell's lenient answer is the extension's own contract
        // value.
        crate::error::mismatch::resolve_at(resources, crate::error::mismatch::MismatchCell::PathMiss, false, ())?;
        return empty_array(resources);
    };
    let mut items = Vec::new();
    items
        .try_reserve_exact(1)
        .map_err(|_| EngineRunError::allocation_failure())?;
    items.push(matched);
    Array::try_from_vec(items)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// The empty pointer array (`[]`).
fn empty_array(_resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    Array::try_from_vec(Vec::new())
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// Walks `tokens` into `value`, returning the cloned match or `None` for a miss.
fn navigate(
    value: &Value,
    tokens: &[String],
    resources: &ResourceContext<'_>,
) -> Result<Option<Value>, EngineRunError> {
    let mut current = value;
    for token in tokens {
        match current.untagged() {
            Value::Object(object) => {
                let Some(child) = object.get(token) else {
                    return Ok(None);
                };
                current = child;
            }
            Value::Array(array) => {
                let Some(position) = array_position(token, resources)? else {
                    return Ok(None);
                };
                let Some(child) = array.get(position) else {
                    return Ok(None);
                };
                current = child;
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(current.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use jqf_data::{ObjectBuilder, ObjectKey};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn string(text: &str) -> Value {
        let _resources = resources();
        Value::try_string(text).expect("string")
    }

    fn object(entries: &[(&str, &str)]) -> Value {
        let _resources = resources();
        let mut builder = ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
        for &(key, value) in entries {
            builder
                .try_insert_last(
                    ObjectKey::try_from_str(key).expect("key"),
                    Value::try_string(value).expect("value"),
                )
                .expect("insert");
        }
        Value::Object(builder.try_finish().expect("object"))
    }

    fn array(items: Vec<Value>) -> Value {
        let _resources = resources();
        Value::Array(Array::try_from_vec(items).expect("array"))
    }

    /// The fixture the corpus's `json_pointer` block navigates.
    fn catalog() -> Value {
        let _resources = resources();
        let rows = array(vec![object(&[("id", "1")]), object(&[("name", "second")])]);
        let mut builder = ObjectBuilder::try_with_capacity(1).expect("builder");
        builder
            .try_insert_last(ObjectKey::try_from_str("rows").expect("key"), rows)
            .expect("insert");
        Value::Object(builder.try_finish().expect("object"))
    }

    fn tokens(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| String::from(*part)).collect()
    }

    fn raised_text(error: EngineRunError) -> String {
        match error {
            EngineRunError::Raised(Value::String(text)) => String::from(text.as_str()),
            other => panic!("expected a raised string, got {other:?}"),
        }
    }

    fn render(value: &Value) -> String {
        let mut line = String::new();
        crate::semantics::render::write_value(&mut line, value).expect("render");
        line
    }

    fn position(token: &str) -> Result<Option<usize>, EngineRunError> {
        let resources = resources();
        array_position(token, &resources)
    }

    /// RFC 6901 §4: an array index token has no leading zeros.
    /// The distinct raise is the module header's leading-zero law, separate from the plain non-integer class.
    #[test]
    fn leading_zero_array_tokens_raise() {
        for token in ["01", "00", "010", "007"] {
            let error = position(token).expect_err("a leading-zero token raises");
            assert_eq!(
                raised_text(error),
                "json_pointer array token must be a non-negative base-10 integer with no leading zeros",
                "token {token:?}"
            );
        }
    }

    #[test]
    fn plain_array_tokens_are_indices() {
        assert_eq!(position("0").expect("zero"), Some(0));
        assert_eq!(position("1").expect("one"), Some(1));
        assert_eq!(position("10").expect("ten"), Some(10));
        assert_eq!(position("123").expect("hundred"), Some(123));
    }

    /// The `-` token is RFC 6901's append position and never matches a read.
    #[test]
    fn the_append_position_never_matches_a_read() {
        assert_eq!(position("-").expect("append position"), None);
    }

    #[test]
    fn non_integer_array_tokens_raise() {
        for token in ["", "1a", "-1", "+1", "1.5", " 1", "1 "] {
            let error = position(token).expect_err("a non-integer token raises");
            assert_eq!(
                raised_text(error),
                "json_pointer array token must be a non-negative base-10 integer",
                "token {token:?}"
            );
        }
    }

    /// The pointer grammar the module header claims: an empty pointer is zero tokens, a non-empty pointer must start
    /// with `/`, `~0`/`~1` unescape, and any other `~` sequence raises the invalid-escape class.
    #[test]
    fn parse_tokens_splits_unescapes_and_rejects() {
        let resources = resources();
        let tokens = parse_tokens(&string(""), &resources).expect("empty pointer");
        assert!(tokens.is_empty());

        let tokens = parse_tokens(&string("/a/b/c"), &resources).expect("pointer");
        let text: Vec<&str> = tokens.iter().map(String::as_str).collect();
        assert_eq!(text, ["a", "b", "c"]);

        let tokens = parse_tokens(&string("/a~1b/c~0d"), &resources).expect("pointer");
        let text: Vec<&str> = tokens.iter().map(String::as_str).collect();
        assert_eq!(text, ["a/b", "c~d"]);

        let error = parse_tokens(&string("a"), &resources).expect_err("no leading slash");
        assert_eq!(raised_text(error), "json_pointer path must be empty or start with `/`");

        let error = parse_tokens(&string("/a~2b"), &resources).expect_err("invalid escape");
        assert_eq!(raised_text(error), "json_pointer path contains an invalid `~` escape");
    }

    /// The URI-fragment form (RFC 6901 §6): a leading `#` percent-decodes the rest before the pointer is parsed, and a
    /// malformed escape raises.
    #[test]
    fn fragment_form_percent_decodes() {
        let resources = resources();
        let tokens = parse_tokens(&string("#/a%20b"), &resources).expect("fragment");
        let text: Vec<&str> = tokens.iter().map(String::as_str).collect();
        assert_eq!(text, ["a b"]);

        let tokens = parse_tokens(&string("#/caf%C3%A9"), &resources).expect("fragment");
        let text: Vec<&str> = tokens.iter().map(String::as_str).collect();
        assert_eq!(text, ["café"]);

        let error = parse_tokens(&string("#/a%zz"), &resources).expect_err("bad hex");
        assert_eq!(
            raised_text(error),
            "json_pointer path contains an invalid percent escape"
        );

        let error = parse_tokens(&string("#/a%"), &resources).expect_err("truncated escape");
        assert_eq!(
            raised_text(error),
            "json_pointer path contains an invalid percent escape"
        );
    }

    /// The full read law through `match_at`: a hit is `[match]`, a miss on the way — missing member, out-of-range
    /// index, `-`, a scalar on the way — is the empty array, and a leading-zero index on an ARRAY raises through the
    /// same path the evaluator drives.
    #[test]
    fn match_at_publishes_hits_misses_and_the_leading_zero_raise() {
        let resources = resources();
        let value = catalog();

        let hit = match_at(&value, &tokens(&["rows", "1", "name"]), &resources).expect("a legal index navigates");
        assert_eq!(render(&hit), "[\"second\"]");

        for miss in [
            tokens(&["missing"]),
            tokens(&["rows", "3"]),
            tokens(&["rows", "-"]),
            tokens(&["rows", "0", "id", "x"]),
        ] {
            let result = match_at(&value, &miss, &resources).expect("a miss is the empty array");
            assert_eq!(render(&result), "[]", "tokens {miss:?}");
        }

        // A miss short-circuits BEFORE a deeper token is validated: `/rows/01` over a document without `rows` is the
        // empty array, never a raise.
        let unrelated = object(&[("a", "1")]);
        let result =
            match_at(&unrelated, &tokens(&["rows", "01"]), &resources).expect("the miss preempts the deeper token");
        assert_eq!(render(&result), "[]");

        let error = match_at(&value, &tokens(&["rows", "01", "name"]), &resources)
            .expect_err("a leading-zero index on an array raises");
        assert_eq!(
            raised_text(error),
            "json_pointer array token must be a non-negative base-10 integer with no leading zeros"
        );
    }
}
