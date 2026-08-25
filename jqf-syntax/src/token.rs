//! Token data shared by the lexer and parser.
//!
//! Tokens keep lexical classification separate from source text. Consumers use the span to recover the original
//! spelling when exact source form matters.

use jqf_source::Span;

use crate::inventory::closed_inventory;

/// A lexical category paired with its original byte range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Token {
    /// Lexical classification assigned by the lexer.
    pub kind: TokenKind,
    /// Half-open byte range in the source string passed to the lexer.
    pub span: Span,
}

impl Token {
    /// Construct an already-classified token.
    #[must_use]
    pub const fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// Fixed source spellings, ordered so longer lexemes win before prefixes.
///
/// The lexer's `fixed_token_kind` match is the mirror of this table, kept longest-first. The match is the dispatch;
/// this table is the spelling inventory. `fixed_lexeme()` reads this table; the integration test
/// `lexer_dispatch_covers_the_complete_fixed_token_inventory` pins the table-to-lexer direction.
pub(crate) const FIXED_TOKEN_LEXEMES: &[(&str, TokenKind)] = &[
    ("?//", TokenKind::DestructureAlt),
    ("//=", TokenKind::AltAssign),
    ("|=", TokenKind::PipeAssign),
    ("::", TokenKind::DoubleColon),
    (".@", TokenKind::DotAt),
    (".&", TokenKind::DotAmp),
    ("+=", TokenKind::AddAssign),
    ("-=", TokenKind::SubAssign),
    ("*=", TokenKind::MulAssign),
    ("/=", TokenKind::DivAssign),
    ("%=", TokenKind::ModAssign),
    ("=>", TokenKind::FatArrow),
    ("==", TokenKind::Eq),
    ("!=", TokenKind::Ne),
    ("<=", TokenKind::Le),
    (">=", TokenKind::Ge),
    ("..", TokenKind::DotDot),
    ("//", TokenKind::Alt),
    (".", TokenKind::Dot),
    ("~", TokenKind::Tilde),
    ("?", TokenKind::Question),
    ("|", TokenKind::Pipe),
    (",", TokenKind::Comma),
    (":", TokenKind::Colon),
    (";", TokenKind::Semi),
    ("(", TokenKind::LParen),
    (")", TokenKind::RParen),
    ("[", TokenKind::LBracket),
    ("]", TokenKind::RBracket),
    ("{", TokenKind::LBrace),
    ("}", TokenKind::RBrace),
    ("+", TokenKind::Plus),
    ("-", TokenKind::Minus),
    ("*", TokenKind::Star),
    ("/", TokenKind::Slash),
    ("%", TokenKind::Percent),
    ("=", TokenKind::Assign),
    ("<", TokenKind::Lt),
    (">", TokenKind::Gt),
];

closed_inventory! {
/// Lexical categories for the catalogued syntax plus jqf extensions.
///
/// The enum is intentionally source-oriented. It does not identify builtins, resolve names, or decide whether a token
/// is valid in a particular grammar context. [`TokenKind::ALL`] comes from this same variant list, so a new category
/// joins the inventory by construction.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum TokenKind {
    /// Name segment before resolution into a call, property key, or binding.
    Ident,
    /// Complete variable spelling beginning with `$`, including qualifications.
    Variable,
    /// Number spelling without a leading sign; unary `-` is a separate token.
    Number,
    /// Complete quoted string source, including any interpolation markers.
    String,
    /// Format filter name such as `@json`; supported formats are not checked here.
    Format,
    /// Contiguous `.@` introducer for node accessors, metadata, and facts.
    DotAt,
    /// Contiguous `.&` introducer for markup attribute access.
    DotAmp,
    /// Identity expression or ordinary path segment separator.
    Dot,
    /// Recursive descent expression introducer.
    DotDot,
    /// Engine-surface marker introducing `~generator` constructors and `~x` bindings.
    Tilde,
    /// Optional path/call marker or postfix error-suppression shorthand.
    Question,
    /// Destructuring-pattern alternative operator.
    DestructureAlt,
    /// Filter composition operator and binding-body separator.
    Pipe,
    /// Generator separator for multiple-output expressions.
    Comma,
    /// Separator for object members, definitions, slices, and metadata forms.
    Colon,
    /// Namespace separator inside qualified names.
    DoubleColon,
    /// Separator for definitions, source items, call arguments, and loop slots.
    Semi,
    /// Begins grouping, calls, and control-form slot lists.
    LParen,
    /// Ends grouping, calls, and control-form slot lists.
    RParen,
    /// Begins arrays, indexes, slices, iterators, and array patterns.
    LBracket,
    /// Ends arrays, indexes, slices, iterators, and array patterns.
    RBracket,
    /// Begins objects, metadata objects, and object patterns.
    LBrace,
    /// Ends objects, metadata objects, and object patterns.
    RBrace,
    /// Additive operator; runtime meaning depends on operand values.
    Plus,
    /// Subtraction operator or unary negation.
    Minus,
    /// Multiplicative operator; may also represent object merge at runtime.
    Star,
    /// Division operator; string splitting is a later semantic decision.
    Slash,
    /// Remainder operator.
    Percent,
    /// Plain assignment operator, or the separator inside jqf `let`.
    Assign,
    /// Reserved unsupported spelling kept so diagnostics can target it precisely.
    FatArrow,
    /// Semantic equality operator.
    Eq,
    /// Semantic inequality operator.
    Ne,
    /// Less-than comparison operator.
    Lt,
    /// Less-than-or-equal comparison operator.
    Le,
    /// Greater-than comparison operator.
    Gt,
    /// Greater-than-or-equal comparison operator.
    Ge,
    /// Alternative/defaulting operator.
    Alt,
    /// Alternative update-assignment shorthand.
    AltAssign,
    /// Addition update-assignment shorthand.
    AddAssign,
    /// Subtraction update-assignment shorthand.
    SubAssign,
    /// Multiplication update-assignment shorthand.
    MulAssign,
    /// Division update-assignment shorthand.
    DivAssign,
    /// Remainder update-assignment shorthand.
    ModAssign,
    /// Update-assignment operator.
    PipeAssign,
    /// Logical conjunction keyword.
    And,
    /// Logical disjunction keyword.
    Or,
    /// Binding introducer for `as` bindings and loop sources.
    As,
    /// Function definition introducer.
    Def,
    /// Module metadata declaration introducer.
    Module,
    /// Module or data import introducer.
    Import,
    /// Module include introducer.
    Include,
    /// Conditional expression introducer.
    If,
    /// Separates a conditional predicate from its result expression.
    Then,
    /// Introduces an additional conditional branch.
    Elif,
    /// Introduces a conditional fallback branch.
    Else,
    /// Terminates conditional and similar delimited forms.
    End,
    /// Error-handling expression introducer.
    Try,
    /// Introduces an error handler for `try`.
    Catch,
    /// Reduction expression introducer.
    Reduce,
    /// Stateful iteration expression introducer.
    Foreach,
    /// Named break target introducer.
    Label,
    /// Control transfer to a named label.
    Break,
    /// jqf binding-sugar introducer.
    Let,
    /// Source form with syntax relevance but runtime-defined behavior.
    Empty,
    /// Null literal.
    Null,
    /// Boolean true literal.
    True,
    /// Boolean false literal.
    False,
    /// Sentinel emitted once after all source bytes are consumed.
    Eof,
    /// Malformed or unsupported source spelling.
    Error,
}
}

impl TokenKind {
    /// Return the fixed source spelling for symbolic tokens.
    ///
    /// Names, literals, keywords, EOF, and error tokens have no fixed spelling through this API.
    #[must_use]
    pub fn fixed_lexeme(self) -> Option<&'static str> {
        FIXED_TOKEN_LEXEMES
            .iter()
            .find_map(|(lexeme, kind)| (*kind == self).then_some(*lexeme))
    }

    /// Stable short description for diagnostics and debug projections.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Ident => "identifier",
            Self::Variable => "variable",
            Self::Number => "number literal",
            Self::String => "string literal or template",
            Self::Format => "format filter",
            Self::DotAt => "node accessor introducer",
            Self::DotAmp => "named attribute introducer",
            Self::Dot => "dot",
            Self::DotDot => "recursive descent",
            Self::Tilde => "engine-surface marker",
            Self::Question => "optional marker",
            Self::DestructureAlt => "destructuring alternative",
            Self::Pipe => "pipe",
            Self::Comma => "comma",
            Self::Colon => "colon",
            Self::DoubleColon => "namespace separator",
            Self::Semi => "semicolon",
            Self::LParen => "opening parenthesis",
            Self::RParen => "closing parenthesis",
            Self::LBracket => "opening bracket",
            Self::RBracket => "closing bracket",
            Self::LBrace => "opening brace",
            Self::RBrace => "closing brace",
            Self::Plus => "addition operator",
            Self::Minus => "subtraction operator",
            Self::Star => "multiplication operator",
            Self::Slash => "division operator",
            Self::Percent => "modulo operator",
            Self::Assign => "assignment operator",
            Self::FatArrow => "unsupported fat-arrow token",
            Self::Eq => "semantic equality operator",
            Self::Ne => "semantic inequality operator",
            Self::Lt => "less-than operator",
            Self::Le => "less-than-or-equal operator",
            Self::Gt => "greater-than operator",
            Self::Ge => "greater-than-or-equal operator",
            Self::Alt => "alternative operator",
            Self::AltAssign => "alternative assignment operator",
            Self::AddAssign => "addition assignment operator",
            Self::SubAssign => "subtraction assignment operator",
            Self::MulAssign => "multiplication assignment operator",
            Self::DivAssign => "division assignment operator",
            Self::ModAssign => "modulo assignment operator",
            Self::PipeAssign => "update assignment operator",
            Self::And => "and keyword",
            Self::Or => "or keyword",
            Self::As => "as keyword",
            Self::Def => "def keyword",
            Self::Module => "module keyword",
            Self::Import => "import keyword",
            Self::Include => "include keyword",
            Self::If => "if keyword",
            Self::Then => "then keyword",
            Self::Elif => "elif keyword",
            Self::Else => "else keyword",
            Self::End => "end keyword",
            Self::Try => "try keyword",
            Self::Catch => "catch keyword",
            Self::Reduce => "reduce keyword",
            Self::Foreach => "foreach keyword",
            Self::Label => "label keyword",
            Self::Break => "break keyword",
            Self::Let => "let keyword",
            Self::Empty => "empty token",
            Self::Null => "null literal",
            Self::True => "true literal",
            Self::False => "false literal",
            Self::Eof => "end of input",
            Self::Error => "invalid token",
        }
    }
}
