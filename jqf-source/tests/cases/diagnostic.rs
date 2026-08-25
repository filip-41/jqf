//! Diagnostic construction contracts: code grammar, label fields, severity order, insertion order, and the
//! fallible-builder success path. Source identity and span boundaries are pinned by the sibling case modules.

use jqf_source::{
    Diagnostic, DiagnosticSource, Label, LabelStyle, Namespace, Severity, SourceId, SourceKind, SourceRef, Span,
};

#[test]
fn diagnostic_codes_are_namespace_qualified() {
    const JSON: Namespace = Namespace::new("json");
    let code = JSON.code("trailing-comma");

    assert_eq!(code.namespace(), JSON);
    assert_eq!(code.name(), "trailing-comma");
    assert_eq!(code.to_string(), "json.trailing-comma");
}

#[test]
fn diagnostic_code_constructors_accept_full_segment_alphabet() {
    const ENGINE_2: Namespace = Namespace::new("engine_2-runtime");
    let code = ENGINE_2.code("runtime-error.v2_ok");

    assert_eq!(ENGINE_2.name(), "engine_2-runtime");
    assert_eq!(code.name(), "runtime-error.v2_ok");
    assert_eq!(code.to_string(), "engine_2-runtime.runtime-error.v2_ok");
}

#[test]
#[should_panic(expected = "invalid diagnostic namespace")]
fn namespace_new_panics_for_invalid_namespace() {
    let _ = Namespace::new("engine.runtime");
}

/// Pins the empty middle-segment rejection of `Namespace::code` (`is_valid_code_name`).
#[test]
#[should_panic(expected = "invalid diagnostic code name")]
fn namespace_code_panics_for_invalid_code_name() {
    const ENGINE: Namespace = Namespace::new("engine");
    let _ = ENGINE.code("runtime..error");
}

/// Pins the empty-input rejection of [`Namespace::new`] (`is_valid_segment`).
#[test]
#[should_panic(expected = "invalid diagnostic namespace")]
fn namespace_new_panics_for_empty_namespace() {
    let _ = Namespace::new("");
}

/// Pins the invalid-byte rejection of [`Namespace::new`] (uppercase is not a segment byte).
#[test]
#[should_panic(expected = "invalid diagnostic namespace")]
fn namespace_new_panics_for_uppercase_byte() {
    let _ = Namespace::new("Engine");
}

/// Pins the empty-input rejection of `Namespace::code` (`is_valid_code_name`).
#[test]
#[should_panic(expected = "invalid diagnostic code name")]
fn namespace_code_panics_for_empty_name() {
    const ENGINE: Namespace = Namespace::new("engine");
    let _ = ENGINE.code("");
}

/// Pins the trailing-dot rejection of `Namespace::code` (empty final segment), distinct from the empty middle segment
/// pinned above.
#[test]
#[should_panic(expected = "invalid diagnostic code name")]
fn namespace_code_panics_for_trailing_dot() {
    const ENGINE: Namespace = Namespace::new("engine");
    let _ = ENGINE.code("error.");
}

#[test]
fn diagnostics_preserve_labels() {
    const PARSE: Namespace = Namespace::new("parse");
    let source = SourceRef::new(SourceId::new(1), SourceKind::Query);
    let span = Span::new(0, 1);
    let diagnostic = Diagnostic::new(PARSE.code("expected-name"), Severity::Error, "expected a name")
        .with_label(Label::primary(source, span, "here"));

    assert_eq!(diagnostic.code().to_string(), "parse.expected-name");
    assert_eq!(diagnostic.severity(), Severity::Error);
    assert_eq!(diagnostic.message(), "expected a name");
    assert_eq!(diagnostic.labels()[0].style(), LabelStyle::Primary);
    assert_eq!(diagnostic.labels()[0].source(), source);
    assert_eq!(diagnostic.labels()[0].span(), span);
    assert_eq!(diagnostic.labels()[0].message(), "here");
}

#[test]
fn diagnostics_preserve_source_metadata() {
    const JSON: Namespace = Namespace::new("json");
    let source = SourceRef::new(SourceId::new(9), SourceKind::Input);
    let diagnostic = Diagnostic::new(JSON.code("invalid-utf8"), Severity::Error, "invalid UTF-8")
        .with_source(DiagnosticSource::new(source, "stdin", 4_096))
        .with_source(DiagnosticSource::new(source, "standard input", 8_192))
        .with_label(Label::primary(source, Span::new(7, 7), "here"));

    assert_eq!(diagnostic.sources().len(), 2);
    assert_eq!(diagnostic.sources()[0].source(), source);
    assert_eq!(diagnostic.sources()[0].label(), "stdin");
    assert_eq!(diagnostic.sources()[0].base_offset(), 4_096);
    assert_eq!(diagnostic.sources()[1].source(), source);
    assert_eq!(diagnostic.sources()[1].label(), "standard input");
    assert_eq!(diagnostic.sources()[1].base_offset(), 8_192);
    assert_eq!(
        diagnostic.sources()[0].base_offset() + u64::from(diagnostic.labels()[0].span().start()),
        4_103
    );
}

#[test]
fn secondary_labels_retain_style_source_span_and_message() {
    let source = SourceRef::new(SourceId::new(4), SourceKind::Input);
    let span = Span::new(1, 2);
    let label = Label::secondary(source, span, "context");

    assert_eq!(label.style(), LabelStyle::Secondary);
    assert_eq!(label.source(), source);
    assert_eq!(label.span(), span);
    assert_eq!(label.message(), "context");
}

/// Pins the severity ladder `Error < Warning < Info < Trace`.
#[test]
fn severity_orders_error_below_warning_info_and_trace() {
    assert!(Severity::Error < Severity::Warning);
    assert!(Severity::Warning < Severity::Info);
    assert!(Severity::Info < Severity::Trace);
}

/// Every `try_*` constructor and builder step yields `Some` when the allocator takes the reservation, and the chained
/// form keeps the source and label it attached. Allocator refusal is not exercised in this crate.
#[test]
fn fallible_diagnostic_builders_chain_when_allocation_succeeds() {
    const PARSE: Namespace = Namespace::new("parse");
    let source = SourceRef::new(SourceId::new(3), SourceKind::Input);
    let span = Span::new(0, 1);
    let built = Diagnostic::try_new(PARSE.code("bad"), Severity::Warning, "boom")
        .and_then(|diagnostic| diagnostic.try_with_source(DiagnosticSource::try_new(source, "input#0", 0)?))
        .and_then(|diagnostic| diagnostic.try_with_label(Label::try_primary(source, span, "here")?))
        .expect("every reservation on the success path is taken");

    assert_eq!(built.sources()[0].source(), source);
    assert_eq!(built.sources()[0].label(), "input#0");
    let label = &built.labels()[0];
    assert_eq!(
        (label.source(), label.span(), label.style()),
        (source, span, LabelStyle::Primary)
    );
    assert_eq!(label.message(), "here");
}

#[test]
fn diagnostic_builder_preserves_insertion_order() {
    const ENGINE: Namespace = Namespace::new("engine");
    let source = SourceRef::new(SourceId::new(2), SourceKind::Input);
    let first = Span::new(0, 1);
    let second = Span::new(2, 3);

    let diagnostic = Diagnostic::new(ENGINE.code("runtime"), Severity::Warning, "runtime issue")
        .with_label(Label::primary(source, first, "first"))
        .with_label(Label::secondary(source, second, "second"));

    assert_eq!(diagnostic.labels()[0].message(), "first");
    assert_eq!(diagnostic.labels()[1].style(), LabelStyle::Secondary);
}
