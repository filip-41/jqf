use alloc::string::String;

use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef, Span};
use jqf_syntax::{StringDecodeError, SyntaxViewError, decode_literal_into, parse_query};

extern crate alloc;

fn source() -> SourceRef {
    SourceRef::new(SourceId::new(41), SourceKind::Query)
}

fn bound_query(text: &str) -> jqf_syntax::BoundSyntax<'_, '_, jqf_syntax::Expr> {
    let parsed = parse_query(source(), text).unwrap().into_valid_syntax().unwrap();
    let parsed = Box::leak(Box::new(parsed));
    parsed
        .bind(ResolvedSource::new(source(), "view.jq", text.as_bytes(), 0))
        .unwrap()
}

#[test]
fn literal_decoding_covers_escapes_and_surrogate_pairs() {
    let text = r#"\"\\\/\b\f\n\r\t\u03bb\uD83D\uDE00"#;
    let quoted = format!("\"{text}\"");
    let bound = bound_query(&quoted);
    let span = Span::from_usize(1, 1 + text.len());
    let mut output = String::from("prefix:");
    decode_literal_into(bound.source(), source(), span, &mut output).unwrap();
    assert_eq!(output, "prefix:\"\\/\u{0008}\u{000c}\n\r\tλ😀");
}

#[test]
fn escaped_interpolation_is_decoded_without_flattening_interpolation_syntax() {
    let text = r#""literal=\\(not interpolation) actual=\(.)""#;
    let bound = bound_query(text);
    let jqf_syntax::ExprKind::String(template) = bound.root().kind() else {
        panic!("string template");
    };
    assert_eq!(template.segments().len(), 2);
    let jqf_syntax::TemplateSegment::Literal { span } = template.segments()[0] else {
        panic!("literal segment");
    };
    let mut output = String::new();
    decode_literal_into(bound.source(), source(), span, &mut output).unwrap();
    assert_eq!(output, "literal=\\(not interpolation) actual=");
}

#[test]
fn decode_is_atomic_and_rejects_mismatched_sources() {
    let text = r#""bad=\uD800""#;
    let parsed = parse_query(source(), text).unwrap();
    let syntax = parsed.syntax().unwrap();
    let bound = syntax
        .bind(ResolvedSource::new(source(), "invalid.jq", text.as_bytes(), 0))
        .unwrap();
    let mut output = String::from("unchanged");
    let error = decode_literal_into(bound.source(), source(), Span::new(1, 11), &mut output).unwrap_err();
    assert!(matches!(error, StringDecodeError::InvalidEscape(_)));
    assert_eq!(output, "unchanged");

    let other = SourceRef::new(SourceId::new(99), SourceKind::Query);
    assert_eq!(
        decode_literal_into(bound.source(), other, Span::new(1, 4), &mut output).unwrap_err(),
        StringDecodeError::View(SyntaxViewError::SourceMismatch)
    );
    assert_eq!(output, "unchanged");
}

#[test]
fn decode_errors_cover_invalid_spans_and_allocation_failure() {
    // A span past the checked source is an InvalidSpan view error, not a decode attempt.
    let bound = bound_query("\"short\"");
    let error = decode_literal_into(bound.source(), source(), Span::new(1, 999), &mut String::new()).unwrap_err();
    assert_eq!(error, StringDecodeError::View(SyntaxViewError::InvalidSpan));

    // A span ending inside a multi-byte character is not UTF-8 aligned.
    let bound = bound_query("\"λ\"");
    let error = decode_literal_into(bound.source(), source(), Span::new(1, 2), &mut String::new()).unwrap_err();
    assert_eq!(error, StringDecodeError::View(SyntaxViewError::InvalidSpan));

    // The allocation-failure variant is only reachable when output capacity reservation fails; pin its rendering
    // directly.
    assert_eq!(
        StringDecodeError::AllocationFailure.to_string(),
        "string decode allocation failed"
    );
}

#[test]
fn literal_decode_rejects_interpolation_and_all_invalid_surrogate_shapes() {
    for text in [
        r#""\uD800""#,
        r#""\uD800x""#,
        r#""\uD800\u0041""#,
        r#""\uDC00\uD800""#,
        r#""\uD800\uD800""#,
        r#""\u123""#,
        r#""\u12G4""#,
        r#""\u""#,
    ] {
        let parsed = parse_query(source(), text).unwrap();
        assert!(!parsed.diagnostics().is_empty(), "{text:?}");
        let syntax = parsed.syntax().unwrap();
        let bound = syntax
            .bind(ResolvedSource::new(source(), "invalid-escape.jq", text.as_bytes(), 0))
            .unwrap();

        // Every spelling above is refused twice over: the parser diagnoses it, and the decoder refuses it again from
        // the same source bytes rather than trusting that the parser ran.
        let mut output = String::new();
        assert!(
            matches!(
                decode_literal_into(
                    bound.source(),
                    source(),
                    Span::from_usize(1, text.len() - 1),
                    &mut output
                ),
                Err(StringDecodeError::InvalidEscape(_))
            ),
            "{text:?}"
        );
    }

    let text = r#""before \(.x) after""#;
    let parsed = parse_query(source(), text).unwrap();
    let syntax = parsed.syntax().unwrap();
    let bound = syntax
        .bind(ResolvedSource::new(source(), "interpolation.jq", text.as_bytes(), 0))
        .unwrap();
    let span = Span::from_usize(1, text.len() - 1);
    let mut output = String::new();
    assert!(matches!(
        decode_literal_into(bound.source(), source(), span, &mut output),
        Err(StringDecodeError::Interpolation(_))
    ));
}

/// A lone LOW surrogate is the one surrogate spelling with a meaning: it parses clean and decodes to U+FFFD. A lone
/// HIGH surrogate has none, and is refused on its own — it does not need a following invalid low to be wrong.
#[test]
fn literal_decode_accepts_a_lone_low_surrogate_and_rejects_a_lone_high_one() {
    // `"\uDC00"` decodes to the replacement character.
    let text = r#""\uDC00""#;
    let parsed = parse_query(source(), text).unwrap();
    assert!(parsed.diagnostics().is_empty(), "{text:?}");
    let syntax = parsed.syntax().unwrap();
    let bound = syntax
        .bind(ResolvedSource::new(source(), "low.jq", text.as_bytes(), 0))
        .unwrap();
    let mut output = String::new();
    decode_literal_into(
        bound.source(),
        source(),
        Span::from_usize(1, text.len() - 1),
        &mut output,
    )
    .expect("a lone low surrogate decodes");
    assert_eq!(output, "\u{FFFD}");

    // A lone HIGH surrogate still raises.
    for text in [r#""\uD800""#, r#""\uD800x""#] {
        let parsed = parse_query(source(), text).unwrap();
        assert!(!parsed.diagnostics().is_empty(), "{text:?}");
    }
}
