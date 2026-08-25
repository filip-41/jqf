//! Source-preserving syntax data produced by the parser.
//!
//! The AST records syntactic shape and byte spans. It deliberately keeps names, literals, and operators tied to source
//! ranges instead of resolving builtins, evaluating constants, or assigning runtime meaning.

use alloc::{boxed::Box, vec::Vec};

use jqf_source::{Diagnostic, SourceRef, Span};

/// The closing-delimiter span for a container whose whole span is `container`: its last byte, or a zero-width insertion
/// at the end when the closer was missing and recovery synthesized it. One copy of the recovery-span law; the
/// close-span accessors share it so it cannot drift.
fn close_span_from_container(container: Span, missing: bool) -> Span {
    if missing {
        Span::new(container.end(), container.end())
    } else {
        Span::new(container.end() - 1, container.end())
    }
}

/// One source-preserving string-template segment.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TemplateSegment {
    /// Literal source bytes between interpolation forms.
    Literal {
        /// Byte range of the literal content, excluding surrounding quote tokens.
        span: Span,
    },
    /// Expression source inside a `\(...)` interpolation.
    Expression {
        /// Byte range of the expression, excluding the `\(` introducer and closing `)`.
        span: Span,
        /// Parsed interpolation expression.
        expression: Box<Expr>,
        /// Byte range of the `\(` interpolation introducer.
        introducer_span: Span,
        /// Byte range of the closing `)`.
        close_span: Span,
    },
}

impl TemplateSegment {
    /// Returns the segment's source range.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::Literal { span } | Self::Expression { span, .. } => *span,
        }
    }
}

/// Parsed source layout of one quoted string or interpolation template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StringTemplate {
    segments: Vec<TemplateSegment>,
    span: Span,
}

impl StringTemplate {
    pub(crate) const fn empty(span: Span) -> Self {
        Self {
            segments: Vec::new(),
            span,
        }
    }

    pub(crate) fn push(&mut self, segment: TemplateSegment) {
        self.segments.push(segment);
    }

    /// Returns literal and expression segments in source order.
    #[must_use]
    pub fn segments(&self) -> &[TemplateSegment] {
        &self.segments
    }

    /// Returns the complete quoted-token span.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Parsed source unit containing top-level declarations and an optional query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceUnit {
    /// Top-level directives and definitions, preserved in source order.
    pub items: Vec<SourceItem>,
    /// Final query expression for program units.
    pub expression: Option<Expr>,
    /// Byte range covered by the parsed source unit.
    pub span: Span,
}

/// Top-level source item.
///
/// Closed: the walk inventory and the engine lowerer claim totality, so a new variant must fail to compile at every
/// match rather than vanish into `_ =>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceItem {
    /// Leading module metadata declaration.
    Module(ModuleItem),
    /// Module or data import declaration.
    Import(ImportItem),
    /// Module include declaration.
    Include(IncludeItem),
    /// Function definition.
    Def(DefItem),
}

impl SourceItem {
    pub(crate) const fn span(&self) -> Span {
        match self {
            Self::Module(item) => item.span,
            Self::Import(item) => item.span,
            Self::Include(item) => item.span,
            Self::Def(item) => item.span,
        }
    }
}

/// Module metadata declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleItem {
    /// Exact `module` keyword span.
    pub module_keyword_span: Span,
    /// Metadata expression attached to the module declaration.
    pub metadata: Expr,
    /// Exact terminating semicolon span.
    pub semicolon_span: Span,
    /// Byte range covered by the declaration.
    pub span: Span,
}

/// Module or data import declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportItem {
    /// Exact `import` keyword span.
    pub import_keyword_span: Span,
    /// Parsed string path token.
    pub path: StringTemplate,
    /// Exact `as` keyword span.
    pub as_keyword_span: Span,
    /// Alias name or variable token span.
    pub alias: Span,
    /// Optional metadata expression.
    pub metadata: Option<Expr>,
    /// Exact terminating semicolon span.
    pub semicolon_span: Span,
    /// Byte range covered by the declaration.
    pub span: Span,
}

/// Module include declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IncludeItem {
    /// Exact `include` keyword span.
    pub include_keyword_span: Span,
    /// Parsed string path token.
    pub path: StringTemplate,
    /// Optional metadata expression.
    pub metadata: Option<Expr>,
    /// Exact terminating semicolon span.
    pub semicolon_span: Span,
    /// Byte range covered by the declaration.
    pub span: Span,
}

/// One source-preserving function parameter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefParameter {
    /// Parameter name or variable span.
    pub name: Span,
    /// Following semicolon span when another parameter was authored.
    pub separator_span: Option<Span>,
}

/// Function definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefItem {
    /// Exact `def` keyword span.
    pub def_keyword_span: Span,
    /// Function name span, including any qualified segments.
    pub name: Span,
    /// Semicolon-separated parameters.
    pub params: Vec<DefParameter>,
    /// Complete parameter-parentheses span, when authored.
    pub parameter_parentheses: Option<Span>,
    pub(crate) parameter_close_missing: bool,
    /// Exact definition colon span.
    pub colon_span: Span,
    /// Function body expression.
    pub body: Expr,
    /// Exact terminating semicolon span.
    pub semicolon_span: Span,
    /// Byte range covered by the definition.
    pub span: Span,
}

impl DefItem {
    /// Exact opening parameter-parenthesis span, when authored.
    #[must_use]
    pub fn parameter_open_span(&self) -> Option<Span> {
        self.parameter_parentheses
            .map(|span| Span::new(span.start(), span.start() + 1))
    }

    /// Exact closing parameter-parenthesis span, including a zero-width recovery insertion.
    #[must_use]
    pub fn parameter_close_span(&self) -> Option<Span> {
        self.parameter_parentheses
            .map(|span| close_span_from_container(span, self.parameter_close_missing))
    }
}

/// Lexically scoped function definition followed by its query body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DefinitionExpr {
    /// Function definition introduced at this query position.
    pub definition: Box<DefItem>,
    /// Remaining query evaluated with the definition in scope.
    pub body: Box<Expr>,
}

/// Parsed syntax together with the source identity that produced it.
///
/// The wrapper retains compact binding metadata only. Source text remains owned by the caller and is attached through
/// [`ParsedSyntax::bind`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedSyntax<T> {
    source: SourceRef,
    source_len: u32,
    root: T,
}

impl<T> ParsedSyntax<T> {
    pub(crate) const fn new(source: SourceRef, source_len: u32, root: T) -> Self {
        Self {
            source,
            source_len,
            root,
        }
    }

    /// Source identity used for parsing.
    #[must_use]
    pub const fn source_ref(&self) -> SourceRef {
        self.source
    }

    /// Exact UTF-8 source byte length used for parsing.
    #[must_use]
    pub const fn source_len(&self) -> u32 {
        self.source_len
    }

    /// Parsed root.
    #[must_use]
    pub const fn root(&self) -> &T {
        &self.root
    }

    /// Consume the wrapper and return its parsed root.
    #[must_use]
    pub fn into_root(self) -> T {
        self.root
    }
}

impl<T> core::ops::Deref for ParsedSyntax<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.root()
    }
}

/// Parser output paired with diagnostics collected while building it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parse<T> {
    syntax: Option<ParsedSyntax<T>>,
    diagnostics: Vec<Diagnostic>,
}

impl<T> Parse<T> {
    pub(crate) fn new(syntax: Option<ParsedSyntax<T>>, diagnostics: Vec<Diagnostic>) -> Self {
        Self { syntax, diagnostics }
    }

    /// Whether a complete syntax root was produced without diagnostics.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.syntax.is_some() && self.diagnostics.is_empty()
    }

    /// Parsed syntax, when the entry point could recover a root node.
    #[must_use]
    pub const fn syntax(&self) -> Option<&ParsedSyntax<T>> {
        self.syntax.as_ref()
    }

    /// Diagnostics produced while lexing and parsing.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Return syntax only when parsing produced a root without diagnostics.
    ///
    /// Recovery trees are returned as an error together with their diagnostics, preventing downstream lowering from
    /// treating a recovered root as valid.
    ///
    /// # Errors
    ///
    /// Returns all parser diagnostics when any error was observed.
    pub fn into_valid_syntax(self) -> Result<ParsedSyntax<T>, Vec<Diagnostic>> {
        match (self.syntax, self.diagnostics) {
            (Some(syntax), diagnostics) if diagnostics.is_empty() => Ok(syntax),
            (_, diagnostics) => Err(diagnostics),
        }
    }
}

/// Query expression with its covering byte span.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Expr {
    kind: ExprKind,
    span: Span,
}

impl Expr {
    pub(crate) const fn new(kind: ExprKind, span: Span) -> Self {
        Self { kind, span }
    }

    /// Syntactic expression form.
    #[must_use]
    pub const fn kind(&self) -> &ExprKind {
        &self.kind
    }

    /// Byte range covered by the expression source form.
    #[must_use]
    pub const fn span(&self) -> Span {
        self.span
    }
}

/// Expression forms understood by the syntax crate.
///
/// Closed: the walk inventory and the engine lowerer claim totality, so a new variant must fail to compile at every
/// match rather than vanish into `_ =>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExprKind {
    /// Recovery node inserted after a syntax error.
    Error,
    /// Identity expression `.`.
    Identity,
    /// Recursive descent expression `..`.
    RecursiveDescent,
    /// `empty` source form.
    Empty,
    /// `null` literal.
    Null,
    /// Boolean literal.
    Bool(bool),
    /// Numeric literal; the exact spelling is recovered through the span.
    Number,
    /// String literal or string template source.
    String(StringTemplate),
    /// Variable reference.
    Variable,
    /// Format filter such as `@json`.
    Format,
    /// Format filter applied directly to a string template.
    FormatTemplate {
        /// Format filter expression.
        format: Box<Expr>,
        /// Parsed template string source layout.
        template: StringTemplate,
    },
    /// Parenthesized expression.
    Group {
        /// Expression inside the delimiters.
        expression: Box<Expr>,
        /// Exact opening-parenthesis span.
        open_span: Span,
        /// Exact closing-parenthesis span.
        close_span: Span,
    },
    /// Array construction; a present expression may be a generator.
    Array {
        /// Optional generator body.
        expression: Option<Box<Expr>>,
        /// Exact opening-bracket span.
        open_span: Span,
        /// Exact closing-bracket span.
        close_span: Span,
    },
    /// Object construction.
    Object {
        /// Ordered members.
        members: Vec<ObjectMember>,
        /// Exact opening-brace span.
        open_span: Span,
        /// Exact closing-brace span.
        close_span: Span,
    },
    /// Prefix operator expression.
    Unary(UnaryExpr),
    /// Binary operator expression.
    Binary(BinaryExpr),
    /// Assignment expression.
    Assignment(AssignmentExpr),
    /// Lexically scoped function definition.
    Definition(DefinitionExpr),
    /// Function or filter call.
    Call(CallExpr),
    /// An ENGINE-constructor call `~name(args)` — the `~`-prefixed surface (`~generator(init; update; extract)`).
    /// Engine things are `~`-prefixed and an unmarked name stays an ordinary call. The expression span includes the `~`
    /// introducer; the name span does not.
    EngineCall {
        /// The `~` introducer span.
        tilde_span: Span,
        /// Call surface excluding the `~` introducer.
        call: CallExpr,
    },
    /// A bare engine term `~name`: either an engine-constructor reference (`~generator`) or an engine-binding reference
    /// (`~x`) before the parser knows which. Lowering resolves the name against the engine-binding scope and the closed
    /// constructor list.
    EngineTerm {
        /// The `~` introducer span.
        tilde_span: Span,
        /// The name span following the `~`, excluding the introducer.
        name: Span,
    },
    /// Postfix path, accessor, or optional marker.
    Postfix(PostfixExpr),
    /// Conditional expression.
    If(ConditionalExpr),
    /// Error handling expression.
    Try(TryExpr),
    /// Reduction expression.
    Reduce(LoopExpr),
    /// Stateful iteration expression.
    Foreach(LoopExpr),
    /// Binding expression introduced by `as` or jqf `let`.
    Binding(BindingExpr),
    /// Label expression.
    Label {
        /// Exact `label` keyword span.
        label_keyword_span: Span,
        /// Label variable span.
        label: Span,
        /// Exact binding-pipe span.
        pipe_span: Span,
        /// Labeled body expression.
        body: Box<Expr>,
    },
    /// Break expression.
    Break {
        /// Exact `break` keyword span.
        break_keyword_span: Span,
        /// Label variable span.
        label: Span,
    },
}

/// Prefix operator expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnaryExpr {
    /// Operator form.
    pub op: UnaryOp,
    /// Operator byte span.
    pub op_span: Span,
    /// Operand expression.
    pub expr: Box<Expr>,
}

/// Prefix operators with syntax-level meaning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UnaryOp {
    /// Unary numeric negation.
    Negate,
}

/// Binary operator expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinaryExpr {
    /// Operator form.
    pub op: BinaryOp,
    /// Operator byte span.
    pub op_span: Span,
    /// Left-hand expression.
    pub left: Box<Expr>,
    /// Right-hand expression.
    pub right: Box<Expr>,
}

/// Binary operators represented directly in expression trees.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BinaryOp {
    /// Filter composition operator.
    Pipe,
    /// Generator separator.
    Comma,
    /// Alternative/defaulting operator.
    Alternative,
    /// Logical conjunction.
    And,
    /// Logical disjunction.
    Or,
    /// Semantic equality comparison.
    Equal,
    /// Semantic inequality comparison.
    NotEqual,
    /// Less-than comparison.
    Less,
    /// Less-than-or-equal comparison.
    LessEqual,
    /// Greater-than comparison.
    Greater,
    /// Greater-than-or-equal comparison.
    GreaterEqual,
    /// Addition operator.
    Add,
    /// Subtraction operator.
    Subtract,
    /// Multiplication operator.
    Multiply,
    /// Division operator.
    Divide,
    /// Remainder operator.
    Remainder,
}

/// Assignment expression with a source-preserving operator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssignmentExpr {
    /// Assignment form.
    pub op: AssignmentOp,
    /// Exact operator byte span.
    pub op_span: Span,
    /// Target expression.
    pub target: Box<Expr>,
    /// Value or update expression.
    pub value: Box<Expr>,
}

/// Assignment operator forms.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AssignmentOp {
    /// Plain assignment `=`.
    Assign,
    /// Update assignment `|=`.
    Update,
    /// Addition update `+=`.
    Add,
    /// Subtraction update `-=`.
    Subtract,
    /// Multiplication update `*=`.
    Multiply,
    /// Division update `/=`.
    Divide,
    /// Remainder update `%=`.
    Remainder,
    /// Alternative update `//=`.
    Alternative,
}

/// Function or filter call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallExpr {
    /// Bare or qualified function/filter name.
    pub name: Span,
    /// Semicolon-separated call arguments.
    pub args: Vec<CallArgument>,
    /// Complete parenthesis span for an explicit call, or `None` for a bare zero-argument call.
    pub parentheses: Option<Span>,
    pub(crate) close_parenthesis_missing: bool,
}

impl CallExpr {
    /// Exact opening-parenthesis span for an explicit call.
    #[must_use]
    pub fn open_parenthesis_span(&self) -> Option<Span> {
        self.parentheses.map(|span| Span::new(span.start(), span.start() + 1))
    }

    /// Exact closing-parenthesis span, including a zero-width recovery insertion.
    #[must_use]
    pub fn close_parenthesis_span(&self) -> Option<Span> {
        self.parentheses
            .map(|span| close_span_from_container(span, self.close_parenthesis_missing))
    }
}

/// One source-preserving filter argument in a named call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallArgument {
    /// Argument expression.
    pub expression: Expr,
    /// Following semicolon span, when another argument was authored.
    pub separator_span: Option<Span>,
}

/// Postfix expression over a base expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostfixExpr {
    chain: PostfixChain,
}

impl PostfixExpr {
    /// Expression being selected from or modified.
    #[must_use]
    pub fn base(&self) -> &Expr {
        match &self.chain {
            PostfixChain::One { base, .. } => base,
            PostfixChain::Two(chain) => &chain.0,
            PostfixChain::Three(chain) => &chain.0,
            PostfixChain::Four(chain) => &chain.0,
            PostfixChain::Five(chain) => &chain.0,
            PostfixChain::Six(chain) => &chain.0,
            PostfixChain::Spill(chain) => &chain.0,
        }
    }

    /// Postfix operations in base-first authored order.
    #[must_use]
    pub fn steps(&self) -> &[PostfixStep] {
        match &self.chain {
            PostfixChain::One { step, .. } => core::slice::from_ref(step),
            PostfixChain::Two(chain) => &chain.1,
            PostfixChain::Three(chain) => &chain.1,
            PostfixChain::Four(chain) => &chain.1,
            PostfixChain::Five(chain) => &chain.1,
            PostfixChain::Six(chain) => &chain.1,
            PostfixChain::Spill(chain) => &chain.1,
        }
    }

    pub(crate) fn finish_one(base: Expr, step: PostfixStep) -> Expr {
        let span = base.span().merge(step.span);
        Expr::new(
            ExprKind::Postfix(Self {
                chain: PostfixChain::One {
                    base: Box::new(base),
                    step,
                },
            }),
            span,
        )
    }

    pub(crate) fn finish_two(base: Expr, steps: [PostfixStep; 2]) -> Expr {
        let span = base.span().merge(steps[1].span);
        Expr::new(
            ExprKind::Postfix(Self {
                chain: PostfixChain::Two(Box::new((base, steps))),
            }),
            span,
        )
    }

    pub(crate) fn finish_three(base: Expr, steps: [PostfixStep; 3]) -> Expr {
        let span = base.span().merge(steps[2].span);
        Expr::new(
            ExprKind::Postfix(Self {
                chain: PostfixChain::Three(Box::new((base, steps))),
            }),
            span,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PostfixChain {
    One { base: Box<Expr>, step: PostfixStep },
    Two(Box<(Expr, [PostfixStep; 2])>),
    Three(Box<(Expr, [PostfixStep; 3])>),
    Four(Box<(Expr, [PostfixStep; 4])>),
    Five(Box<(Expr, [PostfixStep; 5])>),
    Six(Box<(Expr, [PostfixStep; 6])>),
    Spill(Box<(Expr, Vec<PostfixStep>)>),
}

pub(crate) struct PostfixBuilder {
    base: Expr,
    first: Option<PostfixStep>,
    second: Option<PostfixStep>,
    third: Option<PostfixStep>,
    fourth: Option<PostfixStep>,
    fifth: Option<PostfixStep>,
    sixth: Option<PostfixStep>,
    len: usize,
    last_span: Span,
    spill: Option<Vec<PostfixStep>>,
}

impl PostfixBuilder {
    pub(crate) fn new(base: Expr, first: PostfixStep, second: PostfixStep) -> Self {
        let last_span = second.span;
        Self {
            base,
            first: Some(first),
            second: Some(second),
            third: None,
            fourth: None,
            fifth: None,
            sixth: None,
            len: 2,
            last_span,
            spill: None,
        }
    }

    pub(crate) fn push(&mut self, step: PostfixStep) {
        self.last_span = step.span;
        if let Some(steps) = &mut self.spill {
            steps.push(step);
            self.len += 1;
            return;
        }
        match self.len {
            2 => self.third = Some(step),
            3 => self.fourth = Some(step),
            4 => self.fifth = Some(step),
            5 => self.sixth = Some(step),
            6 => {
                let mut spill = Vec::with_capacity(12);
                spill.push(self.first.take().expect("first postfix builder slot"));
                spill.push(self.second.take().expect("second postfix builder slot"));
                spill.push(self.third.take().expect("third postfix builder slot"));
                spill.push(self.fourth.take().expect("fourth postfix builder slot"));
                spill.push(self.fifth.take().expect("fifth postfix builder slot"));
                spill.push(self.sixth.take().expect("sixth postfix builder slot"));
                spill.push(step);
                self.spill = Some(spill);
            }
            _ => unreachable!("inline postfix builder has two to six steps"),
        }
        self.len += 1;
    }

    pub(crate) fn span(&self) -> Span {
        self.base.span().merge(self.last_span)
    }

    pub(crate) fn finish(self) -> Expr {
        let span = self.span();
        let chain = if let Some(steps) = self.spill {
            PostfixChain::Spill(Box::new((self.base, steps)))
        } else {
            let first = self.first.expect("first postfix step");
            let second = self.second.expect("second postfix step");
            match self.len {
                4 => PostfixChain::Four(Box::new((
                    self.base,
                    [
                        first,
                        second,
                        self.third.expect("third postfix step"),
                        self.fourth.expect("fourth postfix step"),
                    ],
                ))),
                5 => PostfixChain::Five(Box::new((
                    self.base,
                    [
                        first,
                        second,
                        self.third.expect("third postfix step"),
                        self.fourth.expect("fourth postfix step"),
                        self.fifth.expect("fifth postfix step"),
                    ],
                ))),
                6 => PostfixChain::Six(Box::new((
                    self.base,
                    [
                        first,
                        second,
                        self.third.expect("third postfix step"),
                        self.fourth.expect("fourth postfix step"),
                        self.fifth.expect("fifth postfix step"),
                        self.sixth.expect("sixth postfix step"),
                    ],
                ))),
                _ => unreachable!("multi-step postfix builder has two to six inline steps"),
            }
        };
        Expr::new(ExprKind::Postfix(PostfixExpr { chain }), span)
    }
}

/// One authored operation in a postfix chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PostfixStep {
    /// Postfix operation.
    pub segment: PostfixSegment,
    /// Byte span of the introducer token such as `.`, `[`, `.@`, or `.&`.
    pub operator_span: Span,
    /// Byte span of the optional suffix when present.
    pub optional_suffix_span: Option<Span>,
    /// Byte range covered by this postfix operation.
    pub span: Span,
}

/// Conditional expression with one or more predicate branches.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionalExpr {
    /// Ordered `if` and `elif` branches.
    pub branches: Vec<ConditionalBranch>,
    /// Explicit `else` branch when present.
    pub else_branch: Option<Box<Expr>>,
    /// Exact `else` keyword span when authored.
    pub else_keyword_span: Option<Span>,
    /// Exact closing `end` keyword span.
    pub end_keyword_span: Span,
}

/// One `if` or `elif` branch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionalBranch {
    /// Exact `if` or `elif` keyword span.
    pub keyword_span: Span,
    /// Predicate expression.
    pub condition: Expr,
    /// Exact `then` keyword span.
    pub then_keyword_span: Span,
    /// Result expression for a true predicate.
    pub then_branch: Expr,
}

/// `try` expression with an optional handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TryExpr {
    /// Exact `try` keyword span.
    pub try_keyword_span: Span,
    /// Protected expression.
    pub expr: Box<Expr>,
    /// Exact `catch` keyword span when authored.
    pub catch_keyword_span: Option<Span>,
    /// Optional `catch` handler expression.
    pub handler: Option<Box<Expr>>,
}

/// Reduction or foreach expression.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopExpr {
    /// Exact `reduce` or `foreach` keyword span.
    pub keyword_span: Span,
    /// Source expression being iterated.
    pub source: Box<Expr>,
    /// Exact required `as` keyword span.
    pub as_keyword_span: Span,
    /// Required binding pattern. Recovery uses [`PatternKind::Error`] rather than an omitted semantic slot.
    pub binding: Pattern,
    /// Exact opening-parenthesis span.
    pub open_span: Span,
    /// Initial state expression.
    pub init: Box<Expr>,
    /// Exact initializer/update semicolon span.
    pub update_separator_span: Span,
    /// Update expression.
    pub update: Box<Expr>,
    /// Exact update/extract semicolon span when authored.
    pub extract_separator_span: Option<Span>,
    /// Extract expression for `foreach`.
    pub extract: Option<Box<Expr>>,
    /// Exact closing-parenthesis span.
    pub close_span: Span,
}

/// Authored binding form and its source punctuation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BindingForm {
    /// `source as pattern | body`.
    As {
        /// Exact `as` keyword span.
        as_keyword_span: Span,
        /// Exact binding-pipe span.
        pipe_span: Span,
    },
    /// jqf `let pattern = source | body`.
    Let {
        /// Exact `let` keyword span.
        let_keyword_span: Span,
        /// Exact equals-separator span.
        equals_span: Span,
        /// Exact binding-pipe span.
        pipe_span: Span,
    },
}

/// Binding expression introduced by `as` or `let`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindingExpr {
    /// Authored binding syntax.
    pub form: BindingForm,
    /// Pattern receiving bound values.
    pub pattern: Pattern,
    /// Generator producing values to bind.
    pub value: Box<Expr>,
    /// Body expression evaluated with the binding in scope.
    pub body: Box<Expr>,
}

/// Postfix path and accessor forms.
///
/// Closed: the walk inventory and the engine lowerer claim totality, so a new variant must fail to compile at every
/// match rather than vanish into `_ =>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PostfixSegment {
    /// Identifier or quoted object field.
    Field {
        /// Field selector source form.
        selector: FieldSelector,
    },
    /// Dynamic index, empty iterator, or bracketed selector.
    Index {
        /// Index expression; absent for iterator syntax `[]`.
        index: Option<Box<Expr>>,
        /// Exact opening-bracket span.
        open_span: Span,
        /// Exact closing-bracket span.
        close_span: Span,
    },
    /// Slice with optional start and end expressions.
    Slice {
        /// Optional start bound.
        start: Option<Box<Expr>>,
        /// Optional end bound.
        end: Option<Box<Expr>>,
        /// Exact colon span separating the two bounds.
        colon_span: Span,
        /// Exact opening-bracket span.
        open_span: Span,
        /// Exact closing-bracket span.
        close_span: Span,
    },
    /// Node, metadata, or fact accessor introduced by `.@`.
    NodeAccessor {
        /// Accessor selector source form.
        selector: AccessorSelector,
    },
    /// Markup attribute accessor introduced by `.&`.
    Attribute {
        /// Accessor selector source form.
        selector: AccessorSelector,
    },
    /// Postfix error-suppression marker on a non-path expression.
    ErrorSuppression,
}

/// Ordinary object-field selector source form.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum FieldSelector {
    /// Identifier or keyword selector.
    Name(Span),
    /// Quoted selector, including interpolation structure.
    String(StringTemplate),
}

impl FieldSelector {
    /// Exact source span of the selector spelling.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::Name(span) => *span,
            Self::String(template) => template.span(),
        }
    }
}

/// Selector source form used by node and attribute accessors.
///
/// Closed: the walk inventory and the engine lowerer claim totality, so a new variant must fail to compile at every
/// match rather than vanish into `_ =>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AccessorSelector {
    /// Direct identifier or keyword selector after `.@` or `.&`.
    Direct {
        /// Selector token span.
        selector: Span,
    },
    /// Bracketed string selector.
    Bracket {
        /// Selector string token span.
        selector: Span,
        /// Exact opening-bracket span.
        open_span: Span,
        /// Exact closing-bracket span.
        close_span: Span,
    },
    /// Parenthesized dynamic selector expression.
    Dynamic {
        /// Selector expression between parentheses.
        selector: Box<Expr>,
        /// Exact opening-parenthesis span.
        open_span: Span,
        /// Exact closing-parenthesis span.
        close_span: Span,
    },
}

impl AccessorSelector {
    /// Exact source span of the authored selector.
    #[must_use]
    pub const fn selector_span(&self) -> Span {
        match self {
            Self::Direct { selector } | Self::Bracket { selector, .. } => *selector,
            Self::Dynamic { selector, .. } => selector.span(),
        }
    }
}

/// Object member preserving shorthand versus explicit value form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectMember {
    /// Object key source form.
    pub key: ObjectKey,
    /// Explicit value expression; absent for shorthand members.
    pub value: Option<Expr>,
    /// Exact key/value colon span for an explicit member.
    pub colon_span: Option<Span>,
    /// Following member-comma span, including an authored trailing comma.
    pub separator_span: Option<Span>,
    /// Byte range covered by the member.
    pub span: Span,
}

impl ObjectMember {
    pub(crate) fn new(key: ObjectKey, value: Option<Expr>, colon_span: Option<Span>, span: Span) -> Self {
        Self {
            key,
            value,
            colon_span,
            separator_span: None,
            span,
        }
    }
}

/// Object member key source form.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ObjectKey {
    /// Identifier-like key, including keyword keys accepted in object position.
    Name(Span),
    /// String or interpolated string key.
    String(StringTemplate),
    /// Variable shorthand or variable key expression.
    Variable(Span),
    /// Parenthesized dynamic key expression.
    Expr(Box<Expr>),
}

impl ObjectKey {
    /// Byte range covered by the key source form.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Self::Name(span) | Self::Variable(span) => *span,
            Self::String(template) => template.span(),
            Self::Expr(expression) => expression.span(),
        }
    }
}

/// Binding or destructuring pattern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pattern {
    kind: PatternKind,
    location: PatternLocation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PatternLocation {
    Plain(Span),
    Detailed(Box<PatternSpans>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PatternSpans {
    span: Span,
    operator_span: Option<Span>,
    separator_span: Option<Span>,
    close_missing: bool,
}

/// Named punctuation spans of one pattern, in authored order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PatternPunctuation {
    /// Opening delimiter for delimited patterns.
    pub(crate) open: Option<Span>,
    /// Closing delimiter for delimited patterns.
    pub(crate) close: Option<Span>,
    /// Alternative operator (`?//`).
    pub(crate) operator: Option<Span>,
    /// Trailing separator that extends the pattern span.
    pub(crate) separator: Option<Span>,
}

impl Pattern {
    pub(crate) const fn new(kind: PatternKind, span: Span) -> Self {
        Self {
            kind,
            location: PatternLocation::Plain(span),
        }
    }

    pub(crate) fn with_delimiters(mut self, close_span: Span) -> Self {
        debug_assert!(close_span.end() == self.span().end());
        if close_span.is_empty() {
            self.details_mut().close_missing = true;
        }
        self
    }

    pub(crate) fn with_operator(mut self, operator_span: Span) -> Self {
        self.details_mut().operator_span = Some(operator_span);
        self
    }

    pub(crate) fn set_trailing_separator(&mut self, separator_span: Span) {
        self.details_mut().separator_span = Some(separator_span);
    }

    fn details_mut(&mut self) -> &mut PatternSpans {
        if let PatternLocation::Plain(span) = self.location {
            self.location = PatternLocation::Detailed(Box::new(PatternSpans {
                span,
                operator_span: None,
                separator_span: None,
                close_missing: false,
            }));
        }
        match &mut self.location {
            PatternLocation::Plain(_) => unreachable!("plain pattern was promoted"),
            PatternLocation::Detailed(details) => details,
        }
    }

    pub(crate) fn punctuation_spans(&self) -> PatternPunctuation {
        let delimited = matches!(&self.kind, PatternKind::Array(_) | PatternKind::Object(_));
        let details = match &self.location {
            PatternLocation::Plain(_) => None,
            PatternLocation::Detailed(details) => Some(details.as_ref()),
        };
        let open = if delimited {
            Some(Span::new(self.span().start(), self.span().start() + 1))
        } else {
            None
        };
        let close = if delimited {
            Some(close_span_from_container(
                self.span(),
                details.is_some_and(|details| details.close_missing),
            ))
        } else {
            None
        };
        PatternPunctuation {
            open,
            close,
            operator: details.and_then(|details| details.operator_span),
            separator: details.and_then(|details| details.separator_span),
        }
    }

    /// Pattern source form.
    #[must_use]
    pub const fn kind(&self) -> &PatternKind {
        &self.kind
    }

    /// Byte range covered by the pattern source form.
    #[must_use]
    pub const fn span(&self) -> Span {
        match &self.location {
            PatternLocation::Plain(span) => *span,
            PatternLocation::Detailed(details) => details.span,
        }
    }
}

/// Binding pattern forms.
///
/// Closed: the walk inventory and the engine lowerer claim totality, so a new variant must fail to compile at every
/// match rather than vanish into `_ =>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternKind {
    /// Recovery pattern inserted after an error.
    Error,
    /// Variable binding pattern.
    Variable,
    /// ENGINE binding pattern `~x`: binds an engine constructor's cursor to a lexically scoped engine binding. Only `as
    /// ~x` introduces it; the value grammar (`reduce`/`foreach` sources) rejects it at lower time.
    EngineBinding,
    /// Array destructuring pattern.
    Array(Vec<Pattern>),
    /// Object destructuring pattern.
    Object(Vec<ObjectPatternMember>),
    /// Destructuring alternative.
    Alternative(Box<Pattern>, Box<Pattern>),
}

/// Object destructuring member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectPatternMember {
    /// Member key source form.
    pub key: ObjectKey,
    /// Explicit nested pattern; absent for shorthand members.
    pub pattern: Option<Pattern>,
    /// Exact key/pattern colon span for an explicit member.
    pub colon_span: Option<Span>,
    /// Following member-comma span when authored.
    pub separator_span: Option<Span>,
    /// Byte range covered by the member.
    pub span: Span,
}
