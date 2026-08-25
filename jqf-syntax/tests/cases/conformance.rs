use jqf_syntax::{ExprKind, SyntaxErrorKind, TemplateSegment};

use super::common::{parse_library, parse_program, parse_query, source};

#[test]
fn parser_accepts_the_listed_query_forms() {
    let valid = [
        ".",
        "..",
        "empty",
        "null",
        "true",
        "false",
        ".5",
        "1.",
        "1.e2",
        "-1.25e-2",
        r#""plain""#,
        r#""before \(.) after""#,
        ".foo",
        ".foo?",
        r#"."not-an-ident""#,
        r#".["key"]"#,
        ".[0]",
        ".[1, 2]",
        ".[$index]",
        ".[$start:$end]",
        ".[1:2]",
        ".[:2]?",
        ".[1:]",
        ".[]",
        "(.)[0].name?",
        "..[0]",
        "...name",
        "[]",
        "[1, 2 | .]",
        "{}",
        "{name: .name, if: .condition, $dynamic, ( .key ): .value,}",
        r#"{"\(.key)": .value}"#,
        ".a + .b * .c",
        ".a == .b and .c != .d or .e",
        ".a = (.b = .c)",
        ".a |= . + 1",
        ".a += 1",
        ".a -= 1",
        ".a *= 2",
        ".a /= 2",
        ".a %= 2",
        ".a //= 2",
        ".a // .b",
        ".items as $item | $item",
        ". as [$a, {$b, key: $c}] | [$a, $b, $c]",
        r#". as {("key"): $value} | $value"#,
        ". as [$a, $b] ?// {$a, b: $b} | [$a, $b]",
        "if .a then .b elif .c then .d else .e end",
        "if true then {} else {} end.foo",
        "try .value",
        "try (1 + 2) catch .",
        "try (label $l0 | .)",
        "try . as $x | .",
        "reduce .[] as $item (0; . + $item)",
        "reduce .[] as $item ({}; .).foo",
        "foreach .[] as $item (0; . + $item)",
        "foreach .[] as $item (0; . + $item; [$item, .])",
        "foreach .[] as $item ({}; .).@tag",
        "label $done | break $done",
        "map(.name; .id)",
        "map(., .)",
        "@json",
        r#"@json "value=\(.)""#,
        "def id: .; id",
        "1 | def inc: . + 1; inc",
        "(def inc: . + 1; inc)",
        "[def inc: . + 1; inc]",
        "if true then def inc: . + 1; inc else . end",
        r#""\(def id: .; id)""#,
        "let $name = .name | $name",
        ".price.@tag",
        "1.@tag",
        "1.&unit",
        ".5.@tag",
        r#".name.@["comment"]?"#,
        ".node.@(.selector)",
        ".a.&href",
        r#".a.&["aria-label"]?"#,
        ".a.&(.attribute)",
        ".name.@comment.leading = \"display name\"",
        ".a.&href |= . + \"/docs\"",
        "...@tag",
        "...&href",
    ];

    for query in valid {
        let parsed = parse_query(source(), query);
        assert!(parsed.diagnostics().is_empty(), "{query:?}: {:?}", parsed.diagnostics());
        assert!(parsed.syntax().is_some(), "{query:?}");
    }
}

#[test]
fn parser_rejects_incompatible_or_incomplete_forms() {
    let invalid = [
        "",
        "call()",
        "call(.;)",
        ". => .",
        ".a < .b < .c",
        ".a = .b = .c",
        "def f(): .; f",
        "def f($x;): .; f",
        "[1,]",
        ". as [] | .",
        ". as {} | .",
        ". as [$a,] | .",
        ". as {$a,} | .",
        ". as [$a ?// $b] | .",
        "{(.key)}",
        ".items (0; . + 1)",
        "foreach .items (0; . + 1)",
        "reduce .[] as $item (0; .; .)",
        ".item.&[.name]",
        r#".item.&["\(.name)"]"#,
        ". @tag",
        ". &href",
        ".a..",
        "1..2",
        "try label $l0 | .",
        "5 + label $l0 | .",
        "{a: label $l0 | .}",
        "$",
        "1e+",
        r#""\q""#,
        r#""\uD800""#,
        r#""\(+)""#,
        ".[1:2:3]",
        ".a === .b",
        ".a !== .b",
        ". === .",
        ". !== .",
    ];

    for query in invalid {
        let parsed = parse_query(source(), query);
        assert!(
            !parsed.diagnostics().is_empty(),
            "{query:?} unexpectedly parsed as {:?}",
            parsed.syntax()
        );
    }
}

// Two laws, one checked boundary: a recovery tree never converts to valid syntax, and a library unit may legally omit
// its final query.
#[test]
fn checked_parse_conversion_rejects_recovery() {
    let recovered = parse_query(source(), ". |");
    assert!(recovered.syntax().is_some());
    assert!(recovered.into_valid_syntax().is_err());
}

#[test]
fn libraries_may_omit_a_final_query_expression() {
    let library = parse_library(source(), "include \"strings\"; def id: .;");
    let unit = library.into_valid_syntax().unwrap();
    assert_eq!(unit.items.len(), 2);
    assert!(unit.expression.is_none());
}

#[test]
fn interpolation_contains_a_parsed_expression_tree_with_absolute_spans() {
    let query = r#""left=\(.a + 1) right""#;
    let parsed = parse_query(source(), query);
    assert!(parsed.diagnostics().is_empty());
    let ExprKind::String(template) = parsed.syntax().expect("string").kind() else {
        panic!("expected string template");
    };
    let TemplateSegment::Expression {
        span,
        expression,
        introducer_span,
        close_span,
    } = &template.segments()[1]
    else {
        panic!("expected interpolation");
    };
    assert_eq!(&query[span.range()], ".a + 1");
    assert_eq!(&query[introducer_span.range()], r"\(");
    assert_eq!(&query[close_span.range()], ")");
    assert!(matches!(expression.kind(), ExprKind::Binary(_)));
}

#[test]
fn template_stream_preserves_empty_adjacent_and_escaped_forms() {
    let empty = parse_query(source(), "\"\"").into_valid_syntax().unwrap();
    let ExprKind::String(empty) = empty.kind() else {
        panic!("expected empty string template");
    };
    assert_eq!(empty.segments().len(), 1);
    assert!(empty.segments()[0].span().is_empty());

    let adjacent_source = r#""\(.a)\(.b)""#;
    let adjacent = parse_query(source(), adjacent_source).into_valid_syntax().unwrap();
    let ExprKind::String(adjacent) = adjacent.kind() else {
        panic!("expected adjacent interpolation template");
    };
    assert_eq!(adjacent.segments().len(), 2);
    for (segment, expected_expression) in adjacent.segments().iter().zip([".a", ".b"]) {
        let TemplateSegment::Expression {
            span,
            introducer_span,
            close_span,
            ..
        } = segment
        else {
            panic!("expected adjacent interpolation expression");
        };
        assert_eq!(&adjacent_source[span.range()], expected_expression);
        assert_eq!(&adjacent_source[introducer_span.range()], r"\(");
        assert_eq!(&adjacent_source[close_span.range()], ")");
    }

    let escaped_source = r#""escaped=\\(.a)""#;
    let escaped = parse_query(source(), escaped_source).into_valid_syntax().unwrap();
    let ExprKind::String(escaped) = escaped.kind() else {
        panic!("expected escaped interpolation template");
    };
    assert_eq!(escaped.segments().len(), 1);
    assert!(matches!(escaped.segments()[0], TemplateSegment::Literal { .. }));
    assert_eq!(&escaped_source[escaped.segments()[0].span().range()], r"escaped=\\(.a)");
}

#[test]
fn source_units_carry_every_top_level_item_form() {
    let program = r#"module {name: "demo"};
import "math" as math {search: "."};
import "data.json" as $data;
include "strings";
def twice(f): f | f;
math::sqrt(.x)"#;
    let parsed = parse_program(source(), program);
    assert!(parsed.diagnostics().is_empty(), "{:?}", parsed.diagnostics());
    let unit = parsed.syntax().expect("program");
    assert_eq!(unit.items.len(), 5);
    assert!(unit.expression.is_some());
}

#[test]
fn string_diagnostic_codes_are_stable() {
    assert_eq!(
        SyntaxErrorKind::InvalidStringEscape.code().to_string(),
        "syntax.invalid-string-escape"
    );
    assert_eq!(
        SyntaxErrorKind::InvalidUnicodeEscape.code().to_string(),
        "syntax.invalid-unicode-escape"
    );
    assert_eq!(
        SyntaxErrorKind::ExpectedCallArgument.code().to_string(),
        "syntax.expected-call-argument"
    );
    assert_eq!(
        SyntaxErrorKind::ChainedComparison.code().to_string(),
        "syntax.chained-comparison"
    );
}

#[test]
fn string_escape_diagnostics_preserve_utf8_boundaries() {
    for query in [r#""\🦀""#, r#""\ué🦀""#] {
        let parsed = parse_query(source(), query);
        assert!(!parsed.diagnostics().is_empty());
        for label in parsed.diagnostics().iter().flat_map(jqf_source::Diagnostic::labels) {
            let start = usize::try_from(label.span().start()).unwrap();
            let end = usize::try_from(label.span().end()).unwrap();
            assert!(query.is_char_boundary(start), "{query:?}: {}", label.span());
            assert!(query.is_char_boundary(end), "{query:?}: {}", label.span());
        }
    }
}
