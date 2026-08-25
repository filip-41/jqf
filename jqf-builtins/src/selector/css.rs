//! The `html.css@1` selector profile: the static-tree-applicable profile of Selectors Level 4 (§4.10 of
//! `codec-portfolio-design.md`).
//!
//! The profile includes type, universal, ID, class, attribute, namespace, compound/complex/list selectors, the four
//! tree combinators, and the static pseudo-classes `:is()`, `:where()`, `:not()`, `:has()`, the structural `:nth-*`
//! family, `:lang()`, `:dir()`, `:root`, `:empty`, and `:scope`.
//! Pseudo-elements, shadow-tree selectors, visited-link state, user-action / time state, layout-dependent state, and
//! browser-UI state fail compilation.
//! Forgiving selector lists are used only where the grammar requires them (`:is()` / `:where()`); `:not()` and `:has()`
//! are not forgiving.
//!
//! Compilation binds no namespace environment in v1: every `prefix|name` form is an undeclared-prefix compile error,
//! `|name` is the no-namespace spelling, and `*|name` is the any-namespace spelling. The projection has no namespaces,
//! so all three that compile match by name.
//!
//! Evaluation requires the complete recovered document mode in its input authority (the `html.mode@1` document fact)
//! — missing or partial mode authority makes the selector route ineligible. Candidate enumeration is limited to the
//! traversal domain (the scope node and its element descendants); matching has full read-only visibility of the
//! document.
//! `:lang()` follows the pinned HTML language order (nearest `lang`, then `xml:lang`, then the pragma-set default, then
//! external protocol `None`, then unknown — unknown does not match). `:dir()` uses only the pinned HTML
//! element-directionality algorithm over recovered attributes and text.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::SelectorBudget;
use super::dir::Direction;
use super::error::SelectorError;
use super::index::MarkupIndex;

/// One compiled CSS program: a non-forgiving selector list.
#[derive(Debug)]
pub(crate) struct CssPlan {
    selectors: Vec<ComplexSelector>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Combinator {
    Descendant,
    Child,
    AdjacentSibling,
    GeneralSibling,
}

/// One complex selector: the subject compound plus its leftward relations.
///
/// `compounds` are in SOURCE order — `compounds[0]` is the LEFTMOST compound (the one a `:has()` anchor relation
/// connects to) and the subject (the node a match starts from) is `compounds.last()`. `compounds[i]` for `i > 0` is
/// related to `compounds[i - 1]` by `combinators[i - 1]`.
#[derive(Debug)]
struct ComplexSelector {
    compounds: Vec<Compound>,
    combinators: Vec<Combinator>,
}

#[derive(Debug)]
struct Compound {
    type_test: Option<TypeTest>,
    simples: Vec<Simple>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TypeTest {
    /// `*` — any element.
    Any,
    /// `E`, `|E`, or `*|E` — an element named E (the v1 environment's default element namespace is `None`, so every
    /// spelling qualifies by name).
    Name(String),
}

#[derive(Debug)]
enum Simple {
    Id(String),
    Class(String),
    Attr(AttrSelector),
    Pseudo(Pseudo),
}

#[derive(Debug)]
struct AttrSelector {
    name: String,
    op: Option<AttrOp>,
    value: String,
    /// `i`/`I`/`s`/`S` flags; `None` is the HTML default law.
    case: Option<AttrCase>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttrOp {
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttrCase {
    /// The `i` flag: ASCII case-insensitive comparison.
    Insensitive,
    /// The `s` flag: case-sensitive comparison.
    Sensitive,
}

#[derive(Debug)]
enum Pseudo {
    Root,
    Empty,
    Scope,
    /// A forgiving selector list (`:is()` / `:where()` — the same match; the two differ only in specificity, which a
    /// static matcher has no notion of).
    Is(Vec<ComplexSelector>),
    Not(Vec<ComplexSelector>),
    Has(Vec<RelativeSelector>),
    FirstChild,
    LastChild,
    OnlyChild,
    NthChild(Nth, Option<Vec<Compound>>),
    NthLastChild(Nth, Option<Vec<Compound>>),
    FirstOfType,
    LastOfType,
    OnlyOfType,
    NthOfType(Nth),
    NthLastOfType(Nth),
    Lang(Vec<LanguageRange>),
    Dir(Direction),
}

/// One `:has()` argument: a relative selector (a complex with an optional leading combinator relating its leftmost
/// compound to the anchor).
#[derive(Debug)]
struct RelativeSelector {
    combinator: Option<Combinator>,
    complex: ComplexSelector,
}

/// The `:has()` anchor relation: the relative selector's LEFTMOST compound must stand in `combinator` relation to
/// `anchor`. This is the leading combinator of the `RelativeSelector`, never a combinator of the chain itself.
#[derive(Clone, Copy, Debug)]
struct AnchorRelation {
    combinator: Combinator,
    anchor: jqf_data::NodeId,
}

/// The `An+B` microsyntax of the `:nth-*` family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Nth {
    a: i64,
    b: i64,
}

impl Nth {
    /// Whether the position `position` (1-based) is `a*n + b` for some integer `n >= 0`.
    fn admits(self, position: i64) -> bool {
        if self.a == 0 {
            return position == self.b;
        }
        // `position - b` overflows for an extreme authored `b`
        // (`:nth-child(1n-9223372036854775807)` -> b = -i64::MAX): no position can satisfy such a formula, so an
        // overflow is a miss.
        let Some(delta) = position.checked_sub(self.b) else {
            return false;
        };
        if delta % self.a != 0 {
            return false;
        }
        delta / self.a >= 0
    }
}

/// One `:lang()` range: RFC 4647 extended-filtering subtags.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LanguageRange {
    subtags: Vec<RangeSubtag>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RangeSubtag {
    /// `*` — matches exactly one tag subtag.
    Any,
    /// A literal subtag, compared ASCII case-insensitively.
    Literal(String),
}

/// HTML's case-insensitive attribute-values list, pinned from the WHATWG HTML standard (the enumerated/legacy
/// attributes whose values are matched ASCII case-insensitively; Selectors Level 4 §6.3.2 defers the default case law
/// to the host language). Attribute NAMES in the projection are already ASCII-lowercased by the HTML codec.
const HTML_CASE_INSENSITIVE_ATTRIBUTES: &[&str] = &[
    "accept",
    "accept-charset",
    "align",
    "alink",
    "axis",
    "bgcolor",
    "charset",
    "checked",
    "clear",
    "codetype",
    "color",
    "compact",
    "declare",
    "defer",
    "dir",
    "direction",
    "disabled",
    "enctype",
    "face",
    "frame",
    "hreflang",
    "http-equiv",
    "lang",
    "language",
    "link",
    "media",
    "method",
    "multiple",
    "nohref",
    "noresize",
    "noshade",
    "nowrap",
    "readonly",
    "rel",
    "rev",
    "rules",
    "scope",
    "scrolling",
    "selected",
    "shape",
    "target",
    "text",
    "type",
    "valign",
    "valuetype",
    "vlink",
];

/// Compiles one CSS selector list.
pub(crate) fn compile_css(text: &str) -> Result<CssPlan, SelectorError> {
    let mut cursor = Cursor::new(text);
    let mut selectors = Vec::new();
    loop {
        cursor.skip_ws();
        if cursor.peek().is_none() {
            break;
        }
        selectors.push(parse_complex(&mut cursor)?);
        cursor.skip_ws();
        match cursor.peek() {
            None => break,
            Some(b',') => {
                cursor.bump();
            }
            Some(_) => return cursor.error("expected ',' or end of selector"),
        }
    }
    if selectors.is_empty() {
        return Err(SelectorError::Compile {
            message: "empty selector list".to_string(),
            offset: 0,
        });
    }
    Ok(CssPlan { selectors })
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
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n' | 0x0C)) {
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

    /// Parses one identifier (CSS ident: letter, `-`, `_`, digits after the first; kept conservative).
    fn ident(&mut self) -> Result<Option<String>, SelectorError> {
        let start = self.at;
        let mut at = self.at;
        // A leading `-` is allowed when followed by a name character.
        if self.bytes.get(at) == Some(&b'-') && self.bytes.get(at + 1).is_some_and(|b| is_ident_char(*b)) {
            at += 1;
        }
        match self.bytes.get(at) {
            Some(b) if is_ident_start(*b) => {
                at += 1;
            }
            _ => return Ok(None),
        }
        while self.bytes.get(at).is_some_and(|b| is_ident_char(*b)) {
            at += 1;
        }
        self.at = at;
        Ok(Some(self.text[start..at].to_string()))
    }

    /// Parses a string literal (CSS strings: `"..."` / `'...'`). The value is sliced from the source text (never
    /// byte-as-char, so UTF-8 survives) and escapes are decoded per css-syntax-3 §4.3.8: a backslash followed by up to
    /// six hex digits (plus one optional whitespace) is a code point, a backslash followed by a newline is a line
    /// continuation that produces nothing, and any other backslash escapes the next code point literally.
    fn string(&mut self) -> Result<Option<String>, SelectorError> {
        let quote = match self.peek() {
            Some(b @ (b'\'' | b'"')) => b,
            _ => return Ok(None),
        };
        self.at += 1;
        let mut out = String::new();
        let mut segment_start = self.at;
        loop {
            match self.peek() {
                None => return self.error("unterminated string"),
                Some(b'\\') => {
                    out.push_str(&self.text[segment_start..self.at]);
                    self.at += 1;
                    let (decoded, next) = self.decode_escape()?;
                    if let Some(character) = decoded {
                        out.push(character);
                    }
                    self.at = next;
                    segment_start = self.at;
                }
                Some(b) if b == quote => {
                    out.push_str(&self.text[segment_start..self.at]);
                    self.at += 1;
                    break;
                }
                Some(_) => {
                    // Advance one full code point so the segment slices stay on char boundaries. `peek` proved a byte
                    // exists; a `str` slice can still land mid-codepoint if the cursor ever drifts, so fail as a parse
                    // error rather than panicking.
                    let Some(character) = self.text.get(self.at..).and_then(|rest| rest.chars().next()) else {
                        return self.error("unterminated string");
                    };
                    self.at += character.len_utf8();
                }
            }
        }
        Ok(Some(out))
    }

    /// Decodes one CSS escape starting just past the backslash.
    fn decode_escape(&self) -> Result<(Option<char>, usize), SelectorError> {
        let mut hex_end = self.at;
        while hex_end < self.bytes.len() && hex_end - self.at < 6 && self.bytes[hex_end].is_ascii_hexdigit() {
            hex_end += 1;
        }
        if hex_end > self.at {
            // css-syntax-3 §4.3.8: an escape's value is its hex number UNLESS it is zero, a surrogate, or beyond the
            // code-point range — each of those decodes to U+FFFD. The mapping is explicit on every invalid arm; a
            // parse failure cannot actually occur (at most six hex digits always fit u32) but lands in the same
            // replacement rule instead of a second, silent answer.
            let mut next = hex_end;
            if matches!(self.bytes.get(next), Some(b' ' | b'\t' | b'\r' | b'\n' | 0x0C)) {
                next += 1;
            }
            let character = match u32::from_str_radix(&self.text[self.at..hex_end], 16) {
                Ok(value) if value != 0 => char::from_u32(value).unwrap_or('\u{FFFD}'),
                _ => '\u{FFFD}',
            };
            return Ok((Some(character), next));
        }
        let Some(character) = self.text[self.at..].chars().next() else {
            return self.error("unterminated string escape");
        };
        let next = self.at + character.len_utf8();
        if matches!(character, '\n' | '\r' | '\u{000C}') {
            // A backslash-newline is a line continuation: nothing is emitted.
            return Ok((None, next));
        }
        Ok((Some(character), next))
    }
}

fn is_ident_start(b: u8) -> bool {
    // A byte at or above 0x80 is part of a non-ASCII code point: CSS idents admit non-ASCII (css-syntax-3 §4.3.1), and
    // slicing the source text keeps the whole character.
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-') || b >= 0x80
}

/// Parses one complex selector.
fn parse_complex(cursor: &mut Cursor<'_>) -> Result<ComplexSelector, SelectorError> {
    cursor.skip_ws();
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    loop {
        cursor.skip_ws();
        match cursor.peek() {
            None | Some(b',') | Some(b')') => break,
            Some(b'>') | Some(b'+') | Some(b'~') => {
                let c = match cursor.bump() {
                    Some(b'>') => Combinator::Child,
                    Some(b'+') => Combinator::AdjacentSibling,
                    Some(b'~') => Combinator::GeneralSibling,
                    _ => unreachable!(),
                };
                cursor.skip_ws();
                combinators.push(c);
                compounds.push(parse_compound(cursor)?);
            }
            Some(_) => {
                if compounds.is_empty() {
                    compounds.push(parse_compound(cursor)?);
                } else {
                    // Whitespace between compounds is the descendant combinator; the top-of-loop skip consumed it.
                    combinators.push(Combinator::Descendant);
                    compounds.push(parse_compound(cursor)?);
                }
            }
        }
    }
    if compounds.is_empty() {
        return cursor.error("empty compound selector");
    }
    Ok(ComplexSelector { compounds, combinators })
}

fn parse_compound(cursor: &mut Cursor<'_>) -> Result<Compound, SelectorError> {
    cursor.skip_ws();
    let mut type_test = None;
    let mut simples = Vec::new();
    let mut seen_anything = false;

    // Type or universal selector (with namespace forms).
    if cursor.peek() == Some(b'*') && !cursor.starts("*=") {
        // `*` alone, `*|E`, or `*|*`.
        if cursor.bytes.get(cursor.at + 1) == Some(&b'|') {
            cursor.at += 2;
            if cursor.peek() == Some(b'*') {
                cursor.bump();
                type_test = Some(TypeTest::Any);
            } else {
                let name = cursor.ident()?.ok_or_else(|| SelectorError::Compile {
                    message: "expected a type name after '*|'".to_string(),
                    offset: cursor.at,
                })?;
                type_test = Some(TypeTest::Name(name));
            }
        } else {
            cursor.bump();
            type_test = Some(TypeTest::Any);
        }
        seen_anything = true;
    } else if cursor.peek() == Some(b'|') && cursor.bytes.get(cursor.at + 1) != Some(&b'=') {
        // `|E` — the no-namespace spelling.
        cursor.bump();
        if cursor.peek() == Some(b'*') {
            cursor.bump();
            type_test = Some(TypeTest::Any);
        } else {
            let name = cursor.ident()?.ok_or_else(|| SelectorError::Compile {
                message: "expected a type name after '|'".to_string(),
                offset: cursor.at,
            })?;
            type_test = Some(TypeTest::Name(name));
        }
        seen_anything = true;
    } else if let Some(name) = cursor.ident()? {
        if cursor.peek() == Some(b'|') && cursor.bytes.get(cursor.at + 1) != Some(&b'=') {
            // `prefix|E` — undeclared prefix in v1.
            return cursor.error("namespace prefixes need a bound namespace environment; use |E or *|E");
        }
        type_test = Some(TypeTest::Name(name));
        seen_anything = true;
    }

    // A compound never contains whitespace: a `#`/`.`/`[`/`:` seen past whitespace belongs to the NEXT compound
    // (parse_attr and parse_pseudo skip their own interior whitespace).
    loop {
        match cursor.peek() {
            Some(b'#') => {
                cursor.bump();
                let id = cursor.ident()?.ok_or_else(|| SelectorError::Compile {
                    message: "expected an identifier after '#'".to_string(),
                    offset: cursor.at,
                })?;
                simples.push(Simple::Id(id));
                seen_anything = true;
            }
            Some(b'.') if !cursor.starts("..") => {
                cursor.bump();
                let class = cursor.ident()?.ok_or_else(|| SelectorError::Compile {
                    message: "expected an identifier after '.'".to_string(),
                    offset: cursor.at,
                })?;
                simples.push(Simple::Class(class));
                seen_anything = true;
            }
            Some(b'[') => {
                simples.push(Simple::Attr(parse_attr(cursor)?));
                seen_anything = true;
            }
            Some(b':') => {
                simples.push(Simple::Pseudo(parse_pseudo(cursor)?));
                seen_anything = true;
            }
            _ => break,
        }
    }
    if !seen_anything {
        return cursor.error("expected a type, id, class, attribute, or pseudo-class");
    }
    Ok(Compound { type_test, simples })
}

fn parse_attr(cursor: &mut Cursor<'_>) -> Result<AttrSelector, SelectorError> {
    debug_assert_eq!(cursor.peek(), Some(b'['));
    cursor.bump();
    cursor.skip_ws();
    let name = cursor.ident()?.ok_or_else(|| SelectorError::Compile {
        message: "expected an attribute name".to_string(),
        offset: cursor.at,
    })?;
    cursor.skip_ws();
    let mut op = None;
    let mut value = String::new();
    let mut case = None;
    if cursor.peek() == Some(b']') {
        // Presence selector.
    } else {
        op = Some(match cursor.peek() {
            Some(b'=') => {
                cursor.bump();
                AttrOp::Equals
            }
            Some(b'~') => {
                cursor.bump();
                expect_char(cursor, b'=')?;
                AttrOp::Includes
            }
            Some(b'|') => {
                cursor.bump();
                expect_char(cursor, b'=')?;
                AttrOp::DashMatch
            }
            Some(b'^') => {
                cursor.bump();
                expect_char(cursor, b'=')?;
                AttrOp::Prefix
            }
            Some(b'$') => {
                cursor.bump();
                expect_char(cursor, b'=')?;
                AttrOp::Suffix
            }
            Some(b'*') => {
                cursor.bump();
                expect_char(cursor, b'=')?;
                AttrOp::Substring
            }
            _ => return cursor.error("expected an attribute operator or ']'"),
        });
        cursor.skip_ws();
        let ident_value = cursor.ident()?;
        if let Some(ident_value) = ident_value {
            value = ident_value;
        } else if let Some(string_value) = cursor.string()? {
            value = string_value;
        } else {
            return cursor.error("expected an attribute value");
        }
        cursor.skip_ws();
        if matches!(cursor.peek(), Some(b'i' | b'I' | b's' | b'S')) {
            case = Some(match cursor.peek() {
                Some(b'i' | b'I') => AttrCase::Insensitive,
                _ => AttrCase::Sensitive,
            });
            cursor.bump();
            cursor.skip_ws();
        }
    }
    if cursor.peek() != Some(b']') {
        return cursor.error("expected ']' to close the attribute selector");
    }
    cursor.bump();
    Ok(AttrSelector { name, op, value, case })
}

fn expect_char(cursor: &mut Cursor<'_>, expected: u8) -> Result<(), SelectorError> {
    if cursor.bump() != Some(expected) {
        return cursor.error("malformed attribute operator");
    }
    Ok(())
}

fn parse_pseudo(cursor: &mut Cursor<'_>) -> Result<Pseudo, SelectorError> {
    debug_assert_eq!(cursor.peek(), Some(b':'));
    cursor.bump();
    if cursor.peek() == Some(b':') {
        return cursor.error("pseudo-elements are outside the static-tree profile");
    }
    let name = cursor.ident()?.ok_or_else(|| SelectorError::Compile {
        message: "expected a pseudo-class name after ':'".to_string(),
        offset: cursor.at,
    })?;
    cursor.skip_ws();
    let has_args = cursor.peek() == Some(b'(');
    match name.as_str() {
        "root" if !has_args => Ok(Pseudo::Root),
        "empty" if !has_args => Ok(Pseudo::Empty),
        "scope" if !has_args => Ok(Pseudo::Scope),
        "first-child" if !has_args => Ok(Pseudo::FirstChild),
        "last-child" if !has_args => Ok(Pseudo::LastChild),
        "only-child" if !has_args => Ok(Pseudo::OnlyChild),
        "first-of-type" if !has_args => Ok(Pseudo::FirstOfType),
        "last-of-type" if !has_args => Ok(Pseudo::LastOfType),
        "only-of-type" if !has_args => Ok(Pseudo::OnlyOfType),
        "is" if has_args => {
            let members = parse_forgiving_list(cursor)?;
            Ok(Pseudo::Is(members))
        }
        "where" if has_args => {
            // `:where()` matches exactly as `:is()` under the static matcher.
            let members = parse_forgiving_list(cursor)?;
            Ok(Pseudo::Is(members))
        }
        "not" if has_args => {
            let members = parse_non_forgiving_list(cursor)?;
            Ok(Pseudo::Not(members))
        }
        "has" if has_args => {
            cursor.bump();
            cursor.skip_ws();
            let mut relatives = Vec::new();
            loop {
                cursor.skip_ws();
                let combinator = match cursor.peek() {
                    Some(b'>') => {
                        cursor.bump();
                        cursor.skip_ws();
                        Some(Combinator::Child)
                    }
                    Some(b'+') => {
                        cursor.bump();
                        cursor.skip_ws();
                        Some(Combinator::AdjacentSibling)
                    }
                    Some(b'~') => {
                        cursor.bump();
                        cursor.skip_ws();
                        Some(Combinator::GeneralSibling)
                    }
                    _ => None,
                };
                let complex = parse_complex(cursor)?;
                relatives.push(RelativeSelector { combinator, complex });
                cursor.skip_ws();
                match cursor.peek() {
                    Some(b',') => {
                        cursor.bump();
                    }
                    Some(b')') => break,
                    _ => return cursor.error("expected ',' or ')' in :has()"),
                }
            }
            cursor.bump();
            if relatives.is_empty() {
                return cursor.error(":has() requires at least one relative selector");
            }
            Ok(Pseudo::Has(relatives))
        }
        "nth-child" if has_args => parse_nth_pseudo(cursor, false),
        "nth-last-child" if has_args => parse_nth_pseudo(cursor, true),
        "nth-of-type" if has_args => parse_nth_type(cursor, false),
        "nth-last-of-type" if has_args => parse_nth_type(cursor, true),
        "lang" if has_args => {
            cursor.bump();
            cursor.skip_ws();
            let mut ranges = Vec::new();
            loop {
                cursor.skip_ws();
                ranges.push(parse_language_range(cursor)?);
                cursor.skip_ws();
                match cursor.peek() {
                    Some(b',') => {
                        cursor.bump();
                    }
                    Some(b')') => break,
                    _ => return cursor.error("expected ',' or ')' in :lang()"),
                }
            }
            cursor.bump();
            if ranges.is_empty() {
                return cursor.error(":lang() requires at least one language range");
            }
            Ok(Pseudo::Lang(ranges))
        }
        "dir" if has_args => {
            cursor.bump();
            cursor.skip_ws();
            let direction = match cursor.ident()?.as_deref() {
                Some("ltr") => Direction::Ltr,
                Some("rtl") => Direction::Rtl,
                _ => return cursor.error(":dir() takes exactly 'ltr' or 'rtl'"),
            };
            cursor.skip_ws();
            if cursor.bump() != Some(b')') {
                return cursor.error("expected ')' in :dir()");
            }
            Ok(Pseudo::Dir(direction))
        }
        _ => {
            if has_args {
                Err(SelectorError::Compile {
                    message: format!("unknown functional pseudo-class ':{name}()' is outside the static profile"),
                    offset: cursor.at,
                })
            } else {
                Err(SelectorError::Compile {
                    message: format!("unknown pseudo-class ':{name}' is outside the static profile"),
                    offset: cursor.at,
                })
            }
        }
    }
}

/// Parses one `:nth-child(An+B [of S])` argument (the child forms can carry the `of <compound-selector-list>` clause;
/// the of-type forms cannot).
fn parse_nth_pseudo(cursor: &mut Cursor<'_>, last: bool) -> Result<Pseudo, SelectorError> {
    cursor.bump();
    cursor.skip_ws();
    let nth = parse_nth(cursor)?;
    cursor.skip_ws();
    let mut of = None;
    if cursor.starts("of") {
        // The `of` keyword is a CSS ident boundary, not a prefix of a name.
        let after = cursor.at + 2;
        if matches!(
            cursor.bytes.get(after),
            None | Some(b' ' | b'\t' | b'\r' | b'\n' | 0x0C | b',' | b')' | b'[' | b'.' | b'#' | b':')
        ) {
            cursor.take("of");
            cursor.skip_ws();
            let mut compounds = Vec::new();
            loop {
                cursor.skip_ws();
                compounds.push(parse_compound(cursor)?);
                cursor.skip_ws();
                match cursor.peek() {
                    Some(b',') => {
                        cursor.bump();
                    }
                    _ => break,
                }
            }
            of = Some(compounds);
        }
    }
    cursor.skip_ws();
    if cursor.bump() != Some(b')') {
        return cursor.error("expected ')' in :nth-child()");
    }
    if last {
        Ok(Pseudo::NthLastChild(nth, of))
    } else {
        Ok(Pseudo::NthChild(nth, of))
    }
}

fn parse_nth_type(cursor: &mut Cursor<'_>, last: bool) -> Result<Pseudo, SelectorError> {
    cursor.bump();
    cursor.skip_ws();
    let nth = parse_nth(cursor)?;
    cursor.skip_ws();
    if cursor.bump() != Some(b')') {
        return cursor.error("expected ')' in :nth-of-type()");
    }
    if last {
        Ok(Pseudo::NthLastOfType(nth))
    } else {
        Ok(Pseudo::NthOfType(nth))
    }
}

/// Parses the `An+B` microsyntax.
fn parse_nth(cursor: &mut Cursor<'_>) -> Result<Nth, SelectorError> {
    cursor.skip_ws();
    // `odd` / `even`.
    if cursor.starts("odd") && is_boundary(cursor.bytes, cursor.at + 3) {
        cursor.take("odd");
        return Ok(Nth { a: 2, b: 1 });
    }
    if cursor.starts("even") && is_boundary(cursor.bytes, cursor.at + 4) {
        cursor.take("even");
        return Ok(Nth { a: 2, b: 0 });
    }
    let start = cursor.at;
    let mut a = 0i64;
    let mut b = 0i64;
    // Optional sign.
    let mut sign = 1i64;
    if matches!(cursor.peek(), Some(b'+' | b'-')) {
        if cursor.peek() == Some(b'-') {
            sign = -1;
        }
        cursor.bump();
    }
    // `n` with an optional coefficient, or a bare integer.
    let mut saw_n = false;
    if matches!(cursor.peek(), Some(b'0'..=b'9')) {
        let digits = take_digits(cursor);
        let coefficient = digits.parse::<i64>().map_err(|_| SelectorError::Compile {
            message: "An+B coefficient out of range".to_string(),
            offset: start,
        })?;
        if cursor.peek() == Some(b'n') {
            cursor.bump();
            a = sign * coefficient;
            saw_n = true;
        } else {
            // A bare integer may be negative: An+B admits `<integer>`, and a negative `b` is valid and simply matches
            // no position.
            b = sign * coefficient;
        }
    } else if cursor.peek() == Some(b'n') {
        cursor.bump();
        a = sign;
        saw_n = true;
    } else {
        return cursor.error("expected 'odd', 'even', or An+B");
    }
    if saw_n {
        // Optional `+B` / `-B` tail.
        cursor.skip_ws();
        match cursor.peek() {
            Some(b'+') => {
                cursor.bump();
                cursor.skip_ws();
                b = take_digits(cursor).parse::<i64>().map_err(|_| SelectorError::Compile {
                    message: "An+B constant out of range".to_string(),
                    offset: start,
                })?;
            }
            Some(b'-') => {
                cursor.bump();
                cursor.skip_ws();
                b = -take_digits(cursor).parse::<i64>().map_err(|_| SelectorError::Compile {
                    message: "An+B constant out of range".to_string(),
                    offset: start,
                })?;
            }
            _ => {}
        }
    }
    Ok(Nth { a, b })
}

fn take_digits(cursor: &mut Cursor<'_>) -> String {
    let start = cursor.at;
    while matches!(cursor.peek(), Some(b'0'..=b'9')) {
        cursor.at += 1;
    }
    cursor.text[start..cursor.at].to_string()
}

fn is_boundary(bytes: &[u8], at: usize) -> bool {
    matches!(
        bytes.get(at),
        None | Some(b' ' | b'\t' | b'\r' | b'\n' | 0x0C | b',' | b')' | b'[' | b'#' | b'.' | b':')
    )
}

/// Parses one `:lang()` language range.
fn parse_language_range(cursor: &mut Cursor<'_>) -> Result<LanguageRange, SelectorError> {
    cursor.skip_ws();
    let mut subtags = Vec::new();
    loop {
        cursor.skip_ws();
        if cursor.peek() == Some(b'*') {
            cursor.bump();
            subtags.push(RangeSubtag::Any);
        } else {
            let start = cursor.at;
            while matches!(cursor.peek(), Some(b) if b.is_ascii_alphanumeric()) {
                cursor.at += 1;
            }
            if cursor.at == start {
                return cursor.error("expected a language subtag or '*'");
            }
            subtags.push(RangeSubtag::Literal(cursor.text[start..cursor.at].to_string()));
        }
        cursor.skip_ws();
        if cursor.peek() == Some(b'-') {
            cursor.bump();
        } else {
            break;
        }
    }
    Ok(LanguageRange { subtags })
}

/// Parses a forgiving selector list (`:is()` / `:where()`): invalid members are dropped, never compile errors; an empty
/// surviving list matches nothing.
fn parse_forgiving_list(cursor: &mut Cursor<'_>) -> Result<Vec<ComplexSelector>, SelectorError> {
    debug_assert_eq!(cursor.peek(), Some(b'('));
    cursor.bump();
    let mut members = Vec::new();
    loop {
        cursor.skip_ws();
        if cursor.peek() == Some(b')') {
            cursor.bump();
            break;
        }
        // Save the cursor position for recovery.
        let saved = cursor.at;
        match parse_complex(cursor) {
            Ok(complex) => members.push(complex),
            Err(_) => {
                // Drop the invalid member and skip to the next ',' or ')' at this paren depth, counting nested
                // brackets/parens. The recovery stops BEFORE the ',' or ')' (which the outer loop consumes) and never
                // fails: an unterminated list is the caller's compile error to report.
                let mut depth = 0i64;
                cursor.at = saved;
                loop {
                    match cursor.peek() {
                        None => break,
                        Some(b'(') => {
                            cursor.bump();
                            depth += 1;
                        }
                        Some(b')') => {
                            if depth == 0 {
                                break;
                            }
                            cursor.bump();
                            depth -= 1;
                        }
                        Some(b'[') => {
                            cursor.bump();
                            depth += 1;
                        }
                        Some(b']') => {
                            // A stray `]` at the list's own depth is junk, not a bracket to balance: the decrement has
                            // a floor at zero, or recovery would run past the `,` and `)`.
                            cursor.bump();
                            if depth > 0 {
                                depth -= 1;
                            }
                        }
                        Some(b',') if depth == 0 => break,
                        Some(_) => {
                            cursor.bump();
                        }
                    }
                }
            }
        }
        cursor.skip_ws();
        match cursor.peek() {
            Some(b',') => {
                cursor.bump();
            }
            Some(b')') => {
                cursor.bump();
                break;
            }
            _ => return cursor.error("expected ',' or ')' in the selector list"),
        }
    }
    Ok(members)
}

/// Parses a non-forgiving selector list (`:not()`): any invalid member fails the whole compile.
fn parse_non_forgiving_list(cursor: &mut Cursor<'_>) -> Result<Vec<ComplexSelector>, SelectorError> {
    debug_assert_eq!(cursor.peek(), Some(b'('));
    cursor.bump();
    let mut members = Vec::new();
    loop {
        cursor.skip_ws();
        if cursor.peek() == Some(b')') && members.is_empty() {
            return cursor.error(":not() requires at least one selector");
        }
        members.push(parse_complex(cursor)?);
        cursor.skip_ws();
        match cursor.peek() {
            Some(b',') => {
                cursor.bump();
            }
            Some(b')') => {
                cursor.bump();
                break;
            }
            _ => return cursor.error("expected ',' or ')' in :not()"),
        }
    }
    Ok(members)
}

/// The evaluation context: budgets plus the scope node.
struct EvalContext {
    budget: SelectorBudget,
    candidates: u64,
    walk: u64,
    scope: jqf_data::NodeId,
}

impl EvalContext {
    fn charge_candidate(&mut self) -> Result<(), SelectorError> {
        self.candidates = self.candidates.checked_add(1).ok_or_else(|| SelectorError::Budget {
            what: "candidate tests",
        })?;
        if self.candidates > self.budget.max_candidate_tests {
            return Err(SelectorError::Budget {
                what: "candidate tests",
            });
        }
        Ok(())
    }

    fn charge_walk(&mut self) -> Result<(), SelectorError> {
        self.walk = self
            .walk
            .checked_add(1)
            .ok_or_else(|| SelectorError::Budget { what: "walk steps" })?;
        if self.walk > self.budget.max_walk_steps {
            return Err(SelectorError::Budget { what: "walk steps" });
        }
        Ok(())
    }
}

/// Evaluates one compiled CSS program over the index.
pub(crate) fn evaluate(
    index: &MarkupIndex,
    scope: jqf_data::NodeId,
    plan: &CssPlan,
    budget: SelectorBudget,
) -> Result<Vec<jqf_data::NodeId>, SelectorError> {
    // The mode authority is REQUIRED: html.css@1 evaluates every mode- sensitive rule against the immutable recovered
    // document mode, and a document without that authority makes the selector route ineligible.
    if index.mode.is_none() {
        return Err(SelectorError::MissingModeAuthority);
    }
    let mut ctx = EvalContext {
        budget,
        candidates: 0,
        walk: 0,
        scope,
    };
    let mut results = Vec::new();
    // Candidates: the scope (when an element) and its element descendants in document order.
    let mut stack: Vec<jqf_data::NodeId> = Vec::new();
    if index.is_element(scope) {
        stack.push(scope);
    }
    while let Some(candidate) = stack.pop() {
        let mut matched = false;
        for selector in &plan.selectors {
            ctx.charge_candidate()?;
            if matches_complex(index, &mut ctx, candidate, selector, None)? {
                matched = true;
                break;
            }
        }
        if matched {
            results.push(candidate);
            if results.len() as u64 > budget.max_results {
                return Err(SelectorError::Budget { what: "results" });
            }
        }
        // Push element children in reverse for document order.
        for child in index.children_of(candidate).iter().rev() {
            if index.is_element(*child) {
                stack.push(*child);
            }
        }
    }
    Ok(results)
}

/// Matches one complex selector with its subject at `node`. `anchor` is `Some` only inside `:has()`: the relative
/// selector's leftmost relation must resolve to the anchor element.
fn matches_complex(
    index: &MarkupIndex,
    ctx: &mut EvalContext,
    node: jqf_data::NodeId,
    complex: &ComplexSelector,
    anchor: Option<AnchorRelation>,
) -> Result<bool, SelectorError> {
    let subject = complex.compounds.last().ok_or_else(|| SelectorError::Internal {
        contract: "empty complex selector",
    })?;
    if !compound_matches(index, ctx, node, subject)? {
        return Ok(false);
    }
    // Right-to-left match with a candidate set per compound: every node that can stand at each chain position, so a
    // stricter combinator to the left re-considers FARTHER candidates instead of committing to the nearest one. The
    // sets are bounded by the walk and candidate budgets.
    let mut reachable: Vec<jqf_data::NodeId> = alloc::vec![node];
    for step in (0..complex.combinators.len()).rev() {
        let combinator = complex.combinators[step];
        let compound = &complex.compounds[step];
        let mut next = Vec::new();
        for current in reachable.drain(..) {
            next.extend(related_candidates(index, ctx, current, combinator, compound)?);
        }
        if next.is_empty() {
            return Ok(false);
        }
        reachable = next;
    }
    match anchor {
        // The leftmost relation connects the leftmost compound to the anchor. A single-compound relative has no chain
        // to check; the anchor relation is still verified against the subject.
        Some(relation) => {
            for leftmost in reachable {
                if relation_holds(index, ctx, leftmost, relation.combinator, relation.anchor)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        None => Ok(true),
    }
}

/// Every node related to `current` by `combinator` that matches `compound`, in match order (nearest first for the
/// many-candidate relations).
fn related_candidates(
    index: &MarkupIndex,
    ctx: &mut EvalContext,
    current: jqf_data::NodeId,
    combinator: Combinator,
    compound: &Compound,
) -> Result<Vec<jqf_data::NodeId>, SelectorError> {
    let mut candidates = Vec::new();
    match combinator {
        Combinator::Child => {
            ctx.charge_walk()?;
            if let Some(parent) = index.parent_of(current) {
                if compound_matches(index, ctx, parent, compound)? {
                    candidates.push(parent);
                }
            }
        }
        Combinator::Descendant => {
            let mut ancestor = index.parent_of(current);
            while let Some(cursor) = ancestor {
                ctx.charge_walk()?;
                if compound_matches(index, ctx, cursor, compound)? {
                    candidates.push(cursor);
                }
                ancestor = index.parent_of(cursor);
            }
        }
        Combinator::AdjacentSibling => {
            ctx.charge_walk()?;
            if let Some(previous) = previous_element_sibling(index, current) {
                if compound_matches(index, ctx, previous, compound)? {
                    candidates.push(previous);
                }
            }
        }
        Combinator::GeneralSibling => {
            let mut sibling = previous_element_sibling(index, current);
            while let Some(cursor) = sibling {
                ctx.charge_walk()?;
                if compound_matches(index, ctx, cursor, compound)? {
                    candidates.push(cursor);
                }
                sibling = previous_element_sibling(index, cursor);
            }
        }
    }
    Ok(candidates)
}

/// Whether `related` stands in `combinator` relation TO `anchor` (used for the `:has()` leftmost relation: `related` is
/// the leftmost compound node).
fn relation_holds(
    index: &MarkupIndex,
    ctx: &mut EvalContext,
    related: jqf_data::NodeId,
    combinator: Combinator,
    anchor: jqf_data::NodeId,
) -> Result<bool, SelectorError> {
    match combinator {
        Combinator::Child => Ok(index.parent_of(related) == Some(anchor)),
        Combinator::Descendant => {
            // The anchor is a proper ancestor of `related`.
            let mut ancestor = index.parent_of(related);
            while let Some(cursor) = ancestor {
                ctx.charge_walk()?;
                if cursor == anchor {
                    return Ok(true);
                }
                ancestor = index.parent_of(cursor);
            }
            Ok(false)
        }
        Combinator::AdjacentSibling => Ok(previous_element_sibling(index, related) == Some(anchor)),
        Combinator::GeneralSibling => {
            let mut sibling = previous_element_sibling(index, related);
            while let Some(cursor) = sibling {
                ctx.charge_walk()?;
                if cursor == anchor {
                    return Ok(true);
                }
                sibling = previous_element_sibling(index, cursor);
            }
            Ok(false)
        }
    }
}

/// The immediately preceding ELEMENT sibling, or none.
fn previous_element_sibling(index: &MarkupIndex, node: jqf_data::NodeId) -> Option<jqf_data::NodeId> {
    let parent = index.parent_of(node)?;
    let position = index.sibling_position(node);
    let children = index.children_of(parent);
    debug_assert!(children.get(position) == Some(&node), "sibling ordinal cache");
    children[..position]
        .iter()
        .rev()
        .copied()
        .find(|child| index.is_element(*child))
}

/// The immediately following ELEMENT sibling, or none.
fn next_element_sibling(index: &MarkupIndex, node: jqf_data::NodeId) -> Option<jqf_data::NodeId> {
    let parent = index.parent_of(node)?;
    let position = index.sibling_position(node);
    let children = index.children_of(parent);
    debug_assert!(children.get(position) == Some(&node), "sibling ordinal cache");
    children[position + 1..]
        .iter()
        .copied()
        .find(|child| index.is_element(*child))
}

/// Whether one compound matches one element.
fn compound_matches(
    index: &MarkupIndex,
    ctx: &mut EvalContext,
    node: jqf_data::NodeId,
    compound: &Compound,
) -> Result<bool, SelectorError> {
    if let Some(type_test) = &compound.type_test {
        match type_test {
            TypeTest::Any => {}
            TypeTest::Name(name) => {
                if index.name_of(node) != name {
                    return Ok(false);
                }
            }
        }
    }
    for simple in &compound.simples {
        if !simple_matches(index, ctx, node, simple)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn simple_matches(
    index: &MarkupIndex,
    ctx: &mut EvalContext,
    node: jqf_data::NodeId,
    simple: &Simple,
) -> Result<bool, SelectorError> {
    match simple {
        Simple::Id(id) => {
            let Some(value) = index.attr(node, "id") else {
                return Ok(false);
            };
            // HTML's host law: id matching is ASCII case-insensitive in quirks mode and case-sensitive in the standards
            // modes.
            Ok(if index.mode.as_deref() == Some("quirks") {
                value.eq_ignore_ascii_case(id)
            } else {
                value == id
            })
        }
        Simple::Class(class) => {
            let Some(value) = index.attr(node, "class") else {
                return Ok(false);
            };
            let token_matches = |token: &str| {
                if index.mode.as_deref() == Some("quirks") {
                    token.eq_ignore_ascii_case(class)
                } else {
                    token == class
                }
            };
            Ok(value.split_ascii_whitespace().any(token_matches))
        }
        Simple::Attr(attr) => attr_matches(index, node, attr),
        Simple::Pseudo(pseudo) => pseudo_matches(index, ctx, node, pseudo),
    }
}

fn attr_matches(index: &MarkupIndex, node: jqf_data::NodeId, attr: &AttrSelector) -> Result<bool, SelectorError> {
    // Attribute NAMES are stored ASCII-lowercased by the HTML codec; the selector's name is normalized the same way.
    let name = attr.name.to_ascii_lowercase();
    let Some(actual) = index.attr(node, &name) else {
        return Ok(false);
    };
    let Some(op) = attr.op else {
        return Ok(true);
    };
    let case_sensitive = match attr.case {
        Some(AttrCase::Insensitive) => false,
        Some(AttrCase::Sensitive) => true,
        None => !HTML_CASE_INSENSITIVE_ATTRIBUTES.contains(&name.as_str()),
    };
    let equals = |left: &str, right: &str| {
        if case_sensitive {
            left == right
        } else {
            left.eq_ignore_ascii_case(right)
        }
    };
    let value = attr.value.as_str();
    let matched = match op {
        AttrOp::Equals => equals(actual, value),
        AttrOp::Includes => actual.split_ascii_whitespace().any(|token| equals(token, value)),
        // The dash-match compares the value through the SAME case law as `=` (the `i`/`s` flags and the HTML
        // case-insensitive list), with the `-` boundary checked on the attribute's own bytes — byte slices, never
        // `str` slicing, so a non-ASCII value cannot land on a non-char boundary.
        AttrOp::DashMatch => {
            let actual_bytes = actual.as_bytes();
            let value_bytes = value.as_bytes();
            let prefix_matches = if case_sensitive {
                actual_bytes.starts_with(value_bytes)
            } else {
                actual_bytes.len() >= value_bytes.len()
                    && actual_bytes[..value_bytes.len()].eq_ignore_ascii_case(value_bytes)
            };
            prefix_matches
                && (actual_bytes.len() == value_bytes.len() || actual_bytes.get(value_bytes.len()) == Some(&b'-'))
        }
        AttrOp::Prefix => {
            if case_sensitive {
                actual.starts_with(value)
            } else {
                let actual_bytes = actual.as_bytes();
                let value_bytes = value.as_bytes();
                actual_bytes.len() >= value_bytes.len()
                    && actual_bytes[..value_bytes.len()].eq_ignore_ascii_case(value_bytes)
            }
        }
        AttrOp::Suffix => {
            if case_sensitive {
                actual.ends_with(value)
            } else {
                let actual_bytes = actual.as_bytes();
                let value_bytes = value.as_bytes();
                actual_bytes.len() >= value_bytes.len()
                    && actual_bytes[actual_bytes.len() - value_bytes.len()..].eq_ignore_ascii_case(value_bytes)
            }
        }
        AttrOp::Substring => {
            if case_sensitive {
                actual.contains(value)
            } else {
                let lower_actual = actual.to_ascii_lowercase();
                let lower_value = value.to_ascii_lowercase();
                lower_actual.contains(&lower_value)
            }
        }
    };
    Ok(matched)
}

fn pseudo_matches(
    index: &MarkupIndex,
    ctx: &mut EvalContext,
    node: jqf_data::NodeId,
    pseudo: &Pseudo,
) -> Result<bool, SelectorError> {
    match pseudo {
        Pseudo::Root => Ok(node == index.document_element),
        // Empty per Selectors Level 4: no element child and no text leaf with data — a zero-length text leaf is not
        // content.
        Pseudo::Empty => Ok(index
            .children_of(node)
            .iter()
            .all(|child| index.is_text_leaf(*child) && index.leaf_text(*child).is_empty())),
        Pseudo::Scope => Ok(node == ctx.scope),
        Pseudo::Is(members) => {
            for member in members {
                ctx.charge_candidate()?;
                if matches_complex(index, ctx, node, member, None)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Pseudo::Not(members) => {
            for member in members {
                ctx.charge_candidate()?;
                if matches_complex(index, ctx, node, member, None)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Pseudo::Has(relatives) => {
            for relative in relatives {
                if relative_matches(index, ctx, node, relative)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Pseudo::FirstChild => Ok(previous_element_sibling(index, node).is_none() && index.parent_of(node).is_some()),
        Pseudo::LastChild => Ok(next_element_sibling(index, node).is_none() && index.parent_of(node).is_some()),
        Pseudo::OnlyChild => {
            let Some(parent) = index.parent_of(node) else {
                return Ok(false);
            };
            // The parent's cached ELEMENT-child count, not a rescan of its children vector.
            Ok(index.element_children_of(parent) == 1)
        }
        Pseudo::NthChild(nth, of) => {
            let Some(position) = element_position(index, ctx, node, true, of.as_deref(), None)? else {
                return Ok(false);
            };
            Ok(nth.admits(position as i64))
        }
        Pseudo::NthLastChild(nth, of) => {
            let Some(position) = element_position(index, ctx, node, false, of.as_deref(), None)? else {
                return Ok(false);
            };
            Ok(nth.admits(position as i64))
        }
        Pseudo::FirstOfType => {
            let Some(position) = element_position(index, ctx, node, true, None, Some(index.name_of(node)))? else {
                return Ok(false);
            };
            Ok(position == 1)
        }
        Pseudo::LastOfType => {
            let Some(position) = element_position(index, ctx, node, false, None, Some(index.name_of(node)))? else {
                return Ok(false);
            };
            Ok(position == 1)
        }
        Pseudo::OnlyOfType => {
            let Some(parent) = index.parent_of(node) else {
                return Ok(false);
            };
            let name = index.name_of(node);
            let mut count = 0usize;
            for sibling in index.children_of(parent) {
                if index.is_element(*sibling) && index.name_of(*sibling) == name {
                    count += 1;
                }
            }
            Ok(count == 1)
        }
        Pseudo::NthOfType(nth) => {
            let Some(position) = element_position(index, ctx, node, true, None, Some(index.name_of(node)))? else {
                return Ok(false);
            };
            Ok(nth.admits(position as i64))
        }
        Pseudo::NthLastOfType(nth) => {
            let Some(position) = element_position(index, ctx, node, false, None, Some(index.name_of(node)))? else {
                return Ok(false);
            };
            Ok(nth.admits(position as i64))
        }
        Pseudo::Lang(ranges) => {
            let Some(language) = element_language(index, node) else {
                // Unknown does not match, not even `*`.
                return Ok(false);
            };
            let tag: Vec<&str> = language.split('-').collect();
            Ok(ranges.iter().any(|range| lang_range_matches(range, &tag)))
        }
        Pseudo::Dir(direction) => Ok(super::dir::directionality(index, node) == *direction),
    }
}

/// One `:has()` relative selector against the anchor `node`.
///
/// The subject — the relative selector's RIGHTMOST compound — may sit arbitrarily deep, so the candidate domain is
/// every element the leading relation can reach (the anchor's descendants for the implicit and child forms, the
/// following siblings' subtrees for the sibling forms), and each candidate is matched with the anchor relation
/// attached.
fn relative_matches(
    index: &MarkupIndex,
    ctx: &mut EvalContext,
    anchor: jqf_data::NodeId,
    relative: &RelativeSelector,
) -> Result<bool, SelectorError> {
    let relation = AnchorRelation {
        // A missing leading combinator is the implicit descendant relation.
        combinator: relative.combinator.unwrap_or(Combinator::Descendant),
        anchor,
    };
    let mut stack: Vec<jqf_data::NodeId> = Vec::new();
    match relative.combinator {
        None | Some(Combinator::Child) => {
            for child in index.children_of(anchor).iter().rev() {
                if index.is_element(*child) {
                    stack.push(*child);
                }
            }
        }
        Some(Combinator::AdjacentSibling) => {
            if let Some(sibling) = next_element_sibling(index, anchor) {
                stack.push(sibling);
            }
        }
        Some(Combinator::GeneralSibling) => {
            let mut sibling = next_element_sibling(index, anchor);
            while let Some(cursor) = sibling {
                stack.push(cursor);
                sibling = next_element_sibling(index, cursor);
            }
        }
        Some(Combinator::Descendant) => {
            // A leading descendant combinator is the implicit form's spelling (`:has(div)`), never written out.
            unreachable!("descendant is the implicit :has() relation")
        }
    }
    while let Some(candidate) = stack.pop() {
        ctx.charge_walk()?;
        if matches_complex(index, ctx, candidate, &relative.complex, Some(relation))? {
            return Ok(true);
        }
        for child in index.children_of(candidate).iter().rev() {
            if index.is_element(*child) {
                stack.push(*child);
            }
        }
    }
    Ok(false)
}

/// The 1-based position of `node` among its siblings, counting from the start (`from_start`) or the end, optionally
/// restricted to siblings matching every compound of the `of` clause AND (for the of-type family) to siblings with the
/// same element name. `None` when the node has no parent element (the document element has no sibling set).
fn element_position(
    index: &MarkupIndex,
    ctx: &mut EvalContext,
    node: jqf_data::NodeId,
    from_start: bool,
    of: Option<&[Compound]>,
    name: Option<&str>,
) -> Result<Option<usize>, SelectorError> {
    let Some(parent) = index.parent_of(node) else {
        // The document element has no sibling set; no structural pseudo-class matches it.
        return Ok(None);
    };
    // Selectors 4 §14: with an `of` clause, the subject itself must match every listed compound — its position is
    // counted only among the S-filtered sibling set, so a subject outside S never matches, whatever its position. One
    // candidate charge per compound, the same budget discipline the sibling loop and `:is()`/`:not()` use.
    if let Some(compounds) = of {
        for compound in compounds {
            ctx.charge_candidate()?;
            if !compound_matches(index, ctx, node, compound)? {
                return Ok(None);
            }
        }
    }
    if of.is_none() && name.is_none() {
        // No clause and no of-type name restricts the counted set, so every ELEMENT sibling counts — exactly what the
        // general loop below accumulates. The cached sibling ordinals answer directly; like the loop they replace, this
        // branch performs no charges.
        let before = index.element_siblings_before(node);
        return Ok(before.map(|before| {
            let before = before as usize;
            if from_start {
                before + 1
            } else {
                let after = index.element_children_of(parent) as usize - before - 1;
                after + 1
            }
        }));
    }
    let mut matched_before = 0usize;
    let mut matched_after = 0usize;
    let mut saw_self = false;
    for sibling in index.children_of(parent) {
        if !index.is_element(*sibling) {
            continue;
        }
        if *sibling == node {
            saw_self = true;
            continue;
        }
        // The `of` clause and the of-type name filter both restrict the counted sibling set. Every charge propagates:
        // an exhausted budget raises rather than silently computing a wrong position.
        let mut in_clause = true;
        if let Some(compounds) = of {
            for compound in compounds {
                ctx.charge_candidate()?;
                if !compound_matches(index, ctx, *sibling, compound)? {
                    in_clause = false;
                    break;
                }
            }
        }
        if in_clause {
            if let Some(expected) = name {
                if index.name_of(*sibling) != expected {
                    in_clause = false;
                }
            }
        }
        if !in_clause {
            continue;
        }
        if !saw_self {
            matched_before += 1;
        } else {
            matched_after += 1;
        }
    }
    if !saw_self {
        return Err(SelectorError::Internal {
            contract: "sibling position of a node outside its parent",
        });
    }
    let position = if from_start {
        matched_before + 1
    } else {
        matched_after + 1
    };
    Ok(Some(position))
}

/// The element's language per the pinned HTML language order: the nearest applicable `lang` or `xml:lang` attribute,
/// then the document's pragma-set default language. `None` is unknown (external protocol input is fixed to `None` by
/// `html.css@1`).
fn element_language<'i>(index: &'i MarkupIndex, node: jqf_data::NodeId) -> Option<&'i str> {
    let mut current = Some(node);
    while let Some(cursor) = current {
        if let Some(lang) = index.attr(cursor, "lang") {
            return Some(lang);
        }
        if let Some(xml_lang) = index.attr(cursor, "xml:lang") {
            return Some(xml_lang);
        }
        current = index.parent_of(cursor);
    }
    index.pragma_language.as_deref()
}

/// RFC 4647 extended filtering of one range against one language tag.
fn lang_range_matches(range: &LanguageRange, tag: &[&str]) -> bool {
    if tag.is_empty() {
        return false;
    }
    for (position, subtag) in range.subtags.iter().enumerate() {
        match subtag {
            RangeSubtag::Any => {
                if position >= tag.len() {
                    return false;
                }
            }
            RangeSubtag::Literal(literal) => {
                let Some(actual) = tag.get(position) else {
                    return false;
                };
                if !literal.eq_ignore_ascii_case(actual) {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::Nth;

    #[test]
    fn nth_admits_does_not_overflow_on_extreme_b() {
        // `:nth-child(1n-9223372036854775807)`: b = -i64::MAX, and `position - b` overflows i64 for every position >=
        // 1. The formula cannot match anything, so each position is a miss — never a panic.
        let nth = Nth { a: 1, b: -i64::MAX };
        for position in [1, 2, i64::MAX] {
            assert!(!nth.admits(position), "position {position} must not match");
        }
        // The mirror case: b = i64::MAX, `position - b` negative but in range.
        let nth = Nth { a: 1, b: i64::MAX };
        assert!(!nth.admits(1));
        assert!(!nth.admits(i64::MAX - 1));
        assert!(nth.admits(i64::MAX));
        // Ordinary An+B still answers.
        let odd = Nth { a: 2, b: 1 };
        assert!(!odd.admits(2));
        assert!(odd.admits(3));
        let every_third_from_zero = Nth { a: 3, b: 0 };
        assert!(every_third_from_zero.admits(3));
        assert!(!every_third_from_zero.admits(4));
    }

    #[test]
    fn string_literals_advance_by_code_point_without_panicking() {
        // The string scanner advances one full code point at a time and never slices mid-character: multibyte and
        // escape-heavy interiors compile as ordinary selectors, and hostile text degrades to a named Compile error
        // rather than a panic.
        for text in [r#"[href="café"]"#, r#"[href="a\26 6C"]"#, r"[title='naïve']"] {
            assert!(super::compile_css(text).is_ok(), "selector {text:?} compiles");
        }
        assert!(super::compile_css(r#"[href="unterminated]"#).is_err());
    }

    /// The hex escape's invalid values — zero, surrogates, beyond the code-point range — each decode to U+FFFD, and
    /// a valid value decodes to itself (css-syntax-3 §4.3.8).
    #[test]
    fn hex_escapes_map_invalid_values_to_the_replacement_character() {
        let decode = |rest: &str| {
            let mut cursor = super::Cursor::new(rest);
            cursor.at += 1; // past the backslash the caller consumed
            let (decoded, next) = cursor.decode_escape().expect("escape decodes");
            (decoded, next)
        };
        let fffd = '\u{FFFD}';
        assert_eq!(decode(r"\0").0, Some(fffd), "zero is replaced");
        assert_eq!(decode(r"\d800").0, Some(fffd), "surrogate is replaced");
        assert_eq!(decode(r"\dfFF").0, Some(fffd), "low surrogate is replaced");
        assert_eq!(
            decode(r"\110000").0,
            Some(fffd),
            "beyond the code-point range is replaced"
        );
        assert_eq!(decode(r"\41").0, Some('A'), "valid value decodes as itself");
        assert_eq!(decode(r"\10ffff").0, Some('\u{10FFFF}'), "the top code point decodes");
        // The whitespace after a hex escape is consumed as its terminator.
        assert_eq!(decode(r"\26 6C").1, 4, "one whitespace terminates");
    }
}
