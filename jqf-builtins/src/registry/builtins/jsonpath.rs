//! The jqf `JSONPath` extension family: `jsonpath/1` and `jsonpath/2`.
//!
//! `JSONPath` (RFC 9535, the 2024 IETF standard) addresses values inside a JSON document by a query string: root `$`,
//! child segments `[<selectors>]` and `.name`/`.*` shorthands, descendant segments `..name`/`..*`/`..[<selectors>]`,
//! name/index/wildcard/slice selectors, filter selectors `[?<logical-expr>]` with the RFC's comparison semantics, and
//! the five standard function extensions `length()`, `count()`, `match()`, `search()`, and `value()`.
//! It is an extension surface — the reference has no `JSONPath` — and the contract authority is
//! `docs/architecture/builtin-library.md` §"Paths, selection, and traversal".
//!
//! The v1 profile IS the full RFC 9535 surface (this module follows the RFC exactly, not a dialect zoo); the sentence
//! in the architecture doc that bounded the original design predates this implementation, and its bounded list (no
//! scripts, no arithmetic, no mixed unions) is superseded for the surface shipped here. The RFC 9535 Compliance Test
//! Suite (`tools/jsonpath-compliance-test-suite/`, gate `make jsonpath-conformance`) is the standing oracle and pins
//! every behavior in this module.
//!
//! Both arities are READ laws, the same product shape as `json_pointer`:
//!
//! - `jsonpath(QUERY)` — evaluate each query string the QUERY filter yields
//!   over the INPUT value and emit one nodelist array per query. A nodelist is the array of the matched values, in
//!   nodelist order.
//! - `jsonpath(SOURCE; QUERY)` — navigate each source value by each query and
//!   emit one nodelist array per source value, per query.
//!
//! `QUERY` is an ORDINARY filter argument in both arities: every output is one query string, so `jsonpath("$.a",
//! "$.b")` emits two arrays and `jsonpath(empty)` emits none. It is also the RIGHTMOST argument, which makes it the
//! outer loop of the right-outer Cartesian argument law — every source is navigated by one query before the next
//! query starts.
//!
//! The evaluator collects all argument outputs before navigation begins (the engine's house pattern for owned
//! evaluators; see `emit_argument_product`).
//! Arrays collected for earlier query×source combinations ARE emitted before a navigation error raises — the
//! `PathEmit` frame carries both the accumulated values and the pending error — but an error during ARGUMENT
//! EVALUATION drops the prefix: the `?` propagates before any `PathEmit` frame is created.
//!
//! A query string is parsed per RFC 9535 §2: a `$` root followed by zero or more segments. The jqf builtin also
//! accepts a BARE query (no leading `$`), which is parsed as if the root were present — `jsonpath(".a")` equals
//! `jsonpath("$.a")`. A query that is not well-formed and valid (RFC §2.1:
//! syntax errors, non-I-JSON index/slice bounds, ill-typed function expressions) raises; the error is raised through
//! the same semantic channel as `json_pointer`'s parse errors.
//!
//! Where the RFC leaves a choice, this module documents it and the CTS pins it:
//!
//! - Object member iteration order is the document's own order (jqf's
//!   `Object` law, last-wins for duplicate names), which is one of the orders the RFC permits — the suite's `results`
//!   (plural) cases list every valid order and jqf's deterministic order is always among them.
//! - A filter over an object selects member values in the same order.
//! - `match()`/`search()` compile their regex through the engine's shared
//!   compiled-regex cache ([`crate::registry::builtins::regex`]), with `match()` anchoring the pattern (`\A(?:…)\z`). A
//!   pattern the engine's regex tier cannot compile is `LogicalFalse`, per RFC §2.4.6/2.4.7 (a non-conforming second
//!   argument is not an error).
//! - Comparisons use jqf's number equality law (exact across representations:
//!   `1 == 1.0`), and `<`/`<=`/`>`/`>=` order only number-number and string-string pairs, exactly as RFC §2.3.5.2.2
//!   specifies.
//!
//! The family declares [`DemandTransfer::Subtree`]: a `JSONPath` can address any location in its source, so no
//! shallower demand is honest — the same answer `json_pointer` gives for the same reason.

use alloc::borrow::ToOwned;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use jqf_data::{Array, Integer, Number, Value};
use jqf_resource::ResourceContext;

use super::id;
use super::regex as regex_builtins;
use crate::error::EngineRunError;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// The I-JSON exact-integer bound RFC 9535 §2.1 imposes on index and slice bounds: `[-(2^53)+1, (2^53)-1]`.
const I_JSON_BOUND: i64 = 9_007_199_254_740_991;

/// One `JSONPath` law, one evaluator shape.
#[derive(Clone, Copy, Debug)]
pub enum JsonPathLaw {
    /// `jsonpath/1`: navigate the input value, one nodelist array per query.
    Read,
    /// `jsonpath/2`: navigate each source value, one array per source.
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

/// The `JSONPath` family record.
pub const JSONPATH_FAMILY: BuiltinFamilyRecord = family(
    id::JSONPATH_FAMILY_ID,
    "jsonpath",
    "Address values by an RFC 9535 JSONPath query.",
    "`jsonpath(QUERY)` evaluates each query string over the input and emits \
     one array of the matched values per query; `jsonpath(SOURCE; QUERY)` \
     navigates each source value, one array per source. Root/child/descendant \
     segments, name/index/wildcard/slice/filter selectors, and the \
     `length`/`count`/`match`/`search`/`value` function extensions follow \
     RFC 9535 exactly.",
);

/// `jsonpath/1`: the QUERY argument is a filter over the input and every output is one query string (the argument law);
/// each query navigates the input value and emits its own nodelist array, in argument order.
pub const JSONPATH_ONE: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JSONPATH_1),
    family: BuiltinFamilyId::new(id::JSONPATH_FAMILY_ID),
    canonical_name: "jsonpath",
    arity: 1,
    parameters: &[ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        example(
            "jsonpath(\"$.store.book[*].title\")",
            r#"{"store":{"book":[{"title":"A"},{"title":"B"}]}}"#,
            "[\"A\",\"B\"]\n",
        ),
        example(
            "jsonpath(\"$.store.book[?@.price < 10].title\")",
            r#"{"store":{"book":[{"price":5,"title":"A"},{"price":15,"title":"B"}]}}"#,
            "[\"A\"]\n",
        ),
        example(
            "jsonpath(\"$..author\")",
            r#"{"a":{"author":"X"},"b":{"c":{"author":"Y"}}}"#,
            "[\"X\",\"Y\"]\n",
        ),
        example("jsonpath(\"$[0:4:2]\")", r#"["a","b","c","d"]"#, "[\"a\",\"c\"]\n"),
        example("jsonpath(\"$[*]\")", r#"{"a":1,"b":2}"#, "[1,2]\n"),
        example("jsonpath(\"$\")", r#"{"a":1}"#, "[{\"a\":1}]\n"),
        // A bare query (no leading `$`) is the same query over the same root.
        example("jsonpath(\".a\")", r#"{"a":1}"#, "[1]\n"),
        example("jsonpath(\"$.missing\")", r#"{"a":1}"#, "[]\n"),
        // A MULTI-VALUED query argument iterates like every other filter argument, one output per query in argument
        // order.
        example("[jsonpath(\"$.a\", \"$.b\")]", r#"{"a":1,"b":2}"#, "[[1],[2]]\n"),
        example("[jsonpath(empty)]", r#"{"a":1}"#, "[]\n"),
        // An invalid query is a raised error, catchable like the reference's own.
        example(
            "[try jsonpath(\"$[?count(1)]\") catch \"bad\"]",
            r#"{"a":1}"#,
            "[\"bad\"]\n",
        ),
    ],
};

/// `jsonpath/2`: both arguments are filters over the input; the RIGHTMOST (QUERY) is the outer loop of the right-outer
/// Cartesian argument law, so one nodelist array is emitted per source value, per query.
pub const JSONPATH_TWO: BuiltinOverloadRecord = BuiltinOverloadRecord {
    id: BuiltinOverloadId::new(id::JSONPATH_2),
    family: BuiltinFamilyId::new(id::JSONPATH_FAMILY_ID),
    canonical_name: "jsonpath",
    arity: 2,
    parameters: &[ParameterKind::Filter, ParameterKind::Filter],
    execution: BuiltinExecution::Evaluator,
    demand_transfer: DemandTransfer::Subtree,
    semantic_revision: SemanticRevision::new(1),
    effects: Effects::Pure,
    examples: &[
        example("jsonpath(.; \"$.a\")", r#"{"a":1}"#, "[1]\n"),
        // The QUERY argument iterates, and it is the OUTER loop: every source is navigated by the first query before
        // the second query starts (the same right-outer law `json_pointer(.; "/b", "/a")` pins).
        example("[jsonpath(.; \"$.b\", \"$.a\")]", r#"{"a":1,"b":2}"#, "[[2],[1]]\n"),
        example(
            "[jsonpath(.x, .y; \"$.a\", \"$.b\")]",
            r#"{"x":{"a":1,"b":2},"y":{"a":3,"b":4}}"#,
            "[[1],[3],[2],[4]]\n",
        ),
    ],
};

/// The overload and family slices the registry aggregates.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[JSONPATH_FAMILY];
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[JSONPATH_ONE, JSONPATH_TWO];

/// The `JSONPath` execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
#[cfg(feature = "ext-jsonpath")]
pub const PAYLOADS: &[(u16, JsonPathLaw)] = &[
    (id::JSONPATH_1, JsonPathLaw::Read),
    (id::JSONPATH_2, JsonPathLaw::ReadSource),
];
// ---------------------------------------------------------------------------
// The RFC 9535 grammar and evaluator.
// ---------------------------------------------------------------------------

/// One parsed RFC 9535 query: the root (`$` for an absolute query, the current node `@` for a relative one) plus its
/// segments.
#[derive(Debug)]
pub struct Query {
    /// Whether the query starts at the ROOT (`$`) or the CURRENT node (`@`).
    /// The top-level query of `jsonpath(...)` is always rooted; the flag only matters for queries embedded in filter
    /// expressions.
    rooted: bool,
    segments: Vec<Segment>,
}

/// One segment: a child segment or a descendant segment, each holding the comma-separated selector list of its
/// bracketed/dot form.
#[derive(Debug)]
enum Segment {
    Child(Vec<Selector>),
    Descendant(Vec<Selector>),
}

/// One selector inside a segment.
#[derive(Debug)]
enum Selector {
    Name(String),
    Index(i64),
    Slice {
        start: Option<i64>,
        end: Option<i64>,
        step: Option<i64>,
    },
    Wildcard,
    Filter(LogicalExpr),
}

/// A filter logical expression (RFC §2.3.5.1). Parentheses are transparent; `!` is its own node so its scope is
/// explicit.
#[derive(Debug)]
enum LogicalExpr {
    Or(Vec<LogicalExpr>),
    And(Vec<LogicalExpr>),
    Not(Box<LogicalExpr>),
    Basic(BasicExpr),
}

/// One atomic filter expression: a parenthesized logical expression, a comparison, or a test (query existence /
/// function result).
#[derive(Debug)]
enum BasicExpr {
    Comparison(Comparable, ComparisonOp, Comparable),
    Test(TestKind),
}

/// What a test expression tests: the existence of a query's nodelist, or a `LogicalType`/`NodesType` function
/// expression's result.
#[derive(Debug)]
enum TestKind {
    Query(Query),
    Function(FunctionExpr),
}

/// One side of a comparison: a literal, a singular query, or a `ValueType` function expression. A non-singular query
/// here is ill-typed (RFC §2.4.3).
#[derive(Debug)]
enum Comparable {
    Literal(Value),
    Query(Query),
    Function(FunctionExpr),
}

/// The parse-time classification of one operand, before its role (comparison or test) is known.
#[derive(Debug)]
enum Primary {
    Literal(Value),
    Query(Query),
    Function(FunctionExpr),
}

/// One function-expression call (RFC §2.4).
#[derive(Debug)]
struct FunctionExpr {
    name: FunctionName,
    args: Vec<FunctionArg>,
}

/// The five standard function extensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FunctionName {
    Length,
    Count,
    Match,
    Search,
    Value,
}

/// The declared type of a function expression's result (RFC §2.4.1), which is what well-typedness checks read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FunctionType {
    Value,
    Logical,
    Nodes,
}

/// One function argument: a literal, a query, a logical expression, or a nested function expression (RFC §2.4.3).
#[derive(Debug)]
enum FunctionArg {
    Literal(Value),
    Query(Query),
    Function(FunctionExpr),
}

/// The six comparison operators.
#[derive(Clone, Copy, Debug)]
enum ComparisonOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Parses one query VALUE into its RFC 9535 AST.
///
/// A non-string query is the `jsonpath query must be a string` error. The `$` root is optional (a bare query is parsed
/// as if rooted); a query that is not well-formed and valid raises the invalid-query class.
pub fn parse_query(query: &Value, resources: &ResourceContext<'_>) -> Result<Query, EngineRunError> {
    let Value::String(text) = query.untagged() else {
        return Err(crate::semantics::path::raise(
            "jsonpath query must be a string",
            resources,
        ));
    };
    parse_query_text(text.as_str())
        .map_err(|detail| crate::semantics::path::raise(&alloc::format!("invalid jsonpath query: {detail}"), resources))
}

/// Parses one query STRING into its AST, with the failure detail as text.
fn parse_query_text(text: &str) -> Result<Query, String> {
    let mut parser = Parser { text, pos: 0 };
    if parser.peek() == Some('$') {
        parser.bump();
    }
    let segments = parser.parse_segments()?;
    if !parser.at_end() {
        // Whitespace is allowed BETWEEN tokens, never at the end of the query (the suite pins `$ ` as invalid); any
        // other leftover is a syntax error.
        let tail = &text[parser.pos..];
        if tail.chars().all(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r')) {
            return Err("trailing whitespace is not part of a query".to_owned());
        }
        return Err(format!(
            "unexpected character {:?} at offset {}",
            parser.peek(),
            parser.pos
        ));
    }
    Ok(Query { rooted: true, segments })
}

/// A byte cursor over the query string.
struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn at_end(&self) -> bool {
        self.pos >= self.text.len()
    }

    /// The RFC `S` production: zero or more of space, tab, LF, CR.
    fn skip_s(&mut self) {
        while let Some(ch) = self.peek() {
            if matches!(ch, ' ' | '\t' | '\n' | '\r') {
                self.bump();
            } else {
                break;
            }
        }
    }

    fn starts_with(&self, needle: &str) -> bool {
        self.text[self.pos..].starts_with(needle)
    }

    fn error(&self, detail: &str) -> String {
        format!("{detail} at offset {}", self.pos)
    }

    fn expect(&mut self, ch: char) -> Result<(), String> {
        if self.peek() == Some(ch) {
            self.bump();
            Ok(())
        } else {
            Err(self.error(&format!("expected {ch:?}")))
        }
    }

    /// `segments = *(S segment)`. Whitespace is consumed ONLY when a segment follows it, so the cursor is left at the
    /// first byte that is not part of a segment — the caller (a filter expression, or the query tail check) decides
    /// what that byte means.
    fn parse_segments(&mut self) -> Result<Vec<Segment>, String> {
        let mut segments = Vec::new();
        loop {
            // `before` is captured BEFORE the skip: when no segment follows the whitespace, the cursor rewinds past it,
            // so a caller (the query tail check, a filter expression) still sees the space.
            let before = self.pos;
            self.skip_s();
            match self.peek() {
                Some('.') => segments.push(self.parse_dot_segment()?),
                Some('[') => segments.push(self.parse_bracketed_segment()?),
                _ => {
                    self.pos = before;
                    break;
                }
            }
        }
        Ok(segments)
    }

    /// A `.`-led segment: `.name`/`.*` (child) or `..name`/`..*`/`..[…]`
    /// (descendant).
    fn parse_dot_segment(&mut self) -> Result<Segment, String> {
        self.expect('.')?;
        let descendant = if self.peek() == Some('.') {
            self.bump();
            true
        } else {
            false
        };
        let selectors = match self.peek() {
            Some('[') => self.parse_bracketed_selectors()?,
            Some('*') => {
                self.bump();
                vec![Selector::Wildcard]
            }
            Some(ch) if is_name_first(ch) => {
                let name = self.parse_member_name()?;
                vec![Selector::Name(name)]
            }
            _ => {
                return Err(self.error(if descendant {
                    "a descendant segment must be `..name`, `..*`, or `..[…]`"
                } else {
                    "a dot segment must be `.name` or `.*`"
                }));
            }
        };
        if descendant {
            Ok(Segment::Descendant(selectors))
        } else {
            Ok(Segment::Child(selectors))
        }
    }

    /// `bracketed-selection = "[" S selector *(S "," S selector) S "]"`.
    fn parse_bracketed_segment(&mut self) -> Result<Segment, String> {
        let selectors = self.parse_bracketed_selectors()?;
        Ok(Segment::Child(selectors))
    }

    fn parse_bracketed_selectors(&mut self) -> Result<Vec<Selector>, String> {
        self.expect('[')?;
        self.skip_s();
        let mut selectors = Vec::new();
        loop {
            self.skip_s();
            selectors.push(self.parse_selector()?);
            self.skip_s();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    return Ok(selectors);
                }
                _ => return Err(self.error("expected `,` or `]` after a selector")),
            }
        }
    }

    /// One selector: name, index, slice, wildcard, or filter. A `:` inside a non-quoted selector makes it a slice.
    fn parse_selector(&mut self) -> Result<Selector, String> {
        match self.peek() {
            Some('*') => {
                self.bump();
                Ok(Selector::Wildcard)
            }
            Some('\'' | '"') => {
                let name = self.parse_string()?;
                Ok(Selector::Name(name))
            }
            Some('?') => {
                self.bump();
                self.skip_s();
                let expr = self.parse_logical_expr()?;
                Ok(Selector::Filter(expr))
            }
            Some(ch) if ch.is_ascii_digit() || ch == '-' => {
                // A slice contains a top-level `:`; an index does not. A leading `:` (empty start) is also a slice.
                let is_slice = self.text[self.pos..]
                    .chars()
                    .take_while(|ch| *ch != ',' && *ch != ']')
                    .any(|ch| ch == ':');
                if is_slice {
                    self.parse_slice()
                } else {
                    let index = self.parse_int()?;
                    Ok(Selector::Index(index))
                }
            }
            Some(':') => self.parse_slice(),
            Some(ch) => Err(self.error(&format!("unexpected character {ch:?} in a selector"))),
            None => Err(self.error("unexpected end of query in a selector")),
        }
    }

    /// `int = "0" / (["-"] DIGIT1 *DIGIT)` with the I-JSON range check.
    fn parse_int(&mut self) -> Result<i64, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.bump();
        }
        while matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
            self.bump();
        }
        let spelling = &self.text[start..self.pos];
        if spelling.is_empty() || spelling == "-" {
            return Err(self.error("expected an integer"));
        }
        if spelling.len() > 1 && spelling.starts_with('0') {
            return Err(self.error("an integer may not have leading zeroes"));
        }
        if spelling.len() > 1 && spelling.starts_with("-0") {
            return Err(self.error("an integer may not have leading zeroes"));
        }
        let value: i64 = spelling.parse().map_err(|_| self.error("an integer is out of range"))?;
        if value.unsigned_abs() > I_JSON_BOUND as u64 {
            return Err(self.error("an index or slice bound is outside the I-JSON exact-integer range"));
        }
        Ok(value)
    }

    /// `slice-selector = [start S] ":" S [end S] [S ":" [S step]]`.
    fn parse_slice(&mut self) -> Result<Selector, String> {
        let start = if self.peek() == Some(':') {
            None
        } else {
            Some(self.parse_int()?)
        };
        self.skip_s();
        self.expect(':')?;
        self.skip_s();
        let end = if matches!(self.peek(), Some(':' | ',' | ']')) {
            None
        } else {
            Some(self.parse_int()?)
        };
        self.skip_s();
        let step = if self.peek() == Some(':') {
            self.bump();
            self.skip_s();
            if matches!(self.peek(), Some(',' | ']')) {
                None
            } else {
                Some(self.parse_int()?)
            }
        } else {
            None
        };
        Ok(Selector::Slice { start, end, step })
    }

    /// A member-name shorthand: `name-first *name-char` (RFC §2.5.1.1).
    fn parse_member_name(&mut self) -> Result<String, String> {
        let start = self.pos;
        let mut first = true;
        while let Some(ch) = self.peek() {
            let valid = if first {
                is_name_first(ch)
            } else {
                is_name_first(ch) || ch.is_ascii_digit()
            };
            if valid {
                self.bump();
                first = false;
            } else {
                break;
            }
        }
        if first {
            return Err(self.error("expected a member name"));
        }
        Ok(self.text[start..self.pos].to_owned())
    }

    /// A quoted string literal (RFC §2.3.1.1): JSON escapes in `""`, the analogous set with `'` in `'…'`, `\uXXXX`
    /// with surrogate pairing.
    fn parse_string(&mut self) -> Result<String, String> {
        let quote = self.bump().ok_or_else(|| self.error("expected a string"))?;
        let mut out = String::new();
        loop {
            let Some(ch) = self.bump() else {
                return Err(self.error("unterminated string literal"));
            };
            match ch {
                '\\' => {
                    let Some(escape) = self.bump() else {
                        return Err(self.error("unterminated string escape"));
                    };
                    match escape {
                        'b' => out.push('\u{0008}'),
                        'f' => out.push('\u{000C}'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        '/' => out.push('/'),
                        '\\' => out.push('\\'),
                        '"' if quote == '"' => out.push('"'),
                        '\'' if quote == '\'' => out.push('\''),
                        'u' => {
                            let code = self.parse_hex4()?;
                            // A high surrogate must be followed by `\u` and a low surrogate; anything else is invalid.
                            if (0xD800..=0xDBFF).contains(&code) {
                                if !self.starts_with("\\u") {
                                    return Err(self.error("a lone high surrogate in a string escape"));
                                }
                                self.pos += 2;
                                let low = self.parse_hex4()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err(self.error("a high surrogate must be followed by a low surrogate"));
                                }
                                let scalar = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                // SAFETY: the surrogate math above yields a
                                // valid scalar (U+10000..U+10FFFF).
                                out.push(char::from_u32(scalar).expect("valid scalar"));
                            } else if (0xDC00..=0xDFFF).contains(&code) {
                                return Err(self.error("a lone low surrogate in a string escape"));
                            } else {
                                out.push(char::from_u32(code).expect("valid scalar"));
                            }
                        }
                        _ => {
                            return Err(self.error(&format!("invalid escape sequence `\\{escape}`")));
                        }
                    }
                }
                ch if ch == quote => return Ok(out),
                ch if (ch as u32) < 0x20 => {
                    // RFC §2.3.1.1: a control character must be ESCAPED (`\uXXXX` or one of the named escapes), never
                    // raw.
                    return Err(self.error("a raw control character in a string must be escaped"));
                }
                _ => out.push(ch),
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let Some(ch) = self.bump() else {
                return Err(self.error("a `\\u` escape needs four hex digits"));
            };
            let digit = ch
                .to_digit(16)
                .ok_or_else(|| self.error(&format!("a `\\u` escape needs hex digits, got {ch:?}")))?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// `logical-expr = logical-or-expr` with `||`/`&&`/`!`/parens.
    fn parse_logical_expr(&mut self) -> Result<LogicalExpr, String> {
        let mut terms = vec![self.parse_logical_and()?];
        loop {
            self.skip_s();
            if self.starts_with("||") {
                self.pos += 2;
                terms.push(self.parse_logical_and()?);
            } else {
                break;
            }
        }
        if terms.len() == 1 {
            Ok(terms.pop().expect("one term"))
        } else {
            Ok(LogicalExpr::Or(terms))
        }
    }

    fn parse_logical_and(&mut self) -> Result<LogicalExpr, String> {
        let mut terms = vec![self.parse_basic()?];
        loop {
            self.skip_s();
            if self.starts_with("&&") {
                self.pos += 2;
                terms.push(self.parse_basic()?);
            } else {
                break;
            }
        }
        if terms.len() == 1 {
            Ok(terms.pop().expect("one term"))
        } else {
            Ok(LogicalExpr::And(terms))
        }
    }

    /// `basic-expr = paren-expr / comparison-expr / test-expr`, with the optional `!` of the paren/test forms handled
    /// here.
    fn parse_basic(&mut self) -> Result<LogicalExpr, String> {
        self.skip_s();
        if self.peek() == Some('!') {
            self.bump();
            self.skip_s();
            if self.peek() == Some('(') {
                let inner = self.parse_paren()?;
                return Ok(LogicalExpr::Not(Box::new(inner)));
            }
            // After `!` only a query or a function expression may follow; a literal is not a valid test.
            let primary = self.parse_primary()?;
            let kind = match primary {
                Primary::Query(query) => TestKind::Query(query),
                Primary::Function(function) => {
                    if function.name.result_type() == FunctionType::Value {
                        return Err(self.error(&format!(
                            "{} returns a value, which cannot be used as a test \
                             expression (compare it instead)",
                            function.name.spelling()
                        )));
                    }
                    TestKind::Function(function)
                }
                Primary::Literal(_) => {
                    return Err(self.error("`!` must precede a query or a function expression"));
                }
            };
            return Ok(LogicalExpr::Not(Box::new(LogicalExpr::Basic(BasicExpr::Test(kind)))));
        }
        if self.peek() == Some('(') {
            return self.parse_paren();
        }
        // A primary (literal / query / function), then either a comparison operator (comparison-expr) or a bare
        // query/function test (test-expr).
        let primary = self.parse_primary()?;
        self.skip_s();
        if let Some(op) = self.peek_comparison_op() {
            self.consume_comparison_op(op);
            self.skip_s();
            let right = self.parse_primary()?;
            let left = Self::comparable_from_primary(primary)?;
            let right = Self::comparable_from_primary(right)?;
            return Ok(LogicalExpr::Basic(BasicExpr::Comparison(left, op, right)));
        }
        match primary {
            Primary::Query(query) => Ok(LogicalExpr::Basic(BasicExpr::Test(TestKind::Query(query)))),
            Primary::Function(function) => {
                // A function in a TEST position must declare a Logical or Nodes result (RFC §2.4.3 rule 1); a
                // ValueType function there — `length(@.a)` alone — is ill-typed.
                if function.name.result_type() == FunctionType::Value {
                    return Err(self.error(&format!(
                        "{} returns a value, which cannot be used as a test \
                         expression (compare it instead)",
                        function.name.spelling()
                    )));
                }
                Ok(LogicalExpr::Basic(BasicExpr::Test(TestKind::Function(function))))
            }
            Primary::Literal(_) => {
                Err(self.error("a literal on its own is not a filter expression (comparisons need an operator)"))
            }
        }
    }

    fn parse_paren(&mut self) -> Result<LogicalExpr, String> {
        self.expect('(')?;
        self.skip_s();
        let inner = self.parse_logical_expr()?;
        self.skip_s();
        self.expect(')')?;
        Ok(inner)
    }

    /// One comparison-op token, if present at the cursor.
    fn peek_comparison_op(&self) -> Option<ComparisonOp> {
        let rest = &self.text[self.pos..];
        if rest.starts_with("==") {
            Some(ComparisonOp::Eq)
        } else if rest.starts_with("!=") {
            Some(ComparisonOp::Ne)
        } else if rest.starts_with("<=") {
            Some(ComparisonOp::Le)
        } else if rest.starts_with(">=") {
            Some(ComparisonOp::Ge)
        } else if rest.starts_with('<') {
            Some(ComparisonOp::Lt)
        } else if rest.starts_with('>') {
            Some(ComparisonOp::Gt)
        } else {
            None
        }
    }

    fn consume_comparison_op(&mut self, op: ComparisonOp) {
        let width = match op {
            ComparisonOp::Eq | ComparisonOp::Ne | ComparisonOp::Le | ComparisonOp::Ge => 2,
            ComparisonOp::Lt | ComparisonOp::Gt => 1,
        };
        self.pos += width;
    }

    /// Parses one operand of a comparison or test: a literal, a query, or a function expression. The operand's role is
    /// decided by the caller.
    fn parse_primary(&mut self) -> Result<Primary, String> {
        self.skip_s();
        let ch = self.peek().ok_or_else(|| self.error("expected an expression"))?;
        match ch {
            '@' | '$' => {
                let rooted = self.peek() == Some('$');
                self.bump();
                let segments = self.parse_segments()?;
                Ok(Primary::Query(Query { rooted, segments }))
            }
            '\'' | '"' => {
                let value = self.parse_string()?;
                let value = Self::string_value(&value)?;
                Ok(Primary::Literal(value))
            }
            ch if ch.is_ascii_digit() || ch == '-' => {
                let value = self.parse_number()?;
                Ok(Primary::Literal(value))
            }
            ch if ch.is_ascii_lowercase() => {
                let name = self.parse_name();
                match name.as_str() {
                    "true" => Ok(Primary::Literal(Value::Bool(true))),
                    "false" => Ok(Primary::Literal(Value::Bool(false))),
                    "null" => Ok(Primary::Literal(Value::Null)),
                    _ => {
                        // A bare identifier is only valid as a function call.
                        if self.peek() != Some('(') {
                            return Err(
                                self.error(&format!("a bare identifier `{name}` is not a valid filter expression"))
                            );
                        }
                        self.bump();
                        let function = self.parse_function_args(&name)?;
                        Ok(Primary::Function(function))
                    }
                }
            }
            other => Err(self.error(&format!("unexpected character {other:?} in an expression"))),
        }
    }

    /// Builds the comparable role of an operand, enforcing well-typedness (RFC §2.4.3): a query must be SINGULAR (only
    /// name/index segments) and a function must declare a `ValueType` result.
    fn comparable_from_primary(primary: Primary) -> Result<Comparable, String> {
        match primary {
            Primary::Literal(value) => Ok(Comparable::Literal(value)),
            Primary::Query(query) => {
                if !is_singular(&query) {
                    return Err(
                        "a comparison operand must be a singular query (only name and index segments)".to_owned(),
                    );
                }
                Ok(Comparable::Query(query))
            }
            Primary::Function(function) => {
                if function.name.result_type() != FunctionType::Value {
                    return Err(format!(
                        "{} returns a non-value type and cannot be used in a comparison",
                        function.name.spelling()
                    ));
                }
                Ok(Comparable::Function(function))
            }
        }
    }

    /// `number = (int / "-0") [frac] [exp]`, parsed through jqf's own number literal law.
    fn parse_number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some('-') {
            self.bump();
        }
        while matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            let digits_start = self.pos;
            while matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
                self.bump();
            }
            if self.pos == digits_start {
                return Err(self.error("a number fraction needs digits"));
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            let digits_start = self.pos;
            while matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
                self.bump();
            }
            if self.pos == digits_start {
                return Err(self.error("a number exponent needs digits"));
            }
        }
        let spelling = &self.text[start..self.pos];
        if !valid_number_spelling(spelling) {
            return Err(self.error(&format!("invalid number literal `{spelling}`")));
        }
        let number = Number::try_json_literal(spelling).map_err(|_| self.error("invalid number literal"))?;
        Ok(Value::Number(number))
    }

    /// `function-name = LCALPHA *(LCALPHA / "_" / DIGIT)`.
    fn parse_name(&mut self) -> String {
        let start = self.pos;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_lowercase() || ch == '_' || ch.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        self.text[start..self.pos].to_owned()
    }

    /// Parses the argument list after the already-consumed `(`.
    fn parse_function_args(&mut self, name: &str) -> Result<FunctionExpr, String> {
        let name = FunctionName::parse(name)
            .ok_or_else(|| self.error(&format!("unknown JSONPath function extension `{name}`")))?;
        self.skip_s();
        let mut args = Vec::new();
        if self.peek() == Some(')') {
            self.bump();
        } else {
            loop {
                args.push(self.parse_function_arg()?);
                self.skip_s();
                match self.peek() {
                    Some(',') => {
                        self.bump();
                        self.skip_s();
                    }
                    Some(')') => {
                        self.bump();
                        break;
                    }
                    _ => return Err(self.error("expected `,` or `)` in a function call")),
                }
            }
        }
        // Arity is exact (RFC §2.4.3: too few or too many params is an ill-formed call).
        let expected = name.param_count();
        if args.len() != expected {
            return Err(format!(
                "{}() takes exactly {expected} argument(s), got {}",
                name.spelling(),
                args.len()
            ));
        }
        for (index, arg) in args.iter().enumerate() {
            let param = name.param_type(index);
            if !arg.well_typed_for(param) {
                return Err(format!(
                    "{}() argument {} is not well-typed for a {} parameter",
                    name.spelling(),
                    index + 1,
                    match param {
                        FunctionType::Value => "value",
                        FunctionType::Logical => "logical",
                        FunctionType::Nodes => "nodelist",
                    }
                ));
            }
        }
        Ok(FunctionExpr { name, args })
    }

    /// `function-argument = literal / filter-query / logical-expr / function-expr`.
    fn parse_function_arg(&mut self) -> Result<FunctionArg, String> {
        self.skip_s();
        let ch = self.peek().ok_or_else(|| self.error("expected a function argument"))?;
        match ch {
            '@' | '$' => {
                let primary = self.parse_primary()?;
                match primary {
                    Primary::Query(query) => Ok(FunctionArg::Query(query)),
                    Primary::Function(function) => Ok(FunctionArg::Function(function)),
                    Primary::Literal(value) => Ok(FunctionArg::Literal(value)),
                }
            }
            '\'' | '"' => {
                let value = self.parse_string()?;
                Ok(FunctionArg::Literal(Self::string_value(&value)?))
            }
            ch if ch.is_ascii_digit() || ch == '-' => {
                let value = self.parse_number()?;
                Ok(FunctionArg::Literal(value))
            }
            ch if ch.is_ascii_lowercase() => {
                let name = self.parse_name();
                match name.as_str() {
                    "true" => Ok(FunctionArg::Literal(Value::Bool(true))),
                    "false" => Ok(FunctionArg::Literal(Value::Bool(false))),
                    "null" => Ok(FunctionArg::Literal(Value::Null)),
                    _ if self.peek() == Some('(') => {
                        self.bump();
                        let function = self.parse_function_args(&name)?;
                        Ok(FunctionArg::Function(function))
                    }
                    // An identifier that is not a function call can only be a logical expression — but a bare
                    // identifier is not one.
                    _ => Err(self.error(&format!("a bare identifier `{name}` is not a valid function argument"))),
                }
            }
            '(' => {
                // A parenthesized logical expression is a valid argument only for a LogicalType parameter, and none of
                // the five standard functions has one — such an argument is always ill-typed.
                Err(self.error(
                    "a logical expression is not a valid argument for any of the five \
                     standard function extensions",
                ))
            }
            other => Err(self.error(&format!("unexpected character {other:?} in a function argument"))),
        }
    }

    fn string_value(text: &str) -> Result<Value, String> {
        Value::try_string(text).map_err(|_| "string allocation failed".to_owned())
    }
}

impl FunctionName {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "length" => Some(Self::Length),
            "count" => Some(Self::Count),
            "match" => Some(Self::Match),
            "search" => Some(Self::Search),
            "value" => Some(Self::Value),
            _ => None,
        }
    }

    fn spelling(self) -> &'static str {
        match self {
            Self::Length => "length",
            Self::Count => "count",
            Self::Match => "match",
            Self::Search => "search",
            Self::Value => "value",
        }
    }

    fn result_type(self) -> FunctionType {
        match self {
            Self::Length | Self::Count | Self::Value => FunctionType::Value,
            Self::Match | Self::Search => FunctionType::Logical,
        }
    }

    fn param_count(self) -> usize {
        match self {
            Self::Length | Self::Count | Self::Value => 1,
            Self::Match | Self::Search => 2,
        }
    }

    /// Every parameter of the five standard functions is either a Value or a Nodes; the exact-arity check guarantees
    /// `index` is in range.
    fn param_type(self, _index: usize) -> FunctionType {
        match self {
            Self::Length | Self::Match | Self::Search => FunctionType::Value,
            Self::Count | Self::Value => FunctionType::Nodes,
        }
    }
}

impl FunctionArg {
    /// Whether this argument shape satisfies a parameter of the given type (RFC §2.4.3 rules 2a–2d, restricted to
    /// the shapes the five standard functions can receive).
    fn well_typed_for(&self, param: FunctionType) -> bool {
        match self {
            FunctionArg::Literal(_) => param == FunctionType::Value,
            FunctionArg::Query(query) => match param {
                FunctionType::Nodes => true,
                FunctionType::Value => is_singular(query),
                FunctionType::Logical => false,
            },
            FunctionArg::Function(function) => match param {
                FunctionType::Value => function.name.result_type() == FunctionType::Value,
                FunctionType::Nodes => function.name.result_type() == FunctionType::Nodes,
                FunctionType::Logical => true,
            },
        }
    }
}

/// Whether a query is SINGULAR (RFC §2.3.5.1): only name and index segments, so it always yields at most one node.
fn is_singular(query: &Query) -> bool {
    query.segments.iter().all(|segment| match segment {
        Segment::Child(selectors) => {
            selectors.len() == 1 && matches!(selectors[0], Selector::Name(_) | Selector::Index(_))
        }
        Segment::Descendant(_) => false,
    })
}

/// `name-first = ALPHA / "_" / %x80-D7FF / %xE000-10FFFF`. Rust `char` is never a surrogate, so every non-ASCII char
/// qualifies.
fn is_name_first(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || !ch.is_ascii()
}

/// Whether a spelling conforms to the RFC's number grammar — `number = (int / "-0") [frac] [exp]` with `int = "0" /
/// (["-"] DIGIT1 *DIGIT)`. This is STRICTER than the reference's own literal acceptance: no leading zeroes (`00`, `01`)
/// and no bare-fraction forms (`-.1`), exactly as the suite pins.
fn valid_number_spelling(spelling: &str) -> bool {
    let bytes = spelling.as_bytes();
    let mut index = 0;
    if bytes.first() == Some(&b'-') {
        index += 1;
    }
    let int_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let int_text = &spelling[int_start..index];
    if int_text.is_empty() || (int_text.len() > 1 && int_text.starts_with('0')) {
        return false;
    }
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        let frac_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == frac_start {
            return false;
        }
    }
    if index < bytes.len() && matches!(bytes[index], b'e' | b'E') {
        index += 1;
        if index < bytes.len() && matches!(bytes[index], b'+' | b'-') {
            index += 1;
        }
        let exp_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == exp_start {
            return false;
        }
    }
    index == bytes.len()
}
// ---------------------------------------------------------------------------
// Evaluation.
// --------------------------------------------------------------------------- Evaluation.
// ---------------------------------------------------------------------------

/// The result of evaluating one comparable side of a comparison: a value, or Nothing (an empty nodelist / a Nothing
/// function result).
enum CompValue {
    Nothing,
    Value(Value),
}

/// Evaluates a parsed query over an owned value, returning the nodelist as an array of the matched values (a `[match,
/// …]` array, `[]` for no match).
pub fn match_query(query: &Query, input: &Value, resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
    // A top-level query is always rooted, so current = root = input.
    let mut matched: Vec<&Value> = Vec::new();
    eval_query(query, input, input, resources, &mut matched)?;
    let mut values: Vec<Value> = Vec::new();
    values
        .try_reserve_exact(matched.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for node in matched {
        values.push(node.clone());
    }
    Array::try_from_vec(values)
        .map(Value::Array)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// Applies one child segment's selector list to one node, in selector order (RFC §2.5.1.2: the per-input-node nodelist
/// is the concatenation of the selector nodelists).
fn apply_child<'v>(
    selectors: &[Selector],
    node: &'v Value,
    root: &'v Value,
    resources: &ResourceContext<'_>,
    out: &mut Vec<&'v Value>,
) -> Result<(), EngineRunError> {
    for selector in selectors {
        match selector {
            Selector::Name(name) => {
                if let Value::Object(object) = node.untagged()
                    && let Some(child) = object.get(name)
                {
                    out.push(child);
                }
            }
            Selector::Index(index) => {
                if let Value::Array(array) = node.untagged()
                    && let Some(child) = array_index(array, *index)
                {
                    out.push(child);
                }
            }
            Selector::Wildcard => match node.untagged() {
                Value::Array(array) => out.extend(array.iter()),
                Value::Object(object) => {
                    for entry in object {
                        out.push(entry.value());
                    }
                }
                _ => {}
            },
            Selector::Slice { start, end, step } => {
                if let Value::Array(array) = node.untagged() {
                    slice_select(array, *start, *end, *step, out);
                }
            }
            Selector::Filter(expr) => match node.untagged() {
                Value::Array(array) => {
                    for element in array {
                        if eval_logical(expr, element, root, resources)? {
                            out.push(element);
                        }
                    }
                }
                Value::Object(object) => {
                    for entry in object {
                        if eval_logical(expr, entry.value(), root, resources)? {
                            out.push(entry.value());
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(())
}

/// Applies one descendant segment: a PRE-ORDER visit of the input node and every descendant, applying the child segment
/// to each visited node in visit order (RFC §2.5.2.2). Iterative, so a deep document cannot overflow the request stack
/// (the codec's own nesting guard already bounds the depth).
fn apply_descendant<'v>(
    selectors: &[Selector],
    node: &'v Value,
    root: &'v Value,
    resources: &ResourceContext<'_>,
    out: &mut Vec<&'v Value>,
) -> Result<(), EngineRunError> {
    let mut stack: Vec<&Value> = vec![node];
    while let Some(current) = stack.pop() {
        apply_child(selectors, current, root, resources, out)?;
        match current.untagged() {
            Value::Array(array) => {
                for element in array.iter().rev() {
                    stack.push(element);
                }
            }
            Value::Object(object) => {
                for entry in object.iter().rev() {
                    stack.push(entry.value());
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// The array-index law: a non-negative index from zero, a negative index from the end (`len + index`), nothing out of
/// range.
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the length cast is the RFC's len-relative math, and the `as usize` is               guarded by the 0 <= normalized < len check directly above it"
)]
fn array_index(array: &Array, index: i64) -> Option<&Value> {
    let len = array.len() as i64;
    let normalized = if index < 0 { len + index } else { index };
    if normalized < 0 || normalized >= len {
        return None;
    }
    array.get(normalized as usize)
}

/// The RFC §2.3.4.2.2 slice law, exactly as spelled: normalize start/end against the length, clamp to the direction's
/// bounds, then iterate.
#[allow(clippy::too_many_lines, reason = "the slice law is two clamped loops")]
#[expect(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "every `as usize` index is produced by the clamped lower/upper bounds and the               loop guard, so it is in range by construction"
)]
fn slice_select<'v>(
    array: &'v Array,
    start: Option<i64>,
    end: Option<i64>,
    step: Option<i64>,
    out: &mut Vec<&'v Value>,
) {
    let len = array.len() as i64;
    let step = step.unwrap_or(1);
    if step == 0 {
        return;
    }
    let start = start.unwrap_or(if step > 0 { 0 } else { len - 1 });
    let end = end.unwrap_or(if step > 0 { len } else { -len - 1 });
    let n_start = if start >= 0 { start } else { len + start };
    let n_end = if end >= 0 { end } else { len + end };
    let (lower, upper) = if step > 0 {
        (n_start.clamp(0, len), n_end.clamp(0, len))
    } else {
        (n_end.clamp(-1, len - 1), n_start.clamp(-1, len - 1))
    };
    if step > 0 {
        let mut index = lower;
        while index < upper {
            if let Some(value) = array.get(index as usize) {
                out.push(value);
            }
            index += step;
        }
    } else {
        let mut index = upper;
        while lower < index {
            if let Some(value) = array.get(index as usize) {
                out.push(value);
            }
            index += step;
        }
    }
}

/// Evaluates one filter logical expression over the current node, with `$` bound to the query argument.
fn eval_logical(
    expr: &LogicalExpr,
    current: &Value,
    root: &Value,
    resources: &ResourceContext<'_>,
) -> Result<bool, EngineRunError> {
    match expr {
        LogicalExpr::Or(terms) => {
            for term in terms {
                if eval_logical(term, current, root, resources)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        LogicalExpr::And(terms) => {
            for term in terms {
                if !eval_logical(term, current, root, resources)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        LogicalExpr::Not(inner) => Ok(!eval_logical(inner, current, root, resources)?),
        LogicalExpr::Basic(BasicExpr::Comparison(left, op, right)) => {
            let left = eval_comparable(left, current, root, resources)?;
            let right = eval_comparable(right, current, root, resources)?;
            compare_values(&left, &right, *op, resources)
        }
        LogicalExpr::Basic(BasicExpr::Test(kind)) => {
            let truth = match kind {
                TestKind::Query(query) => {
                    let mut nodes: Vec<&Value> = Vec::new();
                    eval_query(query, current, root, resources, &mut nodes)?;
                    !nodes.is_empty()
                }
                TestKind::Function(function) => eval_function_truth(function, current, root, resources)?,
            };
            Ok(truth)
        }
    }
}

/// Evaluates a query inside a filter: `@` starts at the current node, `$` at the root. The query's own segments are the
/// same evaluator the top level uses.
fn eval_query<'v>(
    query: &Query,
    current: &'v Value,
    root: &'v Value,
    resources: &ResourceContext<'_>,
    out: &mut Vec<&'v Value>,
) -> Result<(), EngineRunError> {
    let start = if query.rooted { root } else { current };
    let mut nodes: Vec<&Value> = vec![start];
    for segment in &query.segments {
        let mut next: Vec<&Value> = Vec::new();
        for node in nodes.drain(..) {
            match segment {
                Segment::Child(selectors) => {
                    apply_child(selectors, node, root, resources, &mut next)?;
                }
                Segment::Descendant(selectors) => {
                    apply_descendant(selectors, node, root, resources, &mut next)?;
                }
            }
        }
        nodes = next;
    }
    out.extend(nodes);
    Ok(())
}

/// Evaluates one comparable: a literal is its value, a singular query its single node's value (or Nothing when empty),
/// a `ValueType` function its result.
fn eval_comparable(
    comparable: &Comparable,
    current: &Value,
    root: &Value,
    resources: &ResourceContext<'_>,
) -> Result<CompValue, EngineRunError> {
    match comparable {
        Comparable::Literal(value) => Ok(CompValue::Value(value.clone())),
        Comparable::Query(query) => {
            let mut nodes: Vec<&Value> = Vec::new();
            eval_query(query, current, root, resources, &mut nodes)?;
            // A singular query yields at most one node by construction, so the only other arity is zero: Nothing.
            match nodes.len() {
                1 => Ok(CompValue::Value(nodes.pop().expect("one node").clone())),
                _ => Ok(CompValue::Nothing),
            }
        }
        Comparable::Function(function) => {
            let result = eval_function(function, current, root, resources)?;
            match result {
                FunctionResult::Value(value) => Ok(value),
                FunctionResult::Logical(_) => Err(EngineRunError::internal_contract(
                    "a ValueType comparable evaluated a logical function",
                )),
            }
        }
    }
}

/// The RFC §2.3.5.2.2 comparison law, directly: `==` is deep equality with Nothing == Nothing true; `<` orders only
/// number-number and string-string pairs; `!=`/`<=`/`>`/`>=` derive from them.
fn compare_values(
    left: &CompValue,
    right: &CompValue,
    op: ComparisonOp,
    resources: &ResourceContext<'_>,
) -> Result<bool, EngineRunError> {
    let eq = match (left, right) {
        (CompValue::Nothing, CompValue::Nothing) => true,
        (CompValue::Nothing, _) | (_, CompValue::Nothing) => false,
        (CompValue::Value(a), CompValue::Value(b)) => crate::semantics::order::semantic_eq(a, b)
            .map_err(|_| crate::semantics::path::raise("Equality check too deep", resources))?,
    };
    let lt = |left: &CompValue, right: &CompValue| -> Result<bool, EngineRunError> {
        match (left, right) {
            (CompValue::Nothing, _) | (_, CompValue::Nothing) => Ok(false),
            (CompValue::Value(a), CompValue::Value(b)) => {
                let (a, b) = (a.untagged(), b.untagged());
                match (a, b) {
                    (Value::Number(_), Value::Number(_)) | (Value::String(_), Value::String(_)) => {
                        Ok(crate::semantics::order::observable_cmp(a, b)
                            .map_err(|_| crate::semantics::path::raise("Comparison too deep", resources))?
                            == core::cmp::Ordering::Less)
                    }
                    _ => Ok(false),
                }
            }
        }
    };
    // a > b  ⇔  b < a, and a >= b  ⇔  b < a or a == b.
    let swapped = |left: &CompValue, right: &CompValue| lt(right, left);
    Ok(match op {
        ComparisonOp::Eq => eq,
        ComparisonOp::Ne => !eq,
        ComparisonOp::Lt => lt(left, right)?,
        ComparisonOp::Le => lt(left, right)? || eq,
        ComparisonOp::Gt => swapped(left, right)?,
        ComparisonOp::Ge => swapped(left, right)? || eq,
    })
}

/// One function-expression evaluation result, classified by declared type.
enum FunctionResult {
    Value(CompValue),
    Logical(bool),
}

/// Evaluates a function expression in a TEST position: its `LogicalType` result, or (`NodesType` result) whether its
/// nodelist is non-empty. A `ValueType` function in a test position is ill-typed and was rejected at parse.
fn eval_function_truth(
    function: &FunctionExpr,
    current: &Value,
    root: &Value,
    resources: &ResourceContext<'_>,
) -> Result<bool, EngineRunError> {
    match eval_function(function, current, root, resources)? {
        FunctionResult::Logical(truth) => Ok(truth),
        FunctionResult::Value(_) => Err(EngineRunError::internal_contract(
            "a ValueType function reached a test position",
        )),
    }
}

/// Translates an RFC 9485 I-Regexp pattern onto the engine's regex tier:
/// RFC 9485 §5.3 converts an unescaped `.` (outside a character class) to `[^\n\r]`, where the `regex` crate's own `.`
/// excludes only `\n`. The suite pins the difference: `match(@, '.')` selects `"\u2028"` and `"\u2029"` but not `"\r"`.
fn iregexp_dot_rewrite(pattern: &str) -> alloc::string::String {
    // The class tracker is deliberately NAIVE: it flips on any `[` and off on any `]`, so a `[` inside a class (POSIX
    // spellings such as `[[:digit:].]`, or a `]` first in a class) desynchronizes it. That is acceptable by
    // construction: I-Regexp (RFC 9485) excludes POSIX classes and the first-position-`]` literal, so such a pattern
    // fails to compile downstream and the match functions answer LogicalFalse — the sanctioned outcome for an
    // uncompilable pattern. A dot misjudged as in-class would only widen the rewrite, never corrupt a compiling
    // pattern.
    let mut out = alloc::string::String::new();
    let mut in_class = false;
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                out.push(ch);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '[' => {
                in_class = true;
                out.push(ch);
            }
            ']' => {
                in_class = false;
                out.push(ch);
            }
            '.' if !in_class => out.push_str("[^\\n\\r]"),
            other => out.push(other),
        }
    }
    out
}

/// Evaluates one function expression per its RFC §2.4 definition.
fn eval_function(
    function: &FunctionExpr,
    current: &Value,
    root: &Value,
    resources: &ResourceContext<'_>,
) -> Result<FunctionResult, EngineRunError> {
    match function.name {
        FunctionName::Length => {
            let arg = eval_value_arg(&function.args[0], current, root, resources)?;
            let value = match arg {
                CompValue::Nothing => CompValue::Nothing,
                CompValue::Value(value) => {
                    let length = match value.untagged() {
                        Value::String(text) => Some(to_length(text.as_str().chars().count())),
                        Value::Array(array) => Some(to_length(array.len())),
                        Value::Object(object) => Some(to_length(object.len())),
                        _ => None,
                    };
                    match length {
                        Some(length) => CompValue::Value(integer_value(length)),
                        None => CompValue::Nothing,
                    }
                }
            };
            Ok(FunctionResult::Value(value))
        }
        FunctionName::Count => {
            let nodes = eval_nodes_arg(&function.args[0], current, root, resources)?;
            Ok(FunctionResult::Value(CompValue::Value(integer_value(to_length(
                nodes.len(),
            )))))
        }
        FunctionName::Value => {
            let nodes = eval_nodes_arg(&function.args[0], current, root, resources)?;
            let value = match nodes.len() {
                1 => CompValue::Value(nodes[0].clone()),
                _ => CompValue::Nothing,
            };
            Ok(FunctionResult::Value(value))
        }
        FunctionName::Match | FunctionName::Search => {
            let subject = eval_value_arg(&function.args[0], current, root, resources)?;
            let pattern = eval_value_arg(&function.args[1], current, root, resources)?;
            let CompValue::Value(subject) = subject else {
                return Ok(FunctionResult::Logical(false));
            };
            let CompValue::Value(pattern) = pattern else {
                return Ok(FunctionResult::Logical(false));
            };
            let (Value::String(subject), Value::String(pattern)) = (subject.untagged(), pattern.untagged()) else {
                return Ok(FunctionResult::Logical(false));
            };
            // `match()` anchors the whole string; `search()` finds a substring. A pattern the engine's regex tier
            // cannot compile is LogicalFalse (RFC §2.4.6/2.4.7: a non-conforming pattern is not an error).
            let pattern = iregexp_dot_rewrite(pattern.as_str());
            let pattern = if function.name == FunctionName::Match {
                alloc::format!(r"\A(?:{pattern})\z")
            } else {
                pattern
            };
            let Ok(regex) = regex_builtins::compile_plain_regex(&pattern) else {
                return Ok(FunctionResult::Logical(false));
            };
            Ok(FunctionResult::Logical(regex.is_match(subject.as_str())))
        }
    }
}

/// Evaluates a `ValueType` function argument to a comparable value (RFC §2.4.3 2d: a literal, a singular query's
/// single node, or Nothing).
fn eval_value_arg(
    arg: &FunctionArg,
    current: &Value,
    root: &Value,
    resources: &ResourceContext<'_>,
) -> Result<CompValue, EngineRunError> {
    match arg {
        FunctionArg::Literal(value) => Ok(CompValue::Value(value.clone())),
        FunctionArg::Query(query) => {
            let mut nodes: Vec<&Value> = Vec::new();
            eval_query(query, current, root, resources, &mut nodes)?;
            // The arity-1 arm is the only value the grammar admits; zero is Nothing and any other arity is unreachable
            // for a singular query.
            match nodes.len() {
                1 => Ok(CompValue::Value(nodes.pop().expect("one node").clone())),
                _ => Ok(CompValue::Nothing),
            }
        }
        FunctionArg::Function(function) => {
            let result = eval_function(function, current, root, resources)?;
            match result {
                FunctionResult::Value(value) => Ok(value),
                FunctionResult::Logical(_) => Err(EngineRunError::internal_contract(
                    "a ValueType parameter evaluated a logical argument",
                )),
            }
        }
    }
}

/// Evaluates a `NodesType` function argument to its nodelist (RFC §2.4.3 2c).
fn eval_nodes_arg<'v>(
    arg: &'v FunctionArg,
    current: &'v Value,
    root: &'v Value,
    resources: &ResourceContext<'_>,
) -> Result<Vec<&'v Value>, EngineRunError> {
    match arg {
        FunctionArg::Query(query) => {
            let mut nodes: Vec<&Value> = Vec::new();
            eval_query(query, current, root, resources, &mut nodes)?;
            Ok(nodes)
        }
        FunctionArg::Function(_) => Err(EngineRunError::internal_contract(
            "a NodesType parameter received a function argument (no standard function returns NodesType)",
        )),
        FunctionArg::Literal(_) => Err(EngineRunError::internal_contract(
            "a NodesType parameter received a non-query argument",
        )),
    }
}

/// One unsigned `JSONPath` count as a jqf integer value.
fn integer_value(value: i64) -> Value {
    Value::Number(Number::integer(Integer::from_i64(value)))
}

/// A container/string length as the `JSONPath` count: no live container can reach `i64::MAX` elements (the allocation
/// would fail first), so the wrap lint is a formality the reason documents.
#[expect(
    clippy::cast_possible_wrap,
    reason = "a length that wraps i64 implies an allocation that already failed"
)]
fn to_length(length: usize) -> i64 {
    length as i64
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

    fn number(text: &str) -> Value {
        Value::Number(Number::try_json_literal(text).expect("number"))
    }

    fn array(items: Vec<Value>) -> Value {
        let _resources = resources();
        Value::Array(Array::try_from_vec(items).expect("array"))
    }

    fn object(entries: &[(&str, Value)]) -> Value {
        let _resources = resources();
        let mut builder = ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
        for &(key, ref value) in entries {
            builder
                .try_insert_last(ObjectKey::try_from_str(key).expect("key"), value.clone())
                .expect("insert");
        }
        Value::Object(builder.try_finish().expect("object"))
    }

    /// The RFC 9535 §1.5 bookstore fixture.
    fn bookstore() -> Value {
        let book = |category: &str, title: &str, price: &str| {
            object(&[
                ("category", string(category)),
                ("title", string(title)),
                ("price", number(price)),
            ])
        };
        object(&[(
            "store",
            object(&[(
                "book",
                array(vec![
                    book("reference", "Sayings of the Century", "8.95"),
                    book("fiction", "Sword of Honour", "12.99"),
                    book("fiction", "Moby Dick", "8.99"),
                ]),
            )]),
        )])
    }

    fn render(value: &Value) -> String {
        let mut line = String::new();
        crate::semantics::render::write_value(&mut line, value).expect("render");
        line
    }

    fn run(query_text: &str, input: &Value) -> String {
        let resources = resources();
        let query = parse_query_text(query_text).expect("query parses");
        let result = match_query(&query, input, &resources).expect("query evaluates");
        render(&result)
    }

    fn rejected(query_text: &str) -> String {
        parse_query_text(query_text).expect_err("query rejected")
    }

    #[test]
    fn root_and_child_selectors() {
        let input = object(&[("a", number("1")), ("b", string("x"))]);
        assert_eq!(run("$", &input), "[{\"a\":1,\"b\":\"x\"}]");
        assert_eq!(run("$.a", &input), "[1]");
        assert_eq!(run("$['b']", &input), "[\"x\"]");
        assert_eq!(run(".a", &input), "[1]", "the bare-query form equals the $-form");
        assert_eq!(run("$.missing", &input), "[]");
    }

    #[test]
    fn wildcard_and_negative_indices() {
        let input = array(vec![string("a"), string("b"), string("c")]);
        assert_eq!(run("$[*]", &input), "[\"a\",\"b\",\"c\"]");
        assert_eq!(run("$[-1]", &input), "[\"c\"]");
        assert_eq!(run("$[-3]", &input), "[\"a\"]");
        assert_eq!(run("$[5]", &input), "[]");
        assert_eq!(run("$[0,2]", &input), "[\"a\",\"c\"]");
        let object_data = object(&[("a", number("1")), ("b", number("2"))]);
        assert_eq!(run("$.*", &object_data), "[1,2]");
    }

    #[test]
    fn slices_follow_the_rfc_table() {
        let input = array(vec![
            string("a"),
            string("b"),
            string("c"),
            string("d"),
            string("e"),
            string("f"),
            string("g"),
        ]);
        assert_eq!(run("$[1:3]", &input), "[\"b\",\"c\"]");
        assert_eq!(run("$[5:]", &input), "[\"f\",\"g\"]");
        assert_eq!(run("$[1:5:2]", &input), "[\"b\",\"d\"]");
        assert_eq!(run("$[5:1:-2]", &input), "[\"f\",\"d\"]");
        assert_eq!(run("$[::-1]", &input), "[\"g\",\"f\",\"e\",\"d\",\"c\",\"b\",\"a\"]");
        assert_eq!(run("$[::]", &input), "[\"a\",\"b\",\"c\",\"d\",\"e\",\"f\",\"g\"]");
        // step 0 selects nothing (the one Python divergence).
        assert_eq!(run("$[1:5:0]", &input), "[]");
        assert_eq!(run("$[-3:]", &input), "[\"e\",\"f\",\"g\"]");
    }

    #[test]
    fn descendant_segments_visit_pre_order() {
        let input = object(&[
            ("o", object(&[("j", number("1")), ("k", number("2"))])),
            (
                "a",
                array(vec![
                    number("5"),
                    number("3"),
                    array(vec![object(&[("j", number("4"))]), object(&[("k", number("6"))])]),
                ]),
            ),
        ]);
        assert_eq!(run("$..j", &input), "[1,4]");
        assert_eq!(run("$..[0]", &input), "[5,{\"j\":4}]");
        // The RFC's own Table 16 order for `$..*` — the input node's CHILDREN come first, the root itself is never
        // selected by `[*]`.
        assert_eq!(
            run("$..*", &input),
            "[{\"j\":1,\"k\":2},[5,3,[{\"j\":4},{\"k\":6}]],1,2,5,3,[{\"j\":4},{\"k\":6}],{\"j\":4},{\"k\":6},4,6]"
        );
    }

    #[test]
    fn filters_compare_and_test() {
        let input = bookstore();
        assert_eq!(
            run("$.store.book[?@.price < 10].title", &input),
            "[\"Sayings of the Century\",\"Moby Dick\"]"
        );
        assert_eq!(
            run("$.store.book[?@.price >= 10].title", &input),
            "[\"Sword of Honour\"]"
        );
        assert_eq!(
            run("$.store.book[?@.category == 'fiction'].title", &input),
            "[\"Sword of Honour\",\"Moby Dick\"]"
        );
        assert_eq!(
            run("$.store.book[?@.price < 10 && @.category == 'fiction'].title", &input),
            "[\"Moby Dick\"]"
        );
        assert_eq!(
            run(
                "$.store.book[?@.category == 'fiction' || @.price == 8.95].title",
                &input
            ),
            "[\"Sayings of the Century\",\"Sword of Honour\",\"Moby Dick\"]"
        );
        // Existence tests: `@.category` exists for every book; `@.isbn` for none.
        assert_eq!(run("$.store.book[?@.isbn]", &input), "[]");
        assert_eq!(
            run("$.store.book[?!@.isbn].title", &input),
            "[\"Sayings of the Century\",\"Sword of Honour\",\"Moby Dick\"]"
        );
        // Nothing == Nothing is true (RFC §2.3.5.2.2).
        assert_eq!(
            run("$.store.book[?@.isbn == @.missing].title", &input),
            "[\"Sayings of the Century\",\"Sword of Honour\",\"Moby Dick\"]"
        );
        // Numeric equality across spellings: `1 == 1.0` under the exact law.
        let numbers = array(vec![number("1"), number("1.0"), number("2")]);
        assert_eq!(run("$[?@ == 1]", &numbers), "[1,1.0]");
    }

    #[test]
    fn function_extensions() {
        let input = array(vec![
            object(&[("a", string("ab"))]),
            object(&[("a", string("x"))]),
            object(&[("b", string("c"))]),
        ]);
        assert_eq!(run("$[?length(@.a) == 2]", &input), "[{\"a\":\"ab\"}]");
        assert_eq!(run("$[?match(@.a, 'a.*')]", &input), "[{\"a\":\"ab\"}]");
        assert_eq!(run("$[?search(@.a, 'a')]", &input), "[{\"a\":\"ab\"}]");
        assert_eq!(run("$[?value(@.a) == 'x']", &input), "[{\"a\":\"x\"}]");
        // `@..*` is the element's descendants — its members, not itself.
        assert_eq!(
            run("$[?count(@..*) == 1]", &input),
            "[{\"a\":\"ab\"},{\"a\":\"x\"},{\"b\":\"c\"}]"
        );
        // A non-string or Nothing match argument is LogicalFalse, never an error: `@.a` is Nothing for the third
        // element, and `1` is not a string. (`match(...) == ...` is ILL-TYPED and rejected at parse — the CTS pins
        // that as an invalid selector.)
        assert_eq!(run("$[?match(@.a, 'zz')]", &input), "[]");
        assert_eq!(run("$[?match(@.b, 'c')]", &input), "[{\"b\":\"c\"}]");
        assert_eq!(run("$[?match(1, 'c')]", &input), "[]");
        // match() anchors; search() finds a substring.
        let words = array(vec![string("ab"), string("xab")]);
        assert_eq!(run("$[?match(@, 'ab')]", &words), "[\"ab\"]");
        assert_eq!(run("$[?search(@, 'ab')]", &words), "[\"ab\",\"xab\"]");
    }

    #[test]
    fn invalid_queries_are_rejected() {
        // Syntax rejections.
        for text in [
            "$[0 2]",
            "$[,0]",
            "$[0,]",
            "$[]",
            "$..",
            "$.&",
            "$.1",
            "$[@.a]",
            " $",
            "$ ",
            "$[01]",
            "$[-0]",
            "$[?@.a==+1]",
            "$[?@.a==- 1]",
            "$[?@.a==--1]",
        ] {
            rejected(text);
        }
        // A top-level query may be `$`-rooted or bare (the documented rooted extension); `@` names the CURRENT node,
        // which has no meaning at the top level — RFC 9535 queries are `$`-rooted, and a leading `@` is a syntax
        // error, never a silently rooted query.
        for text in ["@", "@.price", "@['a']", "@.price < 10"] {
            rejected(text);
        }
        // Well-typedness rejections (RFC §2.4.3): a non-singular query in a comparison, a ValueType function in a
        // test, a wrong-shaped arg.
        for text in [
            "$[?@[*]==0]",
            "$[?@[0:0]==0]",
            "$[?@..a==0]",
            "$[?count(1)>2]",
            "$[?count()==1]",
            "$[?length(@.a)]",
            "$[?length(@.*)<3]",
            "$[?match(@.a)==1]",
            "$[?match(@.a,@.b,@.c)==1]",
            "$[?match(@.a, 'a.*')==true]",
            "$[?value(@.a)]",
        ] {
            rejected(text);
        }
    }

    #[test]
    fn the_raise_through_parse_query_carries_the_detail() {
        let resources = resources();
        let error = parse_query(&string("$["), &resources).expect_err("invalid query raises");
        let text = raised_text(error);
        assert!(text.starts_with("invalid jsonpath query:"), "{text}");
        let error = parse_query(&number("1"), &resources).expect_err("non-string raises");
        assert_eq!(raised_text(error), "jsonpath query must be a string");
    }

    fn raised_text(error: EngineRunError) -> String {
        match error {
            EngineRunError::Raised(Value::String(text)) => String::from(text.as_str()),
            other => panic!("expected a raised string, got {other:?}"),
        }
    }

    #[test]
    fn the_rfc_surrogate_pair_escape_decodes() {
        let input = object(&[("🁁", string("value"))]);
        // `\uD83C\uDC41` is the DOMINO TILE pair from RFC §2.3.1.1's note.
        assert_eq!(run("$['\\uD83C\\uDC41']", &input), "[\"value\"]");
        assert!(
            rejected("$['\\uD83C']").starts_with("a lone high surrogate in a string escape"),
            "the lone-surrogate rejection names the cause"
        );
    }
}
