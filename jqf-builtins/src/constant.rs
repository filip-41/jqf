//! Compile-time constant evaluation and the shared compile-error vocabulary.
//!
//! One job: evaluate the CONSTANT expressions module metadata and the constant folds need (`evaluate_constant`,
//! `static_template_text`, `lower_number`, with the private `decode_literal_segment`/`constant_object_key` helpers) and
//! own the two data types that evaluation's failure channel and the engine's compile boundary share (`ParseRejection`,
//! `UnsupportedConstruct`). The failure channel here is [`ConstantEvalError`], the three-variant slice of the engine's
//! `EngineCompileError` that constant evaluation actually constructs; the engine maps it onto `EngineCompileError` with
//! a `From` impl, so every lowering `?` site converts without edits.
//!
//! This module exists because the builtin registry's `modulemeta/0` evaluates module metadata at RUNTIME (the registry
//! lives in this crate, the engine's compile does not): the evaluation logic and its error vocabulary must live on the
//! registry's side of the crate boundary. The engine keeps `EngineCompileError` itself — its `Display` names
//! compile-side constants (`SUPPORTED_SYNTAX`, `RegisteredBuiltins`) — and re-exports
//! [`ParseRejection`]/[`UnsupportedConstruct`] so the public `jqf_engine::` surface is unchanged.

use alloc::string::String;
use core::fmt;

use jqf_data::{Array, Number, Value};
use jqf_resource::ResourceError;
use jqf_source::{Diagnostic, Span};
use jqf_syntax::{Expr, ExprKind, ObjectKey, StringTemplate, SyntaxSource, TemplateSegment};

/// First parser diagnostic surfaced through the engine compile boundary.
#[derive(Debug)]
pub struct ParseRejection {
    message: String,
    span: Option<Span>,
}

impl ParseRejection {
    /// The surfaced diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The primary diagnostic span, when the parser attached one.
    #[must_use]
    pub const fn span(&self) -> Option<Span> {
        self.span
    }

    /// Surfaces the first of a parser's diagnostics.
    pub fn from_diagnostics(diagnostics: &[Diagnostic]) -> Self {
        match diagnostics.first() {
            Some(diagnostic) => Self {
                message: try_copy_str(diagnostic.message()).unwrap_or_else(|| String::from("parse failed")),
                span: diagnostic.labels().first().map(jqf_source::Label::span),
            },
            None => Self::internal("parser produced no syntax and no diagnostic"),
        }
    }

    /// Surfaces a bind failure (the syntax source could not be bound).
    pub fn from_bind(error: jqf_syntax::SyntaxSourceError) -> Self {
        Self {
            message: internal_message(&error),
            span: None,
        }
    }

    /// Surfaces one string-decode error at the span it occurred at.
    pub fn decode(error: &jqf_syntax::StringDecodeError, span: Span) -> Self {
        Self {
            message: internal_message(error),
            span: Some(span),
        }
    }

    /// A rejection with a fixed message and no span.
    pub fn internal(message: &'static str) -> Self {
        Self {
            message: try_copy_str(message).unwrap_or_else(|| String::from("internal parse error")),
            span: None,
        }
    }
}

/// A construct outside the static-path bootstrap subset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum UnsupportedConstruct {
    /// Assignment or update over a node/attribute accessor path (`.a.@tag = "x"`, `.a.&href |= f`). The READ half of
    /// the accessor surface lowers to real accessor steps; the WRITE half is deferred — pathmode records no path
    /// component for an accessor step, so it would fail at runtime with the generic path error. Rejected here at
    /// compile time instead.
    AccessorAssignment,
    /// Any other top-level expression form, named for diagnostics.
    Expression(&'static str),
}

impl UnsupportedConstruct {
    /// A stable human phrase naming the construct.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::AccessorAssignment => "assignment/update over a node or attribute accessor path (`.@` / `.&`)",
            Self::Expression(name) => name,
        }
    }
}

/// The failure channel of constant-expression evaluation: the three `EngineCompileError` variants this module's
/// functions actually construct.
///
/// The engine maps this onto `EngineCompileError` with a `From` impl (three mechanical arms), so `lower.rs`'s existing
/// `?` call sites convert without edits. Nothing here references the engine.
#[derive(Debug)]
pub enum ConstantEvalError {
    /// A string-decode or span failure surfaced as a parser rejection.
    Parse(ParseRejection),
    /// A construct outside the constant subset, named with its span.
    Unsupported {
        /// Byte span of the rejected construct.
        span: Span,
        /// The named construct that is not part of the subset.
        construct: UnsupportedConstruct,
    },
    /// The request ledger rejected charging a value the evaluation built.
    Resource(ResourceError),
}

impl ConstantEvalError {
    /// The `Unsupported` constructor.
    pub(crate) fn unsupported(span: Span, construct: UnsupportedConstruct) -> Self {
        Self::Unsupported { span, construct }
    }
}

pub fn try_copy_str(text: &str) -> Option<String> {
    let mut owned = String::new();
    owned.try_reserve_exact(text.len()).ok()?;
    owned.push_str(text);
    Some(owned)
}

pub fn internal_message(error: &impl fmt::Display) -> String {
    use fmt::Write as _;
    let mut message = String::new();
    if write!(message, "{error}").is_err() {
        return String::from("internal parse error");
    }
    message
}

/// Evaluates one constant expression (literals and nested object/array constructors) for module metadata.
pub fn evaluate_constant<'ast>(expr: &'ast Expr, source: &SyntaxSource<'ast>) -> Result<Value, ConstantEvalError> {
    match expr.kind() {
        ExprKind::Null => Ok(Value::Null),
        ExprKind::Bool(value) => Ok(Value::Bool(*value)),
        ExprKind::Number => lower_number(expr.span(), false, source),
        ExprKind::String(template) => match static_template_text(template, source)? {
            Some(text) => {
                Value::try_string(&text).map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))
            }
            None => Err(ConstantEvalError::unsupported(
                expr.span(),
                UnsupportedConstruct::Expression("interpolated module metadata (Module metadata must be constant)"),
            )),
        },
        ExprKind::Group { expression, .. } => evaluate_constant(expression, source),
        ExprKind::Array { expression, .. } => {
            let Some(generator) = expression else {
                let array =
                    Array::try_new().map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))?;
                return Ok(Value::Array(array));
            };
            // A constant array literal is a single constant element expression.
            let value = evaluate_constant(generator, source)?;
            let mut array =
                Array::try_new().map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))?;
            array
                .try_push(value)
                .map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))?;
            Ok(Value::Array(array))
        }
        ExprKind::Object { members, .. } => {
            let mut builder = jqf_data::ObjectBuilder::try_with_capacity(members.len())
                .map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))?;
            for member in members {
                let key = constant_object_key(&member.key, member.span, source)?;
                let Some(value) = member.value.as_ref() else {
                    return Err(ConstantEvalError::unsupported(
                        member.span,
                        UnsupportedConstruct::Expression(
                            "a shorthand module-metadata member (Module metadata must be constant)",
                        ),
                    ));
                };
                let value = evaluate_constant(value, source)?;
                builder
                    .try_insert_last(key, value)
                    .map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))?;
            }
            builder
                .try_finish()
                .map(Value::Object)
                .map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))
        }
        _ => Err(ConstantEvalError::unsupported(
            expr.span(),
            UnsupportedConstruct::Expression(
                "a non-constant module metadata expression (Module metadata must be constant)",
            ),
        )),
    }
}

/// The decoded text of a template that carries NO interpolation, or `None` when a hole makes the text a runtime value.
///
/// Every caller that needs a COMPILE-TIME string — a path key step, a slice bound — turns `None` into its own
/// rejection, because an interpolated key is a dynamic index and an interpolated bound is a dynamic bound. Neither is
/// an unsupported STRING: the string itself lowers fine (see `lower_string_template`), it is the position that cannot
/// take a generator.
pub fn static_template_text(
    template: &StringTemplate,
    source: &SyntaxSource<'_>,
) -> Result<Option<String>, ConstantEvalError> {
    let mut key = String::new();
    for segment in template.segments() {
        match segment {
            TemplateSegment::Literal { span } => decode_literal_segment(*span, source, &mut key)?,
            TemplateSegment::Expression { .. } => return Ok(None),
            _ => {
                return Err(ConstantEvalError::unsupported(
                    segment.span(),
                    UnsupportedConstruct::Expression("an unsupported string key segment"),
                ));
            }
        }
    }
    Ok(Some(key))
}

/// Appends one literal segment's decoded text (the escape set, surrogate pairs included) to `text`.
pub fn decode_literal_segment(
    span: Span,
    source: &SyntaxSource<'_>,
    text: &mut String,
) -> Result<(), ConstantEvalError> {
    jqf_syntax::decode_literal_into(source, source.source_ref(), span, text)
        .map_err(|error| ConstantEvalError::Parse(ParseRejection::decode(&error, span)))
}

/// One constant object-constructor key as an owned object key.
pub fn constant_object_key(
    key: &ObjectKey,
    span: jqf_source::Span,
    source: &SyntaxSource<'_>,
) -> Result<jqf_data::ObjectKey, ConstantEvalError> {
    let text: String = match key {
        ObjectKey::Name(span) => copy_string(
            source
                .text()
                .get(span.range())
                .ok_or_else(|| ConstantEvalError::Parse(ParseRejection::internal("object key span out of range")))?,
        )?,
        ObjectKey::String(template) => match static_template_text(template, source)? {
            Some(text) => text,
            None => {
                return Err(ConstantEvalError::unsupported(
                    span,
                    UnsupportedConstruct::Expression(
                        "an interpolated module-metadata key (Module metadata must be constant)",
                    ),
                ));
            }
        },
        ObjectKey::Variable(_) | ObjectKey::Expr(_) => {
            return Err(ConstantEvalError::unsupported(
                span,
                UnsupportedConstruct::Expression("a dynamic module-metadata key (Module metadata must be constant)"),
            ));
        }
        _ => {
            return Err(ConstantEvalError::unsupported(
                span,
                UnsupportedConstruct::Expression(
                    "an unsupported module-metadata key (Module metadata must be constant)",
                ),
            ));
        }
    };
    jqf_data::ObjectKey::try_from_str(&text).map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))
}

/// One number literal as an owned value.
///
/// `negative` splices the unary-minus sign onto the magnitude's spelling:
/// a constant-folded `-1` lowers through here with the sign carried separately, because the syntax tree owns only the
/// unsigned literal.
pub fn lower_number(span: Span, negative: bool, source: &SyntaxSource<'_>) -> Result<Value, ConstantEvalError> {
    let text = source
        .text()
        .get(span.range())
        .ok_or_else(|| ConstantEvalError::Parse(ParseRejection::internal("number literal span out of range")))?;
    let number = if negative {
        let mut signed = String::new();
        signed
            .try_reserve_exact(text.len() + 1)
            .map_err(|_| ConstantEvalError::Resource(ResourceError::AllocationFailed))?;
        signed.push('-');
        signed.push_str(text);
        Number::try_json_literal(&signed)
    } else {
        Number::try_json_literal(text)
    }
    .map_err(|_| ConstantEvalError::Parse(ParseRejection::internal("number literal outside the supported range")))?;
    Ok(Value::Number(number))
}

fn copy_string(text: &str) -> Result<String, ConstantEvalError> {
    try_copy_str(text).ok_or(ConstantEvalError::Resource(ResourceError::AllocationFailed))
}
