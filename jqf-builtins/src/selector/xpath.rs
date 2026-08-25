//! The `xml.xpath@1` selector profile: a closed XPath 3.1 subset (§4.9 of `codec-portfolio-design.md`).
//!
//! The closed grammar admits exactly:
//!
//! - absolute (`/a`, `//a`) and relative (`a`, `.`, `..`, `a//b`) paths;
//! - the child (default), `descendant`, `descendant-or-self`, `parent`, and
//!   `self` ELEMENT axes;
//! - expanded-name tests (`local`, `Q{uri}local`) and the wildcard `*`;
//! - union `|` of paths;
//! - positional predicates `[N]`, `[position() = N]` / `[N = position()]`, `[position() = last()]` /
//!   `[last() = position()]`, and equality of one XPath string literal with `@Q`, `text()`, or `string(.)` in either
//!   operand order.
//!
//! Every other construct — attribute/text/comment/PI/namespace/document and generic `node()` selection as result
//! axes, functions beyond the three atoms above, inequality, Boolean connectives, arithmetic, and everything beyond —
//! fails compilation with a named error. The two quote forms with the specified doubled-delimiter spelling are
//! accepted; no host-language escaping is imported. `prefix:local` forms are undeclared-prefix compile errors (v1 binds
//! no namespace environment; `Q{uri}local` is the one expanded-name spelling).
//!
//! Results are elements only, in document order, deduplicated. The context node is the scope passed to the evaluator:
//! relative paths start there, absolute paths start at the virtual document node. Attribute and text references are
//! predicate operands, never result-producing axes.

use alloc::borrow::Cow;
use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use super::SelectorBudget;
use super::error::SelectorError;
use super::index::{MarkupIndex, NodeRef};

#[derive(Clone, Debug)]
pub(crate) struct PathExpr {
    absolute: bool,
    /// The leading `//` abbreviation: one `descendant-or-self::node()` step from the document node, spelled `//`, never
    /// as a written `node()` test.
    leading_descendant: bool,
    steps: Vec<Step>,
}

#[derive(Clone, Debug)]
struct Step {
    axis: Axis,
    test: NodeTest,
    predicates: Vec<Predicate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Axis {
    Child,
    Descendant,
    DescendantOrSelf,
    Parent,
    Self_,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NodeTest {
    /// The wildcard `*` (and the `//` abbreviation's node()-semantics step).
    Any,
    /// A plain name: matches the element whose EXPANDED name has this local part and no namespace (`clark == local`).
    Name(String),
    /// `Q{uri}local`: matches the element whose clark name is exactly `{uri}local`. The clark text is formatted once at
    /// parse time.
    ExpandedName(String),
}

/// An attribute name's clark spelling (an unprefixed name's clark is the local part itself), formatted once at parse
/// time.
type AttrName = String;

#[derive(Clone, Debug)]
enum Predicate {
    /// `[N]` (or `[position() = N]` / `[N = position()]`) — the set's `N`-th member (1-based) in document order.
    Position(u64),
    /// `[LEFT OP RIGHT]` — a general comparison over each member.
    Compare {
        left: ValueAtom,
        op: CmpOp,
        right: ValueAtom,
    },
}

/// A comparison operator inside a predicate (the missing `=` arm generalizes to the ordered set).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// One side of a predicate comparison: a scalar value or a node-context quantity, evaluated per member of the filtered
/// set.
#[derive(Clone, Debug)]
pub(crate) enum ValueAtom {
    /// A quoted string literal.
    Literal(String),
    /// An unquoted number literal (XPath numbers are binary64 — this is the comparison's OWN conversion, never the
    /// value model's, exactly like XPath 1.0's `number()` conversion of an attribute string).
    Number(f64),
    /// `@name` — the direct attribute's string value.
    Attr(AttrName),
    /// `text()` — the direct text children; a comparison is true when ANY child satisfies it, and a scalar position
    /// (concat/string-length) reads the FIRST child's value (XPath's node-set string() law).
    Text,
    /// `string(.)` — the element's string value (concatenated descendant text).
    StringValue,
    /// `name()` — the element's expanded name.
    Name,
    /// `position()` — the member's 1-based position in the filtered set.
    Position,
    /// `last()` — the filtered set's size.
    Last,
    /// `count(path)` — the number of nodes `path` selects from the member.
    Count(Box<PathExpr>),
    /// `string-length(atom)` — the atom's string value's codepoint length.
    StringLength(Box<ValueAtom>),
    /// `concat(atom, ...)` — the atoms' string values concatenated.
    Concat(Vec<ValueAtom>),
}

/// The parser cursor over selector text.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
    text: &'a str,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            at: 0,
            text,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }

    fn error<T>(&self, message: &str) -> Result<T, SelectorError> {
        Err(SelectorError::Compile {
            message: message.to_string(),
            offset: self.at,
        })
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.at += 1;
        }
    }

    fn starts(&self, prefix: &str) -> bool {
        self.text[self.at..].starts_with(prefix)
    }

    fn take(&mut self, prefix: &str) {
        debug_assert!(self.starts(prefix));
        self.at += prefix.len();
    }

    /// Parses one NCName (no colon).
    fn ncname(&mut self) -> Result<Option<String>, SelectorError> {
        let start = self.at;
        match self.peek() {
            Some(b) if is_name_start(b) => {
                self.at += 1;
            }
            _ => return Ok(None),
        }
        while let Some(b) = self.peek() {
            if is_name_continue(b) {
                self.at += 1;
            } else {
                break;
            }
        }
        Ok(Some(self.text[start..self.at].to_string()))
    }

    /// Parses one string literal with doubled-delimiter escaping.
    fn literal(&mut self) -> Result<Option<String>, SelectorError> {
        let quote = match self.peek() {
            Some(b @ (b'\'' | b'"')) => b,
            _ => return Ok(None),
        };
        let start = self.at;
        self.at += 1;
        let mut end = self.at;
        loop {
            match self.bytes.get(end).copied() {
                None => return self.error("unterminated string literal"),
                Some(b) if b == quote => {
                    if self.bytes.get(end + 1) == Some(&quote) {
                        end += 2;
                    } else {
                        break;
                    }
                }
                Some(_) => end += 1,
            }
        }
        // end now points at the closing quote; the raw span is start+1..end.
        let raw = &self.text[start + 1..end];
        let mut out = String::new();
        let mut cursor = 0;
        while cursor < raw.len() {
            let byte = raw.as_bytes()[cursor];
            if byte == quote {
                // A doubled delimiter: exactly one escaped quote byte.
                out.push(quote as char);
                cursor += 2;
            } else {
                // One UTF-8 scalar, copied verbatim.
                let ch = raw[cursor..].chars().next().ok_or_else(|| SelectorError::Compile {
                    message: "invalid UTF-8 in literal".to_string(),
                    offset: start + cursor,
                })?;
                out.push(ch);
                cursor += ch.len_utf8();
            }
        }
        self.at = end + 1;
        Ok(Some(out))
    }
}

fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_name_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}

/// One compiled XPath selector: either the element-result path law (the closed step grammar) or a TOP-LEVEL FUNCTION
/// call whose result is a scalar (the four pure functions the predicate grammar knows answer at top level, where XPath
/// 1.0 evaluates them with the document node as the context node).
#[derive(Debug)]
pub(crate) enum XPathPlan {
    /// An element-result expression: one or more `|`-unioned paths.
    Paths(Vec<PathExpr>),
    /// One top-level function call returning a scalar.
    Function(FunctionExpr),
}

/// A top-level function call, reusing the predicate grammar's atoms.
#[derive(Debug)]
pub(crate) enum FunctionExpr {
    /// `count(path)` — the node-set's cardinality, XPath §2.6.
    Count(PathExpr),
    /// `concat(atom, …)` — XPath §4.2.
    Concat(alloc::vec::Vec<ValueAtom>),
    /// `string-length(atom)` — XPath §4.4, codepoints.
    StringLength(Box<ValueAtom>),
    /// `name()` — the context node's QName; the document node has none, so the top-level answer is the empty string.
    Name,
}

pub(crate) fn compile_xpath(text: &str) -> Result<XPathPlan, SelectorError> {
    let mut cursor = Cursor::new(text);
    // A top-level function result is a whole-expression form: the scalar consumes the selector and nothing may follow
    // it (a `|` union of a scalar and a node-set is an XPath type error, rejected here as a compile error).
    cursor.skip_ws();
    if let Some(function) = parse_top_level_function(&mut cursor)? {
        cursor.skip_ws();
        if cursor.peek().is_some() {
            return cursor.error("a scalar function result cannot be unioned with a path");
        }
        return Ok(XPathPlan::Function(function));
    }
    let mut paths = Vec::new();
    loop {
        cursor.skip_ws();
        paths.push(parse_path(&mut cursor)?);
        cursor.skip_ws();
        match cursor.peek() {
            None => break,
            Some(b'|') => {
                cursor.bump();
            }
            Some(_) => return cursor.error("expected '|' or end of selector"),
        }
    }
    Ok(XPathPlan::Paths(paths))
}

/// The top-level function call at the head of `cursor`, if the selector begins with one. Returns `Ok(None)` when the
/// text is not a function call (an ordinary path is then parsed by the caller).
fn parse_top_level_function(cursor: &mut Cursor<'_>) -> Result<Option<FunctionExpr>, SelectorError> {
    if cursor.starts("count(") {
        cursor.take("count(");
        let path = parse_path(cursor)?;
        cursor.skip_ws();
        if cursor.bump() != Some(b')') {
            return cursor.error("expected ')' after count's path");
        }
        return Ok(Some(FunctionExpr::Count(path)));
    }
    if cursor.starts("concat(") {
        cursor.take("concat(");
        let mut atoms = alloc::vec::Vec::new();
        loop {
            atoms.push(parse_atom(cursor)?);
            cursor.skip_ws();
            match cursor.peek() {
                Some(b',') => {
                    cursor.bump();
                }
                Some(b')') => {
                    cursor.bump();
                    break;
                }
                _ => return cursor.error("expected ',' or ')' inside concat(...)"),
            }
        }
        return Ok(Some(FunctionExpr::Concat(atoms)));
    }
    if cursor.starts("string-length(") {
        cursor.take("string-length(");
        let inner = parse_atom(cursor)?;
        cursor.skip_ws();
        if cursor.bump() != Some(b')') {
            return cursor.error("expected ')' after string-length's argument");
        }
        return Ok(Some(FunctionExpr::StringLength(Box::new(inner))));
    }
    if cursor.starts("name()") {
        cursor.take("name()");
        return Ok(Some(FunctionExpr::Name));
    }
    Ok(None)
}

fn parse_path(cursor: &mut Cursor<'_>) -> Result<PathExpr, SelectorError> {
    cursor.skip_ws();
    let mut absolute = false;
    let mut leading_descendant = false;
    if cursor.peek() == Some(b'/') {
        cursor.bump();
        if cursor.peek() == Some(b'/') {
            cursor.bump();
            leading_descendant = true;
        }
        absolute = true;
    }
    let mut steps = Vec::new();
    loop {
        cursor.skip_ws();
        match cursor.peek() {
            None | Some(b'|') | Some(b')') | Some(b',') => break,
            Some(b'/') => {
                cursor.bump();
                if cursor.peek() == Some(b'/') {
                    cursor.bump();
                    steps.push(Step {
                        axis: Axis::DescendantOrSelf,
                        test: NodeTest::Any,
                        predicates: Vec::new(),
                    });
                }
            }
            Some(_) => steps.push(parse_step(cursor)?),
        }
    }
    if steps.is_empty() {
        // `/` or `//` alone selects the document node / every node — result kinds the profile cannot represent.
        return cursor.error("a bare '/' or '//' selects the document node, which is not an element result");
    }
    Ok(PathExpr {
        absolute,
        leading_descendant,
        steps,
    })
}

fn parse_step(cursor: &mut Cursor<'_>) -> Result<Step, SelectorError> {
    cursor.skip_ws();
    // Axis specifier or the `.`/`..` abbreviations. Both abbreviations imply a node()-test step whose results are
    // restricted to elements, exactly like the `//` abbreviation's own node()-semantics step.
    if cursor.peek() == Some(b'.') {
        let axis = if cursor.bytes.get(cursor.at + 1) == Some(&b'.') {
            cursor.at += 2;
            Axis::Parent
        } else {
            cursor.at += 1;
            Axis::Self_
        };
        return Ok(Step {
            axis,
            test: NodeTest::Any,
            predicates: Vec::new(),
        });
    }
    let axis = if cursor.starts("child::") {
        cursor.take("child::");
        Axis::Child
    } else if cursor.starts("descendant-or-self::") {
        cursor.take("descendant-or-self::");
        Axis::DescendantOrSelf
    } else if cursor.starts("descendant::") {
        cursor.take("descendant::");
        Axis::Descendant
    } else if cursor.starts("parent::") {
        cursor.take("parent::");
        Axis::Parent
    } else if cursor.starts("self::") {
        cursor.take("self::");
        Axis::Self_
    } else {
        Axis::Child
    };
    cursor.skip_ws();
    let test = parse_node_test(cursor)?;
    let mut predicates = Vec::new();
    loop {
        cursor.skip_ws();
        if cursor.peek() == Some(b'[') {
            predicates.push(parse_predicate(cursor)?);
        } else {
            break;
        }
    }
    Ok(Step { axis, test, predicates })
}

fn parse_node_test(cursor: &mut Cursor<'_>) -> Result<NodeTest, SelectorError> {
    cursor.skip_ws();
    if cursor.peek() == Some(b'*') {
        cursor.bump();
        return Ok(NodeTest::Any);
    }
    if cursor.starts("Q{") {
        let close = cursor.text[cursor.at..]
            .find('}')
            .ok_or_else(|| SelectorError::Compile {
                message: "unterminated Q{uri} expanded name".to_string(),
                offset: cursor.at,
            })?;
        let uri = cursor.text[cursor.at + 2..cursor.at + close].to_string();
        cursor.at += close + 1;
        let local = cursor.ncname()?.ok_or_else(|| SelectorError::Compile {
            message: "Q{uri} expanded name missing its local part".to_string(),
            offset: cursor.at,
        })?;
        return Ok(NodeTest::ExpandedName(alloc::format!("{{{uri}}}{local}")));
    }
    if cursor.peek() == Some(b'@') {
        return cursor.error("attribute selection is a predicate operand, never a result axis");
    }
    if cursor.starts("node()") {
        return cursor.error("generic node() selection fails compilation");
    }
    if cursor.starts("text()") {
        return cursor.error("text() is a predicate operand, never a result axis");
    }
    let name = cursor.ncname()?.ok_or_else(|| SelectorError::Compile {
        message: "expected an element name, '*', or an axis".to_string(),
        offset: cursor.at,
    })?;
    if name.contains(':') {
        return cursor.error("prefix:local names need a bound namespace environment; use Q{uri}local");
    }
    Ok(NodeTest::Name(name))
}

/// Parses an expanded-name attribute reference (after `@`).
fn parse_attr_name(cursor: &mut Cursor<'_>) -> Result<AttrName, SelectorError> {
    if cursor.starts("Q{") {
        let close = cursor.text[cursor.at..]
            .find('}')
            .ok_or_else(|| SelectorError::Compile {
                message: "unterminated Q{uri} attribute name".to_string(),
                offset: cursor.at,
            })?;
        let uri = cursor.text[cursor.at + 2..cursor.at + close].to_string();
        cursor.at += close + 1;
        let local = cursor.ncname()?.ok_or_else(|| SelectorError::Compile {
            message: "Q{uri} attribute name missing its local part".to_string(),
            offset: cursor.at,
        })?;
        return Ok(alloc::format!("{{{uri}}}{local}"));
    }
    let name = cursor.ncname()?.ok_or_else(|| SelectorError::Compile {
        message: "expected an attribute name after '@'".to_string(),
        offset: cursor.at,
    })?;
    if name.contains(':') {
        return cursor.error("prefix:local attribute names need a bound namespace environment");
    }
    Ok(name)
}

fn parse_predicate(cursor: &mut Cursor<'_>) -> Result<Predicate, SelectorError> {
    // The `[` was confirmed by the caller's peek.
    debug_assert_eq!(cursor.peek(), Some(b'['));
    cursor.bump();
    cursor.skip_ws();
    // The `[N]` shorthand: a bare positive integer with no comparison. Its explicit spellings (`[position() = N]`, `[N
    // = position()]`) collapse into the same Position law; any other comparison is general.
    if matches!(cursor.peek(), Some(b'0'..=b'9')) {
        let number = parse_positive_integer(cursor)?;
        cursor.skip_ws();
        return if let Some(op) = parse_cmp_op(cursor) {
            cursor.skip_ws();
            let right = parse_atom(cursor)?;
            let predicate = if op == CmpOp::Eq && matches!(right, ValueAtom::Position) {
                Predicate::Position(number)
            } else {
                Predicate::Compare {
                    left: ValueAtom::Number(number as f64),
                    op,
                    right,
                }
            };
            finish_predicate(cursor, predicate)
        } else {
            finish_predicate(cursor, Predicate::Position(number))
        };
    }
    let left = parse_atom(cursor)?;
    cursor.skip_ws();
    let op = parse_cmp_op(cursor).ok_or_else(|| SelectorError::Compile {
        message: "expected a comparison operator ('=', '!=', '<', '<=', '>', '>=')".to_string(),
        offset: cursor.at,
    })?;
    cursor.skip_ws();
    let right = parse_atom(cursor)?;
    finish_predicate(cursor, Predicate::Compare { left, op, right })
}

/// Consumes the closing `]` after a parsed predicate body.
fn finish_predicate(cursor: &mut Cursor<'_>, predicate: Predicate) -> Result<Predicate, SelectorError> {
    cursor.skip_ws();
    if cursor.peek() != Some(b']') {
        return cursor.error("expected ']' to close the predicate");
    }
    cursor.bump();
    Ok(predicate)
}

/// Parses one comparison operator, if present.
fn parse_cmp_op(cursor: &mut Cursor<'_>) -> Option<CmpOp> {
    let op = match cursor.peek() {
        Some(b'=') => CmpOp::Eq,
        Some(b'!') if cursor.bytes.get(cursor.at + 1) == Some(&b'=') => CmpOp::Ne,
        Some(b'<') => {
            if cursor.bytes.get(cursor.at + 1) == Some(&b'=') {
                CmpOp::Le
            } else {
                CmpOp::Lt
            }
        }
        Some(b'>') => {
            if cursor.bytes.get(cursor.at + 1) == Some(&b'=') {
                CmpOp::Ge
            } else {
                CmpOp::Gt
            }
        }
        _ => return None,
    };
    let width = match op {
        CmpOp::Eq | CmpOp::Lt | CmpOp::Gt => 1,
        CmpOp::Ne | CmpOp::Le | CmpOp::Ge => 2,
    };
    cursor.at += width;
    Some(op)
}

/// Parses one predicate VALUE atom: a literal, a number, an attribute reference,
/// text()/string(.)/name()/position()/last(), or the pure functions count()/concat()/string-length().
fn parse_atom(cursor: &mut Cursor<'_>) -> Result<ValueAtom, SelectorError> {
    cursor.skip_ws();
    match cursor.peek() {
        Some(b'\'' | b'"') => Ok(ValueAtom::Literal(cursor.literal()?.ok_or_else(|| {
            SelectorError::Compile {
                message: "expected a string literal".to_string(),
                offset: cursor.at,
            }
        })?)),
        Some(b'0'..=b'9' | b'-') => {
            let (number, _) = parse_number_literal(cursor)?;
            Ok(ValueAtom::Number(number))
        }
        Some(b'@') => {
            cursor.bump();
            Ok(ValueAtom::Attr(parse_attr_name(cursor)?))
        }
        Some(_) => {
            if cursor.starts("text()") {
                cursor.take("text()");
                Ok(ValueAtom::Text)
            } else if cursor.starts("string(.)") {
                cursor.take("string(.)");
                Ok(ValueAtom::StringValue)
            } else if cursor.starts("string-length(") {
                cursor.take("string-length(");
                let inner = parse_atom(cursor)?;
                cursor.skip_ws();
                if cursor.bump() != Some(b')') {
                    return cursor.error("expected ')' after string-length's argument");
                }
                Ok(ValueAtom::StringLength(Box::new(inner)))
            } else if cursor.starts("concat(") {
                cursor.take("concat(");
                let mut atoms = Vec::new();
                loop {
                    atoms.push(parse_atom(cursor)?);
                    cursor.skip_ws();
                    match cursor.peek() {
                        Some(b',') => {
                            cursor.bump();
                        }
                        Some(b')') => {
                            cursor.bump();
                            break;
                        }
                        _ => return cursor.error("expected ',' or ')' inside concat(...)"),
                    }
                }
                Ok(ValueAtom::Concat(atoms))
            } else if cursor.starts("count(") {
                cursor.take("count(");
                let path = parse_path(cursor)?;
                cursor.skip_ws();
                if cursor.bump() != Some(b')') {
                    return cursor.error("expected ')' after count's path");
                }
                Ok(ValueAtom::Count(Box::new(path)))
            } else if cursor.starts("name()") {
                cursor.take("name()");
                Ok(ValueAtom::Name)
            } else if cursor.starts("position()") {
                cursor.take("position()");
                Ok(ValueAtom::Position)
            } else if cursor.starts("last()") {
                cursor.take("last()");
                Ok(ValueAtom::Last)
            } else {
                cursor.error(
                    "expected a literal, number, @attribute, text(), string(.), name(), \
                     position(), last(), count(...), concat(...), or string-length(...)",
                )
            }
        }
        None => cursor.error("expected a predicate operand"),
    }
}

/// Parses a number literal: an optional minus, digits, and an optional fractional part, converted through XPath's own
/// binary64 number law. The text slice is returned for the positive-integer shorthand law.
fn parse_number_literal<'a>(cursor: &mut Cursor<'a>) -> Result<(f64, &'a str), SelectorError> {
    let start = cursor.at;
    if cursor.peek() == Some(b'-') {
        cursor.at += 1;
    }
    let digits_start = cursor.at;
    while matches!(cursor.peek(), Some(b'0'..=b'9')) {
        cursor.at += 1;
    }
    if cursor.at == digits_start {
        return cursor.error("expected a number");
    }
    if cursor.peek() == Some(b'.') {
        cursor.at += 1;
        while matches!(cursor.peek(), Some(b'0'..=b'9')) {
            cursor.at += 1;
        }
    }
    let text = &cursor.text[start..cursor.at];
    let number = text.parse::<f64>().map_err(|_| SelectorError::Compile {
        message: "number literal out of range".to_string(),
        offset: start,
    })?;
    Ok((number, text))
}

/// Parses a positive decimal integer.
fn parse_positive_integer(cursor: &mut Cursor<'_>) -> Result<u64, SelectorError> {
    let start = cursor.at;
    while matches!(cursor.peek(), Some(b'0'..=b'9')) {
        cursor.at += 1;
    }
    if cursor.at == start {
        return cursor.error("expected a positive integer");
    }
    let digits = &cursor.text[start..cursor.at];
    if digits.starts_with('0') && digits.len() > 1 {
        return cursor.error("leading zeroes are not a positive integer");
    }
    let value = digits.parse::<u64>().map_err(|_| SelectorError::Compile {
        message: "integer out of range".to_string(),
        offset: start,
    })?;
    if value == 0 {
        return cursor.error("XPath positions are positive integers");
    }
    Ok(value)
}

/// One selector evaluation's result: the element node-set (the path law) or a scalar (a top-level function call).
pub(crate) enum XPathOutcome {
    /// Element results, in document order, deduplicated.
    Nodes(alloc::vec::Vec<jqf_data::NodeId>),
    /// One scalar result (count/concat/string-length/name).
    Scalar(AtomScalar),
}

pub(crate) fn evaluate(
    index: &MarkupIndex,
    scope: jqf_data::NodeId,
    plan: &XPathPlan,
    budget: SelectorBudget,
) -> Result<XPathOutcome, SelectorError> {
    let XPathPlan::Paths(paths) = plan else {
        // A top-level function evaluates with the DOCUMENT NODE as the context node (position 1 of 1), exactly XPath
        // 1.0's top-level context. The four functions are the predicate grammar's atoms, so the evaluation reuses
        // `resolve_atom` wholesale; the budget is charged through the shared activation counter.
        let atom = match plan {
            XPathPlan::Function(FunctionExpr::Count(path)) => ValueAtom::Count(alloc::boxed::Box::new(path.clone())),
            XPathPlan::Function(FunctionExpr::Concat(atoms)) => ValueAtom::Concat(atoms.clone()),
            XPathPlan::Function(FunctionExpr::StringLength(inner)) => ValueAtom::StringLength(inner.clone()),
            XPathPlan::Function(FunctionExpr::Name) => ValueAtom::Name,
            XPathPlan::Paths(_) => unreachable!("function arm matched the path plan"),
        };
        let mut candidates = 0u64;
        return match resolve_atom(index, NodeRef::Document, 1, 1, &atom, budget, &mut candidates)? {
            AtomValue::Scalar(scalar) => Ok(XPathOutcome::Scalar(scalar)),
            AtomValue::TextValues(_) | AtomValue::Missing => Err(SelectorError::Internal {
                contract: "a function result resolved to a node-set",
            }),
        };
    };
    let context = NodeRef::Element(scope);
    let mut results: Vec<NodeRef> = Vec::new();
    let mut candidates = 0u64;
    for path in paths {
        let set = eval_path(index, path, context, budget, &mut candidates)?;
        results.extend(set);
    }
    // A union is the document-ordered merge of its members, deduplicated (dedup_order sorts and dedups below). The
    // sentinel sorts first and can never survive the element filter, but the order is the law, not an accident of path
    // order.
    dedup_order(index, &mut results);
    let mut out = Vec::new();
    for member in results {
        if let NodeRef::Element(node) = member {
            if in_scope_domain(index, scope, node) {
                out.push(node);
            }
        }
    }
    if out.len() as u64 > budget.max_results {
        return Err(SelectorError::Budget { what: "results" });
    }
    Ok(XPathOutcome::Nodes(out))
}

/// Whether `node` is the scope element or one of its element descendants — the traversal domain the scope law names
/// (lib.rs docs). Absolute paths and leading-`//` paths seed the document node and every element, so their results are
/// cut here; the ancestor chain walk is the domain test.
fn in_scope_domain(index: &MarkupIndex, scope: jqf_data::NodeId, node: jqf_data::NodeId) -> bool {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if current == scope {
            return true;
        }
        cursor = index.parent_of(current);
    }
    false
}

fn eval_path(
    index: &MarkupIndex,
    path: &PathExpr,
    context: NodeRef,
    budget: SelectorBudget,
    candidates: &mut u64,
) -> Result<Vec<NodeRef>, SelectorError> {
    let mut set: Vec<NodeRef> = if path.absolute {
        vec![NodeRef::Document]
    } else {
        vec![context]
    };
    if path.leading_descendant {
        // `//` at the start: the document node plus every element, in document order (the descendant-or-self::node()
        // abbreviation).
        set = vec![NodeRef::Document];
        for node in index.element_ids_in_document_order()? {
            set.push(NodeRef::Element(node));
        }
    }
    for step in &path.steps {
        let mut next: Vec<NodeRef> = Vec::new();
        for member in &set {
            *candidates = charge_candidates(*candidates, budget)?;
            // The step is applied with each member as the CONTEXT NODE, and its predicates are evaluated against THAT
            // member's candidate set — XPath's per-context position law (`//p[1]` is the FIRST p child of every
            // parent, not the first p in the document).
            let mut candidates_for = Vec::new();
            axis_step(index, *member, &step.test, step.axis, budget, &mut candidates_for)?;
            dedup_order(index, &mut candidates_for);
            let mut filtered = candidates_for;
            for predicate in &step.predicates {
                *candidates = charge_candidates(*candidates, budget)?;
                filtered = apply_predicate(index, filtered, predicate, budget, candidates)?;
            }
            next.extend(filtered);
        }
        // The step's union across context nodes: document order, deduplicated (the sentinel first).
        dedup_order(index, &mut next);
        set = next;
    }
    Ok(set)
}

fn charge_candidates(candidates: u64, budget: SelectorBudget) -> Result<u64, SelectorError> {
    let next = candidates.checked_add(1).ok_or_else(|| SelectorError::Budget {
        what: "candidate tests",
    })?;
    if next > budget.max_candidate_tests {
        return Err(SelectorError::Budget {
            what: "candidate tests",
        });
    }
    Ok(next)
}

/// One axis step over one member of the input set.
fn axis_step(
    index: &MarkupIndex,
    member: NodeRef,
    test: &NodeTest,
    axis: Axis,
    budget: SelectorBudget,
    out: &mut Vec<NodeRef>,
) -> Result<(), SelectorError> {
    match axis {
        Axis::Child => {
            if let NodeRef::Document = member {
                // The document node's children: the document element.
                if test_matches(index, index.document_element, test) {
                    out.push(NodeRef::Element(index.document_element));
                }
                return Ok(());
            }
            let node = member.element().ok_or_else(|| SelectorError::Internal {
                contract: "element member of a child step",
            })?;
            for child in index.children_of(node) {
                if index.is_element(*child) && test_matches(index, *child, test) {
                    out.push(NodeRef::Element(*child));
                }
            }
        }
        Axis::Descendant => {
            match member {
                NodeRef::Element(node) => {
                    collect_descendants(index, node, test, out, budget)?;
                }
                NodeRef::Document => {
                    // The document node's descendants are every element: the document element and its whole subtree.
                    collect_descendants_from(index, index.document_element, test, out, budget)?;
                }
            }
        }
        Axis::DescendantOrSelf => match member {
            NodeRef::Element(node) => {
                if test_matches(index, node, test) {
                    out.push(NodeRef::Element(node));
                }
                collect_descendants(index, node, test, out, budget)?;
            }
            NodeRef::Document => {
                collect_descendants_from(index, index.document_element, test, out, budget)?;
            }
        },
        Axis::Parent => {
            if let NodeRef::Element(node) = member {
                if let Some(parent) = index.parent_of(node) {
                    if test_matches(index, parent, test) {
                        out.push(NodeRef::Element(parent));
                    }
                }
            }
        }
        Axis::Self_ => {
            if let NodeRef::Element(node) = member {
                if test_matches(index, node, test) {
                    out.push(NodeRef::Element(node));
                }
            }
        }
    }
    Ok(())
}

fn collect_descendants(
    index: &MarkupIndex,
    node: jqf_data::NodeId,
    test: &NodeTest,
    out: &mut Vec<NodeRef>,
    budget: SelectorBudget,
) -> Result<(), SelectorError> {
    let mut stack: Vec<jqf_data::NodeId> = index
        .children_of(node)
        .iter()
        .rev()
        .copied()
        .filter(|child| index.is_element(*child))
        .collect();
    while let Some(cursor) = stack.pop() {
        if test_matches(index, cursor, test) {
            out.push(NodeRef::Element(cursor));
        }
        for child in index.children_of(cursor).iter().rev() {
            if index.is_element(*child) {
                stack.push(*child);
            }
        }
        if stack.len() as u64 > budget.max_walk_steps {
            return Err(SelectorError::Budget { what: "walk steps" });
        }
    }
    Ok(())
}

/// The descendants of the document ELEMENT, itself included (the document node's descendant-or-self set with the
/// document node itself excluded).
fn collect_descendants_from(
    index: &MarkupIndex,
    document_element: jqf_data::NodeId,
    test: &NodeTest,
    out: &mut Vec<NodeRef>,
    budget: SelectorBudget,
) -> Result<(), SelectorError> {
    if test_matches(index, document_element, test) {
        out.push(NodeRef::Element(document_element));
    }
    collect_descendants(index, document_element, test, out, budget)
}

fn test_matches(index: &MarkupIndex, node: jqf_data::NodeId, test: &NodeTest) -> bool {
    match test {
        NodeTest::Any => true,
        NodeTest::Name(local) => index.name_of(node) == local,
        NodeTest::ExpandedName(clark) => index.name_of(node) == clark,
    }
}

/// Orders a set in document order (the sentinel first) and removes duplicates.
fn dedup_order(index: &MarkupIndex, set: &mut Vec<NodeRef>) {
    set.sort_by_key(|member| match member {
        NodeRef::Document => None,
        NodeRef::Element(node) => Some(index.rank_of(*node)),
    });
    set.dedup();
}

fn apply_predicate(
    index: &MarkupIndex,
    set: Vec<NodeRef>,
    predicate: &Predicate,
    budget: SelectorBudget,
    candidates: &mut u64,
) -> Result<Vec<NodeRef>, SelectorError> {
    let last = set.len() as u64;
    let mut out = Vec::new();
    for (position, member) in set.into_iter().enumerate() {
        let position = position as u64 + 1;
        let keep = match predicate {
            Predicate::Position(n) => position == *n,
            Predicate::Compare { left, op, right } => {
                compare_atoms(index, member, position, last, left, *op, right, budget, candidates)?
            }
        };
        if keep {
            out.push(member);
        }
    }
    Ok(out)
}

/// One side of a comparison, resolved against one member of the filtered set: either a single scalar or a node-set of
/// text values (text()).
enum AtomValue {
    /// A single scalar.
    Scalar(AtomScalar),
    /// The direct text children's values (the `text()` node-set): a comparison holds when ANY child satisfies it.
    TextValues(alloc::vec::Vec<alloc::string::String>),
    /// An absent attribute: every comparison is FALSE (XPath's empty node-set law).
    Missing,
}

/// A resolved scalar side.
#[derive(Clone, Debug)]
pub(crate) enum AtomScalar {
    Number(f64),
    Text(alloc::string::String),
}

impl AtomScalar {
    /// The side's string value. A number renders via XPath's own number-to-string law (`xpath_number_string`, §4.4):
    /// NaN -> "NaN", infinities -> "Infinity" / "-Infinity", an integer in decimal form with no decimal point, and
    /// every other finite value in decimal form with a point — never an exponent.
    fn as_cow(&self) -> Cow<'_, str> {
        match self {
            AtomScalar::Number(n) => {
                if n.is_nan() {
                    Cow::Borrowed("NaN")
                } else if *n == f64::INFINITY {
                    Cow::Borrowed("Infinity")
                } else if *n == f64::NEG_INFINITY {
                    Cow::Borrowed("-Infinity")
                } else {
                    Cow::Owned(xpath_number_string(*n))
                }
            }
            AtomScalar::Text(text) => Cow::Borrowed(text),
        }
    }
}

/// Evaluates one comparison against one member. The law is XPath 1.0's general-comparison simplification over scalar
/// sides: when EITHER side is a number, both sides convert to numbers (an unparseable string converts to NaN, and every
/// NaN comparison is false); otherwise the sides compare as strings. A missing attribute is an empty node-set: false.
/// `text()` on a side is true when ANY direct text child satisfies the comparison (general comparison over the
/// node-set).
#[allow(
    clippy::too_many_arguments,
    reason = "XPath general comparison threads context (index, member, position, last) plus both atoms and the budget through one resolver; splitting would scatter the counting discipline"
)]
fn compare_atoms(
    index: &MarkupIndex,
    member: NodeRef,
    position: u64,
    last: u64,
    left: &ValueAtom,
    op: CmpOp,
    right: &ValueAtom,
    budget: SelectorBudget,
    candidates: &mut u64,
) -> Result<bool, SelectorError> {
    let left_value = resolve_atom(index, member, position, last, left, budget, candidates)?;
    let right_value = resolve_atom(index, member, position, last, right, budget, candidates)?;
    // The text() node-set law: true when any pair satisfies. A text() side whose node-set is EMPTY is false (XPath).
    match (left_value, right_value) {
        (AtomValue::TextValues(lefts), AtomValue::TextValues(rights)) => {
            if lefts.is_empty() || rights.is_empty() {
                return Ok(false);
            }
            for left in &lefts {
                for right in &rights {
                    if compare_scalars(AtomScalar::Text(left.clone()), op, AtomScalar::Text(right.clone())) {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        }
        (AtomValue::TextValues(texts), AtomValue::Scalar(right))
        | (AtomValue::Scalar(right), AtomValue::TextValues(texts)) => {
            if texts.is_empty() {
                return Ok(false);
            }
            for text in &texts {
                if compare_scalars(AtomScalar::Text(text.clone()), op, right.clone()) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        (AtomValue::Missing, _) | (_, AtomValue::Missing) => Ok(false),
        (AtomValue::Scalar(left), AtomValue::Scalar(right)) => Ok(compare_scalars(left, op, right)),
    }
}

/// Applies one operator to two resolved scalars.
fn compare_scalars(left: AtomScalar, op: CmpOp, right: AtomScalar) -> bool {
    // Numeric when either side is a number (XPath 1.0's conversion law).
    if matches!(left, AtomScalar::Number(_)) || matches!(right, AtomScalar::Number(_)) {
        let left = as_number(left);
        let right = as_number(right);
        return match op {
            CmpOp::Eq => left == right,
            CmpOp::Ne => left != right,
            CmpOp::Lt => left < right,
            CmpOp::Le => left <= right,
            CmpOp::Gt => left > right,
            CmpOp::Ge => left >= right,
        };
    }
    let left = left.as_cow();
    let right = right.as_cow();
    match op {
        CmpOp::Eq => left == right,
        CmpOp::Ne => left != right,
        CmpOp::Lt => left < right,
        CmpOp::Le => left <= right,
        CmpOp::Gt => left > right,
        CmpOp::Ge => left >= right,
    }
}

/// XPath's number() conversion of one side.
fn as_number(scalar: AtomScalar) -> f64 {
    match scalar {
        AtomScalar::Number(n) => n,
        AtomScalar::Text(text) => xpath_number(&text),
    }
}

/// XPath 1.0 §4.4 `number()`: strip leading/trailing whitespace, then the text must be an optional `-` followed by
/// digits with an OPTIONAL fractional part (`-?digits(.digits?)?` or `-?.digits`) — no exponent, no `+`, no
/// `Infinity`/`NaN`. Anything else is NaN. Rust's `parse::<f64>` accepts spellings XPath rejects (`+1`, `1e5`,
/// `Infinity`), so it cannot be the conversion; the grammar check runs first, then a bare digits-only parse (overflow
/// saturates to infinity, matching IEEE/XPath).
fn xpath_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return f64::NAN;
    }
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let (int_part, frac_part) = match digits.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (digits, None),
    };
    let int_ok = int_part.is_empty() || int_part.bytes().all(|b| b.is_ascii_digit());
    let frac_ok = frac_part.is_none_or(|f| f.is_empty() || f.bytes().all(|b| b.is_ascii_digit()));
    let has_digit = !int_part.is_empty() || frac_part.is_some_and(|f| !f.is_empty());
    if !int_ok || !frac_ok || !has_digit {
        return f64::NAN;
    }
    let value: f64 = digits.parse().unwrap_or(f64::INFINITY);
    if negative { -value } else { value }
}

/// XPath 1.0 §4.4 `string(number)`: NaN -> "NaN", infinities as named, zero (either sign) -> "0"; otherwise the number
/// in DECIMAL form — an integer with no decimal point, a non-integer with at least one digit on each side of the
/// point and no trailing zeros. The Number production has no exponent, so no magnitude ever renders in scientific
/// notation.
fn xpath_number_string(value: f64) -> alloc::string::String {
    if value.is_nan() {
        return alloc::string::String::from("NaN");
    }
    if value == f64::INFINITY {
        return alloc::string::String::from("Infinity");
    }
    if value == f64::NEG_INFINITY {
        return alloc::string::String::from("-Infinity");
    }
    if value == 0.0 {
        return alloc::string::String::from("0");
    }
    let negative = value < 0.0;
    let magnitude = value.abs();
    // Integer fast-path: an integral f64 renders with no decimal point; `{:.0}` is its exact decimal expansion, however
    // large. A non-integer renders via Display's shortest round-trip, which always carries a fraction (never a trailing
    // zero) and, since Rust 1.87, never an exponent. `% 1.0` is the no_std integrality test (`fract` needs libm).
    let rendered = if magnitude % 1.0 == 0.0 {
        alloc::format!("{magnitude:.0}")
    } else {
        alloc::format!("{magnitude}")
    };
    if negative {
        alloc::format!("-{rendered}")
    } else {
        rendered
    }
}

/// The QName XPath 1.0's `name()` returns (§4.1). The projection stores each element's EXPANDED name — `local` for a
/// no-namespace element, `{uri}local` clark for a namespaced one — and the XML codec discards the authored prefix
/// (`jqf-codec/xml/src/value.rs`: prefix spelling is not part of the expanded name), so the QName is exactly
/// recoverable only in the no-namespace case (the local part itself). For a namespaced element `name()` serves the
/// local part — XPath's `local-name()` law — never the clark spelling, which no authored selector can name.
/// Recorded deviation: the authored `prefix:local` spelling is unrecoverable on this seam.
fn qname_of(index: &MarkupIndex, node: jqf_data::NodeId) -> alloc::string::String {
    let name = index.name_of(node);
    match name.split_once('}') {
        Some((_, local)) => local.to_string(),
        None => name.to_string(),
    }
}

/// Resolves one predicate atom against one member of the filtered set.
fn resolve_atom(
    index: &MarkupIndex,
    member: NodeRef,
    position: u64,
    last: u64,
    atom: &ValueAtom,
    budget: SelectorBudget,
    candidates: &mut u64,
) -> Result<AtomValue, SelectorError> {
    match atom {
        ValueAtom::Literal(text) => Ok(AtomValue::Scalar(AtomScalar::Text(text.clone()))),
        ValueAtom::Number(number) => Ok(AtomValue::Scalar(AtomScalar::Number(*number))),
        ValueAtom::Attr(name) => match member {
            NodeRef::Element(node) => match index.attr(node, name) {
                Some(value) => Ok(AtomValue::Scalar(AtomScalar::Text(value.to_string()))),
                None => Ok(AtomValue::Missing),
            },
            NodeRef::Document => Ok(AtomValue::Missing),
        },
        ValueAtom::Text => match member {
            NodeRef::Element(node) => {
                let texts = index
                    .children_of(node)
                    .iter()
                    .filter(|child| !index.is_element(**child) && index.is_text_leaf(**child))
                    .map(|child| index.leaf_text(*child).to_string())
                    .collect();
                Ok(AtomValue::TextValues(texts))
            }
            NodeRef::Document => Ok(AtomValue::TextValues(alloc::vec::Vec::new())),
        },
        ValueAtom::StringValue => match member {
            NodeRef::Element(node) => Ok(AtomValue::Scalar(AtomScalar::Text(index.content_of(node).to_string()))),
            NodeRef::Document => Ok(AtomValue::Scalar(AtomScalar::Text(alloc::string::String::new()))),
        },
        ValueAtom::Name => match member {
            NodeRef::Element(node) => Ok(AtomValue::Scalar(AtomScalar::Text(qname_of(index, node)))),
            NodeRef::Document => Ok(AtomValue::Scalar(AtomScalar::Text(alloc::string::String::new()))),
        },
        ValueAtom::Position => Ok(AtomValue::Scalar(AtomScalar::Number(position as f64))),
        ValueAtom::Last => Ok(AtomValue::Scalar(AtomScalar::Number(last as f64))),
        ValueAtom::Count(path) => {
            // The nested path evaluates against the CALLER's budget and candidate counter: count()'s walks are charged
            // to the shared activation ceiling, never a fresh budget whose counters are discarded.
            let set = eval_path(index, path, member, budget, candidates)?;
            Ok(AtomValue::Scalar(AtomScalar::Number(set.len() as f64)))
        }
        ValueAtom::StringLength(inner) => {
            let inner = resolve_scalar(index, member, position, last, inner, budget, candidates)?;
            let text = inner.as_cow().chars().count() as u64;
            Ok(AtomValue::Scalar(AtomScalar::Number(text as f64)))
        }
        ValueAtom::Concat(atoms) => {
            let mut out = alloc::string::String::new();
            for atom in atoms {
                let resolved = resolve_scalar(index, member, position, last, atom, budget, candidates)?;
                out.push_str(&resolved.as_cow());
            }
            Ok(AtomValue::Scalar(AtomScalar::Text(out)))
        }
    }
}

/// Resolves an atom in a SCALAR position (concat/string-length argument):
/// text() reads its FIRST text child (XPath's node-set string() law), and a missing attribute is the empty string.
fn resolve_scalar(
    index: &MarkupIndex,
    member: NodeRef,
    position: u64,
    last: u64,
    atom: &ValueAtom,
    budget: SelectorBudget,
    candidates: &mut u64,
) -> Result<AtomScalar, SelectorError> {
    match atom {
        ValueAtom::Text => match resolve_atom(index, member, position, last, atom, budget, candidates)? {
            AtomValue::TextValues(texts) => Ok(AtomScalar::Text(texts.into_iter().next().unwrap_or_default())),
            AtomValue::Scalar(scalar) => Ok(scalar),
            AtomValue::Missing => Ok(AtomScalar::Text(alloc::string::String::new())),
        },
        ValueAtom::Attr(_) => match resolve_atom(index, member, position, last, atom, budget, candidates)? {
            AtomValue::Scalar(scalar) => Ok(scalar),
            AtomValue::Missing => Ok(AtomScalar::Text(alloc::string::String::new())),
            AtomValue::TextValues(_) => unreachable!("attr never resolves to text values"),
        },
        other => resolve_atom(index, member, position, last, other, budget, candidates).map(|value| match value {
            AtomValue::Scalar(scalar) => scalar,
            AtomValue::TextValues(texts) => AtomScalar::Text(texts.into_iter().next().unwrap_or_default()),
            AtomValue::Missing => AtomScalar::Text(alloc::string::String::new()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{xpath_number, xpath_number_string};

    #[test]
    fn xpath_number_follows_the_41_grammar() {
        // Valid: optional minus, digits with optional fraction, leading or trailing dot allowed but not both sides
        // empty.
        assert_eq!(xpath_number("1"), 1.0);
        assert_eq!(xpath_number("-1"), -1.0);
        assert_eq!(xpath_number("1.5"), 1.5);
        assert_eq!(xpath_number(".5"), 0.5);
        assert_eq!(xpath_number("5."), 5.0);
        assert_eq!(xpath_number("  -3.25  "), -3.25); // whitespace stripped
        // XPath rejects what Rust's parse accepts: plus sign, exponents, Infinity/NaN spellings, empty, bare ".",
        // double dots.
        for bad in [
            "+1", "1e5", "1E5", "Infinity", "NaN", "", "   ", ".", "1..5", "1.2.3", "abc",
        ] {
            assert!(xpath_number(bad).is_nan(), "expected NaN for {bad:?}");
        }
    }

    #[test]
    fn xpath_number_string_follows_the_44_law() {
        assert_eq!(xpath_number_string(f64::NAN), "NaN");
        assert_eq!(xpath_number_string(f64::INFINITY), "Infinity");
        assert_eq!(xpath_number_string(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(xpath_number_string(0.0), "0");
        assert_eq!(xpath_number_string(-0.0), "0");
        // An integer renders with NO decimal point (§4.4).
        assert_eq!(xpath_number_string(1.0), "1");
        assert_eq!(xpath_number_string(123.0), "123");
        assert_eq!(xpath_number_string(-7.0), "-7");
        assert_eq!(xpath_number_string(1e6), "1000000");
        assert_eq!(xpath_number_string(1e19), "10000000000000000000");
        // A non-integer keeps the point, one digit each side, no trailing zeros.
        assert_eq!(xpath_number_string(-1.5), "-1.5");
        assert_eq!(xpath_number_string(0.1), "0.1");
        assert_eq!(xpath_number_string(123.456), "123.456");
        assert_eq!(xpath_number_string(1e-6), "0.000001");
        // Decimal form at every magnitude; the Number production has no exponent.
        assert_eq!(xpath_number_string(1e-7), "0.0000001");
        assert_eq!(xpath_number_string(1.5e-7), "0.00000015");
        assert_eq!(xpath_number_string(2.5e21), "2500000000000000000000");
    }
}
