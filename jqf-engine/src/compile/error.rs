//! Compile failures and the rejection-message surface they print.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Structured failure to compile a program.
#[derive(Debug)]
#[non_exhaustive]
pub enum EngineCompileError {
    /// Program source exceeded the syntax input bound before parsing.
    Input(SyntaxInputError),
    /// The syntax parser reported one or more diagnostics.
    Parse(ParseRejection),
    /// A recognized construct the compiler does not lower.
    Unsupported {
        /// Byte span of the rejected construct.
        span: Span,
        /// The named construct that is not part of the subset.
        construct: UnsupportedConstruct,
    },
    /// A call named no registered builtin overload at its arity (the
    /// `name/arity is not defined` compile error). This is a resolution failure,
    /// not an out-of-subset rejection: the call grammar is recognized, but no
    /// `(name, arity)` matches the registry.
    UndefinedCall {
        /// Byte span of the call name.
        span: Span,
        /// The unresolved call name, as authored.
        name: String,
        /// The call's authored arity.
        arity: u8,
    },
    /// A call whose argument count passes the registry's one-byte arity
    /// ceiling. No overload can ever resolve at such a count, so the
    /// rejection names the AUTHORED count rather than a clamped one.
    ArityLimit {
        /// Byte span of the call name.
        span: Span,
        /// The call name, as authored.
        name: String,
        /// The authored argument count.
        count: u32,
    },
    /// A `$x` reference with no enclosing binder (the `$x is not defined`
    /// compile error, exit-3 class). Like [`Self::UndefinedCall`] this is a
    /// RESOLUTION failure, not an out-of-subset rejection: the grammar is
    /// recognized, but no lexical scope binds the name.
    UndefinedVariable {
        /// Byte span of the variable reference.
        span: Span,
        /// The unresolved variable name, as authored (including the `$`).
        name: String,
    },
    /// A `~x` engine-binding reference with no enclosing `as ~x` binder.
    /// Resolution failure in the ENGINE namespace, kept distinct from
    /// [`Self::UndefinedVariable`] because the two namespaces never collide.
    UndefinedEngineBinding {
        /// Byte span of the engine-binding reference, including the `~`.
        span: Span,
        /// The unresolved engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// A BARE engine binding `~x` reached a VALUE position: `~x` alone, `[~x]`,
    /// `~x | .`, `~x | tostring`. The engine binding never crosses into the
    /// value domain — the only projections that lower to values are `~x.next`
    /// and `~x.rest`, and this guard is the reason the boundary is un-crossable
    /// by construction.
    EngineBindingAsValue {
        /// Byte span of the offending reference, including the `~`.
        span: Span,
        /// The engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// A postfix chain on an engine binding that is not exactly one of the two
    /// protocol projections: `~x.next.foo`, `~x[0]`, `~x.next[0]`. The protocol
    /// is CLOSED — `.next` and `.rest` are the only projections that exist, and
    /// each is a complete expression on its own.
    EngineBindingProjection {
        /// Byte span of the offending postfix chain.
        span: Span,
        /// The engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// An engine-constructor call `~name(...)` whose name is not a registered
    /// engine constructor (the constructor list is CLOSED, and the only
    /// resident is `~generator`).
    UndefinedEngineConstructor {
        /// Byte span of the constructor reference, including the `~`.
        span: Span,
        /// The unresolved constructor name, as authored.
        name: String,
    },
    /// An engine constructor bound to a VALUE pattern, or an engine binding
    /// whose value is not an engine constructor: `~generator(...) as $x` and
    /// `(1,2) as ~x`. The two-mark rule — both the binding AND the constructor
    /// carry `~` — makes the value/engine boundary un-crossable by construction.
    EngineBindingShape {
        /// Byte span of the offending form.
        span: Span,
        /// Why the form is rejected, rendered into the message.
        reason: alloc::string::String,
    },
    /// A pull of an engine binding from inside a RECURSIVE user `def` body (the
    /// carve-out): the callable body runs on a NESTED evaluator
    /// with no cursor store, so the pull is rejected at COMPILE time — never a
    /// hang, never a silent wrong answer. The loop idiom is written with
    /// `while`/`until`/`repeat`/`recurse` instead, which are frame-machine
    /// generators and can pull.
    EnginePullInRecursiveDef {
        /// Byte span of the offending pull.
        span: Span,
        /// The engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// An engine-binding reference inside a `~generator` constructor argument:
    /// the constructor's cursor is a SEPARATELY-OWNED machine, and its graphs
    /// cannot capture a cursor owned by the enclosing machine (cross-machine
    /// capture is impossible without a shared cursor store).
    EngineBindingInConstructor {
        /// Byte span of the offending reference.
        span: Span,
        /// The engine-binding name, as authored (including the `~`).
        name: String,
    },
    /// An engine binding used as a `reduce`/`foreach` loop pattern (`reduce SRC
    /// as ~x (...)`): loops bind VALUES; only `as ~x` introduces a cursor.
    EngineBindingLoopPattern {
        /// Byte span of the offending pattern.
        span: Span,
    },
    /// A `break $x` with no enclosing `label $x` (the `$*label-x is not
    /// defined` compile error, exit-3 class). Kept distinct from
    /// [`Self::UndefinedVariable`] because labels are a SEPARATE namespace: it
    /// is named `$*label-x`, and reporting it as a plain variable would misstate
    /// which stack the lookup missed.
    UndefinedLabel {
        /// Byte span of the label reference.
        span: Span,
        /// The unresolved label name, as authored (including the `$`).
        name: String,
    },
    /// Program structure nested deeper than the lowering walk will descend.
    ///
    /// The parser bounds the tree it builds at the same ceiling, so this is
    /// what catches the trees lowering reaches by another route: a `def` body
    /// is re-lowered at every call site, and a filter argument is re-lowered
    /// inside the callee, so a shallow-looking program can still present a
    /// deeper walk than any single parse did.
    ///
    /// It is a statement about the PROGRAM (the exit-3 class), not about the
    /// host's resources, which is why it is not a [`Self::Resource`] arm even
    /// though it shares the ledger's ceiling and its wording.
    NestingTooDeep {
        /// Byte span of the expression one level past the ceiling.
        span: Span,
        /// The ceiling, in levels.
        limit: u32,
    },
    /// The request ledger refused an arena allocation for the compiled program.
    Resource(ResourceError),
    /// An `include`/`import` chain re-entered a module already on the loading
    /// stack (the `circular import of …` compile error, exit-3 class).
    CircularImport {
        /// Byte span of the `include`/`import` item that closed the cycle.
        span: Span,
        /// The resolved module label that was imported twice on the stack.
        label: String,
    },
    /// `--split-exp` cannot carry `--arg`/`--argjson` bindings: the split
    /// expression is standalone and `$index` is its only reserved name.
    SplitExpWithCliVars,
    /// Embedded prelude initialization panicked; later compiles refuse rather
    /// than wait forever on a state that will never become ready.
    PreludeInitFailed,
}

impl EngineCompileError {
    pub(crate) fn unsupported(span: Span, construct: UnsupportedConstruct) -> Self {
        Self::Unsupported { span, construct }
    }

    pub(crate) fn undefined_call(span: Span, name: &str, arity: u8) -> Self {
        Self::UndefinedCall {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("<call>")),
            arity,
        }
    }

    pub(crate) fn arity_limit(span: Span, name: &str, count: u32) -> Self {
        Self::ArityLimit {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("<call>")),
            count,
        }
    }

    pub(crate) fn undefined_variable(span: Span, name: &str) -> Self {
        Self::UndefinedVariable {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("$<variable>")),
        }
    }

    pub(crate) fn undefined_engine_binding(span: Span, name: &str) -> Self {
        Self::UndefinedEngineBinding {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    pub(crate) fn engine_binding_as_value(span: Span, name: &str) -> Self {
        Self::EngineBindingAsValue {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    pub(crate) fn engine_binding_projection(span: Span, name: &str) -> Self {
        Self::EngineBindingProjection {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    pub(crate) fn engine_pull_in_recursive_def(span: Span, name: &str) -> Self {
        Self::EnginePullInRecursiveDef {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    pub(crate) fn engine_binding_in_constructor(span: Span, name: &str) -> Self {
        Self::EngineBindingInConstructor {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("~<binding>")),
        }
    }

    pub(crate) fn undefined_label(span: Span, name: &str) -> Self {
        Self::UndefinedLabel {
            span,
            name: try_copy_str(name).unwrap_or_else(|| String::from("$<label>")),
        }
    }

    pub(crate) fn circular_import(span: Span, label: &str) -> Self {
        Self::CircularImport {
            span,
            label: try_copy_str(label).unwrap_or_else(|| String::from("<module>")),
        }
    }

    /// The program-source span the rejection points at, when one exists.
    ///
    /// Every rejection ABOUT the program text carries one; the two arms with
    /// none are statements about the input size and the host's resources.
    /// This is the accessor a presenter (the CLI's caret excerpt) reads —
    /// the Display text keeps the byte offsets, so a consumer with no source
    /// in hand loses nothing.
    #[must_use]
    pub const fn span(&self) -> Option<Span> {
        match self {
            Self::Input(_) | Self::Resource(_) | Self::SplitExpWithCliVars | Self::PreludeInitFailed => None,
            Self::Parse(rejection) => rejection.span(),
            Self::Unsupported { span, .. }
            | Self::UndefinedCall { span, .. }
            | Self::ArityLimit { span, .. }
            | Self::UndefinedVariable { span, .. }
            | Self::UndefinedEngineBinding { span, .. }
            | Self::EngineBindingAsValue { span, .. }
            | Self::EngineBindingProjection { span, .. }
            | Self::UndefinedEngineConstructor { span, .. }
            | Self::EngineBindingShape { span, .. }
            | Self::EnginePullInRecursiveDef { span, .. }
            | Self::EngineBindingInConstructor { span, .. }
            | Self::EngineBindingLoopPattern { span, .. }
            | Self::UndefinedLabel { span, .. }
            | Self::NestingTooDeep { span, .. }
            | Self::CircularImport { span, .. } => Some(*span),
        }
    }
}

impl From<jqf_builtins::constant::ConstantEvalError> for EngineCompileError {
    fn from(error: jqf_builtins::constant::ConstantEvalError) -> Self {
        match error {
            jqf_builtins::constant::ConstantEvalError::Parse(rejection) => Self::Parse(rejection),
            jqf_builtins::constant::ConstantEvalError::Unsupported { span, construct } => {
                Self::Unsupported { span, construct }
            }
            jqf_builtins::constant::ConstantEvalError::Resource(error) => Self::Resource(error),
        }
    }
}

/// The SYNTAX half of the rejection message's capability enumeration.
///
/// It states what the engine actually runs, in the order the grammar landed
/// it. It is prose because lowering has no single source of truth to generate
/// it from — `lower_expr` is a match over the AST, not a table — so it is
/// guarded instead: `the_message_names_only_forms_that_compile` compiles one
/// probe program per named form and fails if a claim here has stopped being
/// true. A vertical that widens the grammar widens this string in the same
/// commit, and one that narrows it is caught by the probe.
///
/// Deliberately NOT listed, because it is still rejected at lower time: a
/// `?//` chain in a THREE-argument `foreach`.
const SUPPORTED_SYNTAX: &str = "identity `.`, field and index paths (`.a`, `.\"k\"`, `.[\"k\"]`, \
     `.[0]`, `.[-1]`) with per-component `?`, `.[]` iteration, recursive descent `..`, slices \
     `.[e1:e2]`, dynamic indexing `.[e]`, unary negation `-`, \
     pipe `|`, comma `,`, \
     parenthesized groups, literals, `[…]`/`{…}` constructors, arithmetic `+ - * / %`, \
     comparisons `== != < <= > >=`, `and`/`or`, the alternative `//`, `if`/`elif`/`else`, \
     `try`/`catch` and its `?` sugar, `as` bindings over a variable or a \
     destructuring pattern (`as [$a,$b]`, `as {a:$v,$b}`) and its `?//` \
     alternative chain, `reduce`, `foreach`, \
     `label $out`/`break $out`, and assignment `=`, update `|=`, and the \
     arithmetic/alternative updates `+= -= *= /= %= //=`";

/// The BUILTIN half of the same enumeration, GENERATED from the registry.
///
/// This is deliberately not prose. `registry::builtin_overloads()` is the
/// single source of truth for which `(name, arity)` pairs resolve, so rendering
/// from it makes the clause honest by construction: a vertical that registers
/// an overload widens this message with no second edit, and the message can
/// never again claim a narrower surface than the engine has. The stale
/// enumeration this replaced named four builtins when seven resolved.
struct RegisteredBuiltins;

impl fmt::Display for RegisteredBuiltins {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (position, overload) in jqf_builtins::registry::builtin_overloads().iter().enumerate() {
            if position > 0 {
                formatter.write_str(", ")?;
            }
            write!(formatter, "`{}/{}`", overload.canonical_name, overload.arity)?;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_lines)]
impl fmt::Display for EngineCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(error) => write!(formatter, "program source rejected: {error}"),
            Self::Parse(rejection) => match rejection.span() {
                Some(span) => write!(
                    formatter,
                    "cannot parse program at bytes {}..{}: {}",
                    span.start(),
                    span.end(),
                    rejection.message()
                ),
                None => write!(formatter, "cannot parse program: {}", rejection.message()),
            },
            Self::Unsupported { span, construct } => {
                // Every rejection that names ONE exact spelling leads with its
                // own sentence and skips the supported-surface dump — a
                // multi-hundred-word wall is the worst first contact with the
                // first carve-out; the same law now covers
                // the accessor family, whose `describe()`
                // texts name the construct and the spellings that DO compile
                // (`jqf --help facts` is the discovery surface for the
                // accessor spellings). Only the generic `Expression(name)`
                // forms keep the full enumeration, which frames an unexpected
                // rejection with what IS supported.
                if matches!(*construct, UnsupportedConstruct::AccessorAssignment) {
                    return write!(
                        formatter,
                        "unsupported construct at bytes {}..{}: {}",
                        span.start(),
                        span.end(),
                        construct.describe()
                    );
                }
                write!(
                    formatter,
                    "unsupported construct at bytes {}..{}: {} is outside the supported \
                     surface ({SUPPORTED_SYNTAX}; builtins: {RegisteredBuiltins})",
                    span.start(),
                    span.end(),
                    construct.describe()
                )
            }
            Self::UndefinedCall { span, name, arity } => write!(
                formatter,
                "{name}/{arity} is not defined at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::ArityLimit { span, name, count } => write!(
                formatter,
                "{name}/{count} is not defined: a call can have at most 255 arguments \
                 at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::UndefinedVariable { span, name } => write!(
                formatter,
                "{name} is not defined at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            // The engine namespace is separate from the variable one, but the
            // resolution failure is the same class (the exit-3 compile error).
            Self::UndefinedEngineBinding { span, name } => write!(
                formatter,
                "{name} is not defined (no `as {name}` binder is in scope) at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingAsValue { span, name } => write!(
                formatter,
                "{name} cannot be returned as a value; use `{name}.next` or `{name}.rest` \
                 at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingProjection { span, name } => write!(
                formatter,
                "{name} has no such projection; the only engine-binding projections are \
                 `{name}.next` and `{name}.rest` at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::UndefinedEngineConstructor { span, name } => write!(
                formatter,
                "{name} is not an engine constructor; the engine constructors are \
                 `~generator`, `~cursor`, `~inputs`, and `~rng` at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingShape { span, reason } => {
                write!(formatter, "{reason} at bytes {}..{}", span.start(), span.end())
            }
            Self::EnginePullInRecursiveDef { span, name } => write!(
                formatter,
                "{name} cannot be pulled from inside a recursive `def` body (the \
                 carve-out); write the loop with `while`/`until`/`repeat`/`recurse` instead, \
                 at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingInConstructor { span, name } => write!(
                formatter,
                "{name} cannot be captured by a `~generator` argument (a cursor's own \
                 machine cannot pull a cursor of the enclosing machine) at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::EngineBindingLoopPattern { span } => write!(
                formatter,
                "an engine binding cannot be a `reduce`/`foreach` loop pattern; only \
                 `as ~x` introduces a cursor at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::UndefinedLabel { span, name } => write!(
                formatter,
                "$*label-{} is not defined at bytes {}..{}",
                name.strip_prefix('$').unwrap_or(name),
                span.start(),
                span.end(),
            ),
            // Spelled exactly as the parser's own refusal and the resource
            // ledger's, because it is the same fact reached from a third place.
            Self::NestingTooDeep { span, limit } => write!(
                formatter,
                "nesting depth limit exceeded: the ceiling is {limit} levels, at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::CircularImport { span, label } => write!(
                formatter,
                "circular import of {label} at bytes {}..{}",
                span.start(),
                span.end(),
            ),
            Self::SplitExpWithCliVars => {
                write!(formatter, "`--split-exp` cannot be combined with CLI variable bindings")
            }
            Self::PreludeInitFailed => write!(formatter, "embedded prelude initialization failed"),
            // The resource error renders through its OWN Display — the
            // jqf-resource prose law: Debug on `ResourceError` is a
            // Rust struct literal (`LimitExceeded { limit_kind: MemoryBytes,
            // .. }`), and the CLI routes this message to exit 2, so a user
            // with a tiny `--max-memory-bytes` must read a sentence, not a
            // type.
            Self::Resource(error) => {
                write!(formatter, "cannot allocate compiled program: {error}")
            }
        }
    }
}
