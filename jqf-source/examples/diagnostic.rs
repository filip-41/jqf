//! A diagnostic that points into a source: spans, refs, and labels together.
//!
//! Run with `cargo run -p jqf-source --example diagnostic`.

use jqf_source::{
    Diagnostic, DiagnosticSource, Label, Namespace, ResolvedSource, Severity, SourceId, SourceKind, SourceRef, Span,
};

fn main() {
    let input = SourceRef::new(SourceId::new(0), SourceKind::Input);
    let bytes = b"{\"price\": \"ninety\"}";
    let resolved = ResolvedSource::new(input, "prices.json", bytes, 0);

    let code = Namespace::new("demo").code("input.unquoted-number");
    let value = Span::from_usize(11, 17);
    let quote_hint = Span::new(value.start(), value.start());

    let diagnostic = Diagnostic::new(code, Severity::Error, "expected a number")
        .with_source(DiagnosticSource::new(input, resolved.label(), resolved.base_offset()))
        .with_label(Label::primary(input, value, "this is a string"))
        .with_label(Label::secondary(input, quote_hint, "a number needs no quotes"));

    assert_eq!(diagnostic.code().to_string(), "demo.input.unquoted-number");
    assert_eq!(resolved.bytes()[value.range()].len(), value.len() as usize);
    assert_eq!(diagnostic.labels().len(), 2);
    assert_eq!(diagnostic.severity(), Severity::Error);

    println!("{code}: {}", diagnostic.message());
    for label in diagnostic.labels() {
        let text = &resolved.bytes()[label.span().range()];
        println!(
            "  {:?} {} @ {}: {}",
            label.style(),
            resolved.label(),
            label.span(),
            String::from_utf8_lossy(text),
        );
    }
}
