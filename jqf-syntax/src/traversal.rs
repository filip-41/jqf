//! Typed, allocation-conscious traversal of authored syntax.
//!
//! Owns the closed [`SyntaxNodeKind`] inventory (declared from one variant list so the type and `ALL` cannot drift),
//! typed node references with allocation-free child and direct-source-span iteration, and the balanced enter/exit
//! [`SyntaxWalk`]. Every accepted AST form in `ast.rs` has exactly one kind here and one child/span mapping; adding a
//! form means extending the single variant list and teaching `kind()`/`child_at`/ `source_spans` about it.

use alloc::vec::Vec;

use jqf_source::Span;

use crate::ast::{ConditionalBranch, ModuleItem};
use crate::inventory::closed_inventory;
use crate::{
    AccessorSelector, BindingForm, CallArgument, DefItem, DefParameter, Expr, ExprKind, FieldSelector, ImportItem,
    IncludeItem, ObjectKey, ObjectMember, ObjectPatternMember, Pattern, PatternKind, PostfixSegment, PostfixStep,
    SourceItem, SourceUnit, StringTemplate, TemplateSegment,
};

closed_inventory! {
/// Closed inventory of authored syntax node forms.
///
/// A new accepted AST form is exactly one new variant in the single list below: the variant declaration and its
/// membership in [`SyntaxNodeKind::ALL`] both come from that one list, so the inventory cannot drift from the type.
/// [`SyntaxNodeRef`] must then be taught how to expose the form's children and source spans, and the integration
/// suite's inventory walk must reach it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SyntaxNodeKind {
    /// Source-unit root.
    SourceUnit,
    /// Module metadata item.
    Module,
    /// Import item.
    Import,
    /// Include item.
    Include,
    /// Function definition.
    Definition,
    /// Function definition parameter.
    DefinitionParameter,
    /// String-template container.
    StringTemplate,
    /// Literal string-template segment.
    StringLiteralSegment,
    /// Interpolated string-template segment.
    StringInterpolation,
    /// Recovery expression.
    Error,
    /// Identity expression.
    Identity,
    /// Recursive descent.
    RecursiveDescent,
    /// Empty generator.
    Empty,
    /// Null literal.
    Null,
    /// Boolean literal.
    Bool,
    /// Number literal.
    Number,
    /// String expression.
    String,
    /// Variable reference.
    Variable,
    /// Format filter.
    Format,
    /// Format filter applied to a template.
    FormatTemplate,
    /// Parenthesized group.
    Group,
    /// Array construction.
    Array,
    /// Object construction.
    Object,
    /// Unary expression.
    Unary,
    /// Binary expression.
    Binary,
    /// Assignment expression.
    Assignment,
    /// Query-local definition expression.
    DefinitionExpression,
    /// Call expression.
    Call,
    /// Call argument.
    CallArgument,
    /// Postfix expression.
    Postfix,
    /// Conditional expression.
    Conditional,
    /// Conditional branch.
    ConditionalBranch,
    /// Try/catch expression.
    Try,
    /// Reduce expression.
    Reduce,
    /// Foreach expression.
    Foreach,
    /// Lexical binding expression.
    Binding,
    /// Engine-surface term `~name` (engine binding or constructor reference).
    EngineTerm,
    /// Engine-constructor call `~name(args)`.
    EngineCall,
    /// Label expression.
    Label,
    /// Break expression.
    Break,
    /// Field postfix step.
    Field,
    /// Index/iterator postfix step.
    Index,
    /// Slice postfix step.
    Slice,
    /// Node/fact accessor postfix step.
    NodeAccessor,
    /// Attribute accessor postfix step.
    Attribute,
    /// Error-suppression postfix step.
    ErrorSuppression,
    /// Direct field selector.
    FieldNameSelector,
    /// Quoted field selector.
    FieldStringSelector,
    /// Direct accessor selector.
    DirectSelector,
    /// Bracketed accessor selector.
    BracketSelector,
    /// Dynamic accessor selector.
    DynamicSelector,
    /// Object member.
    ObjectMember,
    /// Name object key.
    ObjectKeyName,
    /// String object key.
    ObjectKeyString,
    /// Variable object key.
    ObjectKeyVariable,
    /// Expression object key.
    ObjectKeyExpression,
    /// Recovery pattern.
    PatternError,
    /// Variable pattern.
    PatternVariable,
    /// Engine binding pattern `~x`.
    PatternEngineBinding,
    /// Array pattern.
    PatternArray,
    /// Object pattern.
    PatternObject,
    /// Alternative pattern.
    PatternAlternative,
    /// Object-pattern member.
    ObjectPatternMember,
}
}

/// Typed borrowed reference to one authored syntax node.
#[derive(Clone, Copy, Debug)]
pub enum SyntaxNodeRef<'tree> {
    /// Source-unit root.
    SourceUnit(&'tree SourceUnit),
    /// Module metadata item.
    Module(&'tree ModuleItem),
    /// Import item.
    Import(&'tree ImportItem),
    /// Include item.
    Include(&'tree IncludeItem),
    /// Function definition.
    Definition(&'tree DefItem),
    /// Function definition parameter.
    DefinitionParameter(&'tree DefParameter),
    /// String-template container.
    StringTemplate(&'tree StringTemplate),
    /// String-template segment.
    TemplateSegment(&'tree TemplateSegment),
    /// Expression.
    Expr(&'tree Expr),
    /// Call argument.
    CallArgument(&'tree CallArgument),
    /// Conditional branch.
    ConditionalBranch(&'tree ConditionalBranch),
    /// Postfix step.
    PostfixStep(&'tree PostfixStep),
    /// Ordinary field selector.
    FieldSelector(&'tree FieldSelector),
    /// Node or attribute accessor selector.
    AccessorSelector(&'tree AccessorSelector),
    /// Object member.
    ObjectMember(&'tree ObjectMember),
    /// Object key.
    ObjectKey(&'tree ObjectKey),
    /// Binding pattern.
    Pattern(&'tree Pattern),
    /// Object-pattern member.
    ObjectPatternMember(&'tree ObjectPatternMember),
}

impl<'tree> SyntaxNodeRef<'tree> {
    /// Creates a query-root node reference.
    #[must_use]
    pub const fn query(expression: &'tree Expr) -> Self {
        Self::Expr(expression)
    }

    /// Creates a source-unit-root node reference.
    #[must_use]
    pub const fn source_unit(unit: &'tree SourceUnit) -> Self {
        Self::SourceUnit(unit)
    }

    /// Closed kind of this node.
    #[must_use]
    pub const fn kind(self) -> SyntaxNodeKind {
        match self {
            Self::SourceUnit(_) => SyntaxNodeKind::SourceUnit,
            Self::Module(_) => SyntaxNodeKind::Module,
            Self::Import(_) => SyntaxNodeKind::Import,
            Self::Include(_) => SyntaxNodeKind::Include,
            Self::Definition(_) => SyntaxNodeKind::Definition,
            Self::DefinitionParameter(_) => SyntaxNodeKind::DefinitionParameter,
            Self::StringTemplate(_) => SyntaxNodeKind::StringTemplate,
            Self::TemplateSegment(TemplateSegment::Literal { .. }) => SyntaxNodeKind::StringLiteralSegment,
            Self::TemplateSegment(TemplateSegment::Expression { .. }) => SyntaxNodeKind::StringInterpolation,
            Self::Expr(expression) => expr_kind(expression.kind()),
            Self::CallArgument(_) => SyntaxNodeKind::CallArgument,
            Self::ConditionalBranch(_) => SyntaxNodeKind::ConditionalBranch,
            Self::PostfixStep(step) => postfix_kind(&step.segment),
            Self::FieldSelector(FieldSelector::Name(_)) => SyntaxNodeKind::FieldNameSelector,
            Self::FieldSelector(FieldSelector::String(_)) => SyntaxNodeKind::FieldStringSelector,
            Self::AccessorSelector(AccessorSelector::Direct { .. }) => SyntaxNodeKind::DirectSelector,
            Self::AccessorSelector(AccessorSelector::Bracket { .. }) => SyntaxNodeKind::BracketSelector,
            Self::AccessorSelector(AccessorSelector::Dynamic { .. }) => SyntaxNodeKind::DynamicSelector,
            Self::ObjectMember(_) => SyntaxNodeKind::ObjectMember,
            Self::ObjectKey(ObjectKey::Name(_)) => SyntaxNodeKind::ObjectKeyName,
            Self::ObjectKey(ObjectKey::String(_)) => SyntaxNodeKind::ObjectKeyString,
            Self::ObjectKey(ObjectKey::Variable(_)) => SyntaxNodeKind::ObjectKeyVariable,
            Self::ObjectKey(ObjectKey::Expr(_)) => SyntaxNodeKind::ObjectKeyExpression,
            Self::Pattern(pattern) => pattern_kind(pattern.kind()),
            Self::ObjectPatternMember(_) => SyntaxNodeKind::ObjectPatternMember,
        }
    }

    /// Covering span of this node.
    #[must_use]
    pub fn span(self) -> Span {
        match self {
            Self::SourceUnit(unit) => unit.span,
            Self::Module(item) => item.span,
            Self::Import(item) => item.span,
            Self::Include(item) => item.span,
            Self::Definition(item) => item.span,
            Self::DefinitionParameter(parameter) => parameter
                .separator_span
                .map_or(parameter.name, |separator| parameter.name.merge(separator)),
            Self::StringTemplate(template) => template.span(),
            Self::TemplateSegment(TemplateSegment::Literal { span }) => *span,
            Self::TemplateSegment(TemplateSegment::Expression {
                introducer_span,
                close_span,
                ..
            }) => introducer_span.merge(*close_span),
            Self::Expr(expression) => expression.span(),
            Self::CallArgument(argument) => argument.separator_span.map_or_else(
                || argument.expression.span(),
                |separator| argument.expression.span().merge(separator),
            ),
            Self::ConditionalBranch(branch) => branch.keyword_span.merge(branch.then_branch.span()),
            Self::PostfixStep(step) => step.span,
            Self::FieldSelector(selector) => selector.span(),
            Self::AccessorSelector(selector) => match selector {
                AccessorSelector::Direct { selector } => *selector,
                AccessorSelector::Bracket {
                    open_span,
                    selector,
                    close_span,
                } => open_span.merge(*selector).merge(*close_span),
                AccessorSelector::Dynamic {
                    open_span,
                    selector,
                    close_span,
                } => open_span.merge(selector.span()).merge(*close_span),
            },
            Self::ObjectMember(member) => member
                .separator_span
                .map_or(member.span, |separator| member.span.merge(separator)),
            Self::ObjectKey(key) => key.span(),
            Self::Pattern(pattern) => pattern
                .punctuation_spans()
                .separator
                .map_or_else(|| pattern.span(), |separator| pattern.span().merge(separator)),
            Self::ObjectPatternMember(member) => member
                .separator_span
                .map_or(member.span, |separator| member.span.merge(separator)),
        }
    }

    /// Allocation-free child iterator in authored source order.
    #[must_use]
    pub const fn children(self) -> SyntaxChildren<'tree> {
        SyntaxChildren { parent: self, index: 0 }
    }

    /// Allocation-free iterator over source spans directly owned by this node.
    ///
    /// Child-node spans are deliberately omitted. Recovery insertion spans are preserved as zero-width entries, in
    /// authored order.
    ///
    /// No engine, SDK, or CLI consumer reads this inventory today; it is kept for this crate's tests, examples, and the
    /// syntax-lifecycle fuzz oracle, which checksums the walk. Cut the walkers with that oracle if no production
    /// consumer arrives.
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive match over every node kind is the compile-time source-span inventory"
    )]
    pub fn source_spans(self) -> SourceSpans {
        let mut spans = [None; MAX_OWNED_SOURCE_SPANS];
        match self {
            Self::Expr(expression) => expr_source_spans(expression, &mut spans),
            Self::Module(item) => {
                spans[0] = Some(item.module_keyword_span);
                spans[1] = Some(item.semicolon_span);
            }
            Self::Import(item) => {
                spans[0] = Some(item.import_keyword_span);
                spans[1] = Some(item.as_keyword_span);
                spans[2] = Some(item.alias);
                spans[3] = Some(item.semicolon_span);
            }
            Self::Include(item) => {
                spans[0] = Some(item.include_keyword_span);
                spans[1] = Some(item.semicolon_span);
            }
            Self::Definition(item) => {
                spans[0] = Some(item.def_keyword_span);
                spans[1] = Some(item.name);
                spans[2] = item.parameter_open_span();
                spans[3] = item.parameter_close_span();
                spans[4] = Some(item.colon_span);
                spans[5] = Some(item.semicolon_span);
            }
            Self::DefinitionParameter(parameter) => {
                spans[0] = Some(parameter.name);
                spans[1] = parameter.separator_span;
            }
            Self::StringTemplate(template) => {
                let span = template.span();
                if span.is_empty() {
                    spans[0] = Some(span);
                } else {
                    spans[0] = Some(Span::new(span.start(), span.start() + 1));
                    spans[1] = Some(Span::new(span.end() - 1, span.end()));
                }
            }
            Self::TemplateSegment(TemplateSegment::Literal { span })
            | Self::FieldSelector(FieldSelector::Name(span)) => spans[0] = Some(*span),
            Self::TemplateSegment(TemplateSegment::Expression {
                introducer_span,
                close_span,
                ..
            }) => {
                spans[0] = Some(*introducer_span);
                spans[1] = Some(*close_span);
            }
            Self::CallArgument(argument) => spans[0] = argument.separator_span,
            Self::ConditionalBranch(branch) => {
                spans[0] = Some(branch.keyword_span);
                spans[1] = Some(branch.then_keyword_span);
            }
            Self::PostfixStep(step) => postfix_source_spans(step, &mut spans),
            Self::SourceUnit(_)
            | Self::FieldSelector(FieldSelector::String(_))
            | Self::ObjectKey(ObjectKey::String(_) | ObjectKey::Expr(_)) => {}
            Self::AccessorSelector(AccessorSelector::Direct { selector }) => {
                spans[0] = Some(*selector);
            }
            Self::AccessorSelector(AccessorSelector::Bracket {
                open_span,
                selector,
                close_span,
            }) => {
                spans[0] = Some(*open_span);
                spans[1] = Some(*selector);
                spans[2] = Some(*close_span);
            }
            Self::AccessorSelector(AccessorSelector::Dynamic {
                open_span, close_span, ..
            }) => {
                spans[0] = Some(*open_span);
                spans[1] = Some(*close_span);
            }
            Self::ObjectMember(member) => {
                spans[0] = member.colon_span;
                spans[1] = member.separator_span;
            }
            Self::ObjectKey(ObjectKey::Name(span) | ObjectKey::Variable(span)) => {
                spans[0] = Some(*span);
            }
            Self::Pattern(pattern) => {
                let punctuation = pattern.punctuation_spans();
                if matches!(
                    pattern.kind(),
                    PatternKind::Error | PatternKind::Variable | PatternKind::EngineBinding
                ) {
                    spans[0] = Some(pattern.span());
                    spans[1] = punctuation.separator;
                } else {
                    spans[0] = punctuation.open;
                    spans[1] = punctuation.operator;
                    spans[2] = punctuation.close;
                    spans[3] = punctuation.separator;
                }
            }
            Self::ObjectPatternMember(member) => {
                spans[0] = member.colon_span;
                spans[1] = member.separator_span;
            }
        }
        SourceSpans { spans, index: 0 }
    }
}

/// Maximum number of directly owned source spans any node reports.
///
/// Every `source_spans` arm writes into an array of this size; a form needing more slots grows the constant and its
/// `source_spans` arm together.
const MAX_OWNED_SOURCE_SPANS: usize = 8;

/// Allocation-free iterator over one node's directly owned source spans.
#[derive(Clone, Debug)]
pub struct SourceSpans {
    spans: [Option<Span>; MAX_OWNED_SOURCE_SPANS],
    index: usize,
}

impl Iterator for SourceSpans {
    type Item = Span;

    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.spans.len() {
            let span = self.spans[self.index];
            self.index += 1;
            if span.is_some() {
                return span;
            }
        }
        None
    }
}

/// Allocation-free iterator over one syntax node's immediate children.
#[derive(Clone, Debug)]
pub struct SyntaxChildren<'tree> {
    parent: SyntaxNodeRef<'tree>,
    index: usize,
}

impl<'tree> Iterator for SyntaxChildren<'tree> {
    type Item = SyntaxNodeRef<'tree>;

    fn next(&mut self) -> Option<Self::Item> {
        let child = child_at(self.parent, self.index);
        if child.is_some() {
            self.index += 1;
        }
        child
    }
}

/// Depth-first traversal event.
#[derive(Clone, Copy, Debug)]
pub enum WalkEvent<'tree> {
    /// The walker reached a node before its children.
    Enter(SyntaxNodeRef<'tree>),
    /// The walker finished all children of a node.
    Exit(SyntaxNodeRef<'tree>),
}

#[derive(Clone, Debug)]
struct WalkFrame<'tree> {
    node: SyntaxNodeRef<'tree>,
    children: SyntaxChildren<'tree>,
}

/// Iterative depth-first syntax walker.
///
/// The scratch vector contains exactly one frame per active AST depth.
#[derive(Clone, Debug)]
pub struct SyntaxWalk<'tree> {
    root: Option<SyntaxNodeRef<'tree>>,
    stack: Vec<WalkFrame<'tree>>,
}

impl<'tree> SyntaxWalk<'tree> {
    /// Walks a query expression with root depth one.
    #[must_use]
    pub fn query(expression: &'tree Expr) -> Self {
        Self::new(SyntaxNodeRef::query(expression))
    }

    /// Walks a program or library source unit with root depth one.
    #[must_use]
    pub fn source_unit(unit: &'tree SourceUnit) -> Self {
        Self::new(SyntaxNodeRef::source_unit(unit))
    }

    /// Walks an arbitrary typed syntax node with root depth one.
    fn new(root: SyntaxNodeRef<'tree>) -> Self {
        Self {
            root: Some(root),
            stack: Vec::new(),
        }
    }
}

impl<'tree> Iterator for SyntaxWalk<'tree> {
    type Item = WalkEvent<'tree>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(root) = self.root.take() {
            self.stack.push(WalkFrame {
                node: root,
                children: root.children(),
            });
            return Some(WalkEvent::Enter(root));
        }
        if let Some(child) = self.stack.last_mut().and_then(|frame| frame.children.next()) {
            self.stack.push(WalkFrame {
                node: child,
                children: child.children(),
            });
            return Some(WalkEvent::Enter(child));
        }
        self.stack.pop().map(|frame| WalkEvent::Exit(frame.node))
    }
}

const fn expr_kind(kind: &ExprKind) -> SyntaxNodeKind {
    match kind {
        ExprKind::Error => SyntaxNodeKind::Error,
        ExprKind::Identity => SyntaxNodeKind::Identity,
        ExprKind::RecursiveDescent => SyntaxNodeKind::RecursiveDescent,
        ExprKind::Empty => SyntaxNodeKind::Empty,
        ExprKind::Null => SyntaxNodeKind::Null,
        ExprKind::Bool(_) => SyntaxNodeKind::Bool,
        ExprKind::Number => SyntaxNodeKind::Number,
        ExprKind::String(_) => SyntaxNodeKind::String,
        ExprKind::Variable => SyntaxNodeKind::Variable,
        ExprKind::Format => SyntaxNodeKind::Format,
        ExprKind::FormatTemplate { .. } => SyntaxNodeKind::FormatTemplate,
        ExprKind::Group { .. } => SyntaxNodeKind::Group,
        ExprKind::Array { .. } => SyntaxNodeKind::Array,
        ExprKind::Object { .. } => SyntaxNodeKind::Object,
        ExprKind::Unary(_) => SyntaxNodeKind::Unary,
        ExprKind::Binary(_) => SyntaxNodeKind::Binary,
        ExprKind::Assignment(_) => SyntaxNodeKind::Assignment,
        ExprKind::Definition(_) => SyntaxNodeKind::DefinitionExpression,
        ExprKind::Call(_) => SyntaxNodeKind::Call,
        ExprKind::Postfix(_) => SyntaxNodeKind::Postfix,
        ExprKind::If(_) => SyntaxNodeKind::Conditional,
        ExprKind::Try(_) => SyntaxNodeKind::Try,
        ExprKind::Reduce(_) => SyntaxNodeKind::Reduce,
        ExprKind::Foreach(_) => SyntaxNodeKind::Foreach,
        ExprKind::Binding(_) => SyntaxNodeKind::Binding,
        ExprKind::EngineTerm { .. } => SyntaxNodeKind::EngineTerm,
        ExprKind::EngineCall { .. } => SyntaxNodeKind::EngineCall,
        ExprKind::Label { .. } => SyntaxNodeKind::Label,
        ExprKind::Break { .. } => SyntaxNodeKind::Break,
    }
}

const fn postfix_kind(segment: &PostfixSegment) -> SyntaxNodeKind {
    match segment {
        PostfixSegment::Field { .. } => SyntaxNodeKind::Field,
        PostfixSegment::Index { .. } => SyntaxNodeKind::Index,
        PostfixSegment::Slice { .. } => SyntaxNodeKind::Slice,
        PostfixSegment::NodeAccessor { .. } => SyntaxNodeKind::NodeAccessor,
        PostfixSegment::Attribute { .. } => SyntaxNodeKind::Attribute,
        PostfixSegment::ErrorSuppression => SyntaxNodeKind::ErrorSuppression,
    }
}

const fn pattern_kind(kind: &PatternKind) -> SyntaxNodeKind {
    match kind {
        PatternKind::Error => SyntaxNodeKind::PatternError,
        PatternKind::Variable => SyntaxNodeKind::PatternVariable,
        PatternKind::EngineBinding => SyntaxNodeKind::PatternEngineBinding,
        PatternKind::Array(_) => SyntaxNodeKind::PatternArray,
        PatternKind::Object(_) => SyntaxNodeKind::PatternObject,
        PatternKind::Alternative(_, _) => SyntaxNodeKind::PatternAlternative,
    }
}

fn child_at(parent: SyntaxNodeRef<'_>, index: usize) -> Option<SyntaxNodeRef<'_>> {
    match parent {
        SyntaxNodeRef::SourceUnit(unit) => unit.items.get(index).map(source_item_ref).or_else(|| {
            (index == unit.items.len())
                .then_some(unit.expression.as_ref())
                .flatten()
                .map(SyntaxNodeRef::Expr)
        }),
        SyntaxNodeRef::Module(item) => (index == 0).then_some(SyntaxNodeRef::Expr(&item.metadata)),
        SyntaxNodeRef::Import(item) => match index {
            0 => Some(SyntaxNodeRef::StringTemplate(&item.path)),
            1 => item.metadata.as_ref().map(SyntaxNodeRef::Expr),
            _ => None,
        },
        SyntaxNodeRef::Include(item) => match index {
            0 => Some(SyntaxNodeRef::StringTemplate(&item.path)),
            1 => item.metadata.as_ref().map(SyntaxNodeRef::Expr),
            _ => None,
        },
        SyntaxNodeRef::Definition(item) => item
            .params
            .get(index)
            .map(SyntaxNodeRef::DefinitionParameter)
            .or_else(|| (index == item.params.len()).then_some(SyntaxNodeRef::Expr(&item.body))),
        SyntaxNodeRef::StringTemplate(template) => template.segments().get(index).map(SyntaxNodeRef::TemplateSegment),
        SyntaxNodeRef::TemplateSegment(TemplateSegment::Expression { expression, .. })
        | SyntaxNodeRef::ObjectKey(ObjectKey::Expr(expression)) => {
            (index == 0).then_some(SyntaxNodeRef::Expr(expression))
        }
        SyntaxNodeRef::Expr(expression) => expr_child_at(expression, index),
        SyntaxNodeRef::CallArgument(argument) => (index == 0).then_some(SyntaxNodeRef::Expr(&argument.expression)),
        SyntaxNodeRef::ConditionalBranch(branch) => match index {
            0 => Some(SyntaxNodeRef::Expr(&branch.condition)),
            1 => Some(SyntaxNodeRef::Expr(&branch.then_branch)),
            _ => None,
        },
        SyntaxNodeRef::PostfixStep(step) => postfix_child_at(step, index),
        SyntaxNodeRef::FieldSelector(FieldSelector::String(template))
        | SyntaxNodeRef::ObjectKey(ObjectKey::String(template)) => {
            (index == 0).then_some(SyntaxNodeRef::StringTemplate(template))
        }
        SyntaxNodeRef::AccessorSelector(AccessorSelector::Dynamic { selector, .. }) => {
            (index == 0).then_some(SyntaxNodeRef::Expr(selector))
        }
        SyntaxNodeRef::ObjectMember(member) => match index {
            0 => Some(SyntaxNodeRef::ObjectKey(&member.key)),
            1 => member.value.as_ref().map(SyntaxNodeRef::Expr),
            _ => None,
        },
        SyntaxNodeRef::Pattern(pattern) => pattern_child_at(pattern, index),
        SyntaxNodeRef::ObjectPatternMember(member) => match index {
            0 => Some(SyntaxNodeRef::ObjectKey(&member.key)),
            1 => member.pattern.as_ref().map(SyntaxNodeRef::Pattern),
            _ => None,
        },
        SyntaxNodeRef::DefinitionParameter(_)
        | SyntaxNodeRef::TemplateSegment(TemplateSegment::Literal { .. })
        | SyntaxNodeRef::FieldSelector(FieldSelector::Name(_))
        | SyntaxNodeRef::AccessorSelector(AccessorSelector::Direct { .. } | AccessorSelector::Bracket { .. })
        | SyntaxNodeRef::ObjectKey(ObjectKey::Name(_) | ObjectKey::Variable(_)) => None,
    }
}

fn source_item_ref(item: &SourceItem) -> SyntaxNodeRef<'_> {
    match item {
        SourceItem::Module(item) => SyntaxNodeRef::Module(item),
        SourceItem::Import(item) => SyntaxNodeRef::Import(item),
        SourceItem::Include(item) => SyntaxNodeRef::Include(item),
        SourceItem::Def(item) => SyntaxNodeRef::Definition(item),
    }
}

fn expr_child_at(expression: &Expr, index: usize) -> Option<SyntaxNodeRef<'_>> {
    match expression.kind() {
        ExprKind::Error
        | ExprKind::Identity
        | ExprKind::RecursiveDescent
        | ExprKind::Empty
        | ExprKind::Null
        | ExprKind::Bool(_)
        | ExprKind::Number
        | ExprKind::Variable
        | ExprKind::Format
        | ExprKind::EngineTerm { .. }
        | ExprKind::Break { .. } => None,
        ExprKind::String(template) => (index == 0).then_some(SyntaxNodeRef::StringTemplate(template)),
        ExprKind::FormatTemplate { format, template } => match index {
            0 => Some(SyntaxNodeRef::Expr(format)),
            1 => Some(SyntaxNodeRef::StringTemplate(template)),
            _ => None,
        },
        ExprKind::Group { expression: inner, .. } => (index == 0).then_some(SyntaxNodeRef::Expr(inner)),
        ExprKind::Array { expression, .. } => (index == 0)
            .then_some(expression.as_deref())
            .flatten()
            .map(SyntaxNodeRef::Expr),
        ExprKind::Object { members, .. } => members.get(index).map(SyntaxNodeRef::ObjectMember),
        ExprKind::Unary(unary) => (index == 0).then_some(SyntaxNodeRef::Expr(&unary.expr)),
        ExprKind::Binary(binary) => match index {
            0 => Some(SyntaxNodeRef::Expr(&binary.left)),
            1 => Some(SyntaxNodeRef::Expr(&binary.right)),
            _ => None,
        },
        ExprKind::Assignment(assignment) => match index {
            0 => Some(SyntaxNodeRef::Expr(&assignment.target)),
            1 => Some(SyntaxNodeRef::Expr(&assignment.value)),
            _ => None,
        },
        ExprKind::Definition(definition) => match index {
            0 => Some(SyntaxNodeRef::Definition(&definition.definition)),
            1 => Some(SyntaxNodeRef::Expr(&definition.body)),
            _ => None,
        },
        ExprKind::Call(call) | ExprKind::EngineCall { call, .. } => {
            call.args.get(index).map(SyntaxNodeRef::CallArgument)
        }
        ExprKind::Postfix(postfix) => match index {
            0 => Some(SyntaxNodeRef::Expr(postfix.base())),
            _ => postfix.steps().get(index - 1).map(SyntaxNodeRef::PostfixStep),
        },
        ExprKind::If(conditional) => conditional
            .branches
            .get(index)
            .map(SyntaxNodeRef::ConditionalBranch)
            .or_else(|| {
                (index == conditional.branches.len())
                    .then_some(conditional.else_branch.as_deref())
                    .flatten()
                    .map(SyntaxNodeRef::Expr)
            }),
        ExprKind::Try(value) => match index {
            0 => Some(SyntaxNodeRef::Expr(&value.expr)),
            1 => value.handler.as_deref().map(SyntaxNodeRef::Expr),
            _ => None,
        },
        ExprKind::Reduce(value) | ExprKind::Foreach(value) => match index {
            0 => Some(SyntaxNodeRef::Expr(&value.source)),
            1 => Some(SyntaxNodeRef::Pattern(&value.binding)),
            2 => Some(SyntaxNodeRef::Expr(&value.init)),
            3 => Some(SyntaxNodeRef::Expr(&value.update)),
            4 => value.extract.as_deref().map(SyntaxNodeRef::Expr),
            _ => None,
        },
        ExprKind::Binding(binding) => match binding.form {
            BindingForm::As { .. } => match index {
                0 => Some(SyntaxNodeRef::Expr(&binding.value)),
                1 => Some(SyntaxNodeRef::Pattern(&binding.pattern)),
                2 => Some(SyntaxNodeRef::Expr(&binding.body)),
                _ => None,
            },
            BindingForm::Let { .. } => match index {
                0 => Some(SyntaxNodeRef::Pattern(&binding.pattern)),
                1 => Some(SyntaxNodeRef::Expr(&binding.value)),
                2 => Some(SyntaxNodeRef::Expr(&binding.body)),
                _ => None,
            },
        },
        ExprKind::Label { body, .. } => (index == 0).then_some(SyntaxNodeRef::Expr(body)),
    }
}

fn postfix_child_at(step: &PostfixStep, index: usize) -> Option<SyntaxNodeRef<'_>> {
    match &step.segment {
        PostfixSegment::Field { selector } => (index == 0).then_some(SyntaxNodeRef::FieldSelector(selector)),
        PostfixSegment::Index { index: value, .. } => (index == 0)
            .then_some(value.as_deref())
            .flatten()
            .map(SyntaxNodeRef::Expr),
        PostfixSegment::Slice { start, end, .. } => match index {
            0 => start
                .as_deref()
                .map(SyntaxNodeRef::Expr)
                .or_else(|| end.as_deref().map(SyntaxNodeRef::Expr)),
            1 if start.is_some() => end.as_deref().map(SyntaxNodeRef::Expr),
            _ => None,
        },
        PostfixSegment::NodeAccessor { selector } | PostfixSegment::Attribute { selector } => {
            (index == 0).then_some(SyntaxNodeRef::AccessorSelector(selector))
        }
        PostfixSegment::ErrorSuppression => None,
    }
}

fn pattern_child_at(pattern: &Pattern, index: usize) -> Option<SyntaxNodeRef<'_>> {
    match pattern.kind() {
        PatternKind::Error | PatternKind::Variable | PatternKind::EngineBinding => None,
        PatternKind::Array(items) => items.get(index).map(SyntaxNodeRef::Pattern),
        PatternKind::Object(members) => members.get(index).map(SyntaxNodeRef::ObjectPatternMember),
        PatternKind::Alternative(left, right) => match index {
            0 => Some(SyntaxNodeRef::Pattern(left)),
            1 => Some(SyntaxNodeRef::Pattern(right)),
            _ => None,
        },
    }
}

fn expr_source_spans(expression: &Expr, spans: &mut [Option<Span>; MAX_OWNED_SOURCE_SPANS]) {
    match expression.kind() {
        ExprKind::Identity => {
            let span = expression.span();
            // Implied identity (`.a`, `.@x`, `.&x`) is zero-width at the operator and owns no byte; the spelled `.`
            // still reports itself.
            if !span.is_empty() {
                spans[0] = Some(span);
            }
        }
        ExprKind::Error
        | ExprKind::RecursiveDescent
        | ExprKind::Empty
        | ExprKind::Null
        | ExprKind::Bool(_)
        | ExprKind::Number
        | ExprKind::Variable
        | ExprKind::Format
        | ExprKind::EngineTerm { .. } => spans[0] = Some(expression.span()),
        ExprKind::String(_) | ExprKind::FormatTemplate { .. } | ExprKind::Definition(_) | ExprKind::Postfix(_) => {}
        ExprKind::Group {
            open_span, close_span, ..
        }
        | ExprKind::Array {
            open_span, close_span, ..
        }
        | ExprKind::Object {
            open_span, close_span, ..
        } => {
            spans[0] = Some(*open_span);
            spans[1] = Some(*close_span);
        }
        ExprKind::Unary(unary) => spans[0] = Some(unary.op_span),
        ExprKind::Binary(binary) => spans[0] = Some(binary.op_span),
        ExprKind::Assignment(assignment) => spans[0] = Some(assignment.op_span),
        ExprKind::Call(call) => {
            spans[0] = Some(call.name);
            spans[1] = call.open_parenthesis_span();
            spans[2] = call.close_parenthesis_span();
        }
        ExprKind::EngineCall { tilde_span, call } => {
            spans[0] = Some(*tilde_span);
            spans[1] = Some(call.name);
            spans[2] = call.open_parenthesis_span();
            spans[3] = call.close_parenthesis_span();
        }
        ExprKind::If(conditional) => {
            spans[0] = conditional.else_keyword_span;
            spans[1] = Some(conditional.end_keyword_span);
        }
        ExprKind::Try(value) => {
            spans[0] = Some(value.try_keyword_span);
            spans[1] = value.catch_keyword_span;
        }
        ExprKind::Reduce(value) | ExprKind::Foreach(value) => {
            spans[0] = Some(value.keyword_span);
            spans[1] = Some(value.as_keyword_span);
            spans[2] = Some(value.open_span);
            spans[3] = Some(value.update_separator_span);
            spans[4] = value.extract_separator_span;
            spans[5] = Some(value.close_span);
        }
        ExprKind::Binding(binding) => match binding.form {
            BindingForm::As {
                as_keyword_span,
                pipe_span,
            } => {
                spans[0] = Some(as_keyword_span);
                spans[1] = Some(pipe_span);
            }
            BindingForm::Let {
                let_keyword_span,
                equals_span,
                pipe_span,
            } => {
                spans[0] = Some(let_keyword_span);
                spans[1] = Some(equals_span);
                spans[2] = Some(pipe_span);
            }
        },
        ExprKind::Label {
            label_keyword_span,
            label,
            pipe_span,
            ..
        } => {
            spans[0] = Some(*label_keyword_span);
            spans[1] = Some(*label);
            spans[2] = Some(*pipe_span);
        }
        ExprKind::Break {
            break_keyword_span,
            label,
        } => {
            spans[0] = Some(*break_keyword_span);
            spans[1] = Some(*label);
        }
    }
}

fn postfix_source_spans(step: &PostfixStep, spans: &mut [Option<Span>; MAX_OWNED_SOURCE_SPANS]) {
    match &step.segment {
        PostfixSegment::Field { .. }
        | PostfixSegment::NodeAccessor { .. }
        | PostfixSegment::Attribute { .. }
        | PostfixSegment::ErrorSuppression => {
            spans[0] = Some(step.operator_span);
            spans[1] = step.optional_suffix_span;
        }
        PostfixSegment::Index {
            open_span, close_span, ..
        } => {
            let next = bracket_introducer(step, *open_span, spans);
            spans[next] = Some(*open_span);
            spans[next + 1] = Some(*close_span);
            spans[next + 2] = step.optional_suffix_span;
        }
        PostfixSegment::Slice {
            colon_span,
            open_span,
            close_span,
            ..
        } => {
            let next = bracket_introducer(step, *open_span, spans);
            spans[next] = Some(*open_span);
            spans[next + 1] = Some(*colon_span);
            spans[next + 2] = Some(*close_span);
            spans[next + 3] = step.optional_suffix_span;
        }
    }
}

/// Records the `.` that introduces a bracketed step, and returns the next free inventory slot.
///
/// A bracketed step written straight onto its base (`.[1]`, `[0][1:2]`) has no introducer of its own: its operator span
/// IS the opening bracket, and the bracket is reported once. A dot may introduce it instead (`.a.[1]`, `$x.[]`,
/// `."quoted".[0]`), and then the dot is a second authored token that no other node owns — it leads the inventory,
/// ahead of the bracket, in authored order.
fn bracket_introducer(
    step: &PostfixStep,
    open_span: Span,
    spans: &mut [Option<Span>; MAX_OWNED_SOURCE_SPANS],
) -> usize {
    if step.operator_span == open_span {
        return 0;
    }
    spans[0] = Some(step.operator_span);
    1
}

#[cfg(test)]
mod tests {
    use alloc::{collections::BTreeSet, format};

    use super::*;

    #[test]
    fn all_inventory_members_have_distinct_canonical_names() {
        let mut names = BTreeSet::new();
        for kind in SyntaxNodeKind::ALL {
            assert!(
                names.insert(format!("{kind:?}")),
                "duplicate canonical name in ALL: {kind:?}"
            );
        }
    }
}
