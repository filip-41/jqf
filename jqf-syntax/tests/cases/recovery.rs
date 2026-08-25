use std::collections::BTreeSet;

use jqf_source::{SourceId, SourceKind, SourceRef, Span};
use jqf_syntax::{
    AccessorSelector, BindingForm, DefItem, Expr, ExprKind, FieldSelector, ObjectKey, Pattern, PatternKind,
    PostfixSegment, SourceItem, SourceUnit, StringTemplate, TemplateSegment, parse_program as try_parse_program,
    parse_query as try_parse_query,
};

fn source() -> SourceRef {
    SourceRef::new(SourceId::new(31), SourceKind::Query)
}

fn parse_query(text: &str) -> jqf_syntax::Parse<Expr> {
    try_parse_query(source(), text).unwrap()
}

#[test]
fn recovery_preserves_later_call_arguments_and_object_members() {
    let call_source = "f(. + ; .ok)";
    let call = parse_query(call_source);
    assert_eq!(call.diagnostics().len(), 1, "{:?}", call.diagnostics());
    let ExprKind::Call(call) = call.syntax().unwrap().kind() else {
        panic!("expected recovered call");
    };
    assert_eq!(call.args.len(), 2);
    assert_eq!(&call_source[call.args[1].expression.span().range()], ".ok");
    assert_eq!(&call_source[call.parentheses.unwrap().range()], "(. + ; .ok)");

    let object_source = "{bad: . + , node: .a.@tag, attr: .a.&href}";
    let object = parse_query(object_source);
    assert_eq!(object.diagnostics().len(), 1, "{:?}", object.diagnostics());
    let ExprKind::Object {
        members, close_span, ..
    } = object.syntax().unwrap().kind()
    else {
        panic!("expected recovered object");
    };
    assert_eq!(members.len(), 3);
    assert_eq!(&object_source[key_span(&members[1].key).range()], "node");
    assert_eq!(&object_source[key_span(&members[2].key).range()], "attr");
    assert_eq!(&object_source[close_span.range()], "}");
    assert_accessor(members[1].value.as_ref(), |segment| {
        matches!(segment, PostfixSegment::NodeAccessor { .. })
    });
    assert_accessor(members[2].value.as_ref(), |segment| {
        matches!(segment, PostfixSegment::Attribute { .. })
    });
}

#[test]
fn nested_delimiter_recovery_respects_outer_call_and_object_boundaries() {
    let call_source = "f((. + ; .ok))";
    let call = parse_query(call_source);
    let diagnostics = format!("{:?}", call.diagnostics());
    let mut expression: &Expr = call.syntax().unwrap();
    while let ExprKind::Group { expression: inner, .. } = expression.kind() {
        expression = inner.as_ref();
    }
    let ExprKind::Call(call) = expression.kind() else {
        panic!("expected recovered call");
    };
    assert_eq!(call.args.len(), 2, "{diagnostics}");
    assert_eq!(&call_source[call.args[1].expression.span().range()], ".ok");

    for object_source in [r#"{bad: .&["x", good: .a.@tag}"#, r#"{bad: .@["x", good: .a.&href}"#] {
        let object = parse_query(object_source);
        let ExprKind::Object {
            members, close_span, ..
        } = object.syntax().unwrap().kind()
        else {
            panic!("expected recovered object");
        };
        assert_eq!(members.len(), 2, "{object_source:?}: {:?}", object.diagnostics());
        assert_eq!(&object_source[key_span(&members[1].key).range()], "good");
        assert_eq!(&object_source[close_span.range()], "}");
    }

    let pattern_source = ". as [{(. + }, $later] | .";
    let pattern = parse_query(pattern_source);
    let ExprKind::Binding(binding) = pattern.syntax().unwrap().kind() else {
        panic!("expected recovered binding");
    };
    let PatternKind::Array(items) = binding.pattern.kind() else {
        panic!("expected recovered outer array pattern");
    };
    assert_eq!(items.len(), 2, "{:?}", pattern.diagnostics());
    let PatternKind::Object(members) = items[0].kind() else {
        panic!("expected recovered inner object pattern");
    };
    assert_eq!(members.len(), 1, "{:?}", pattern.diagnostics());
    assert_eq!(&pattern_source[items[1].span().range()], "$later");

    let control_source = "if . then (. + else .ok end";
    let control = parse_query(control_source);
    let ExprKind::If(conditional) = control.syntax().unwrap().kind() else {
        panic!("expected recovered conditional");
    };
    assert_eq!(
        &control_source[conditional.else_keyword_span.unwrap().range()],
        "else",
        "{:?}",
        control.diagnostics()
    );
    assert_eq!(
        &control_source[conditional.else_branch.as_ref().unwrap().span().range()],
        ".ok"
    );
    assert_eq!(&control_source[conditional.end_keyword_span.range()], "end");
}

#[test]
fn recovery_preserves_later_control_branches_patterns_and_definitions() {
    let conditional_source = "if .a then . + else .b end";
    let conditional = parse_query(conditional_source);
    assert_eq!(conditional.diagnostics().len(), 1, "{:?}", conditional.diagnostics());
    let ExprKind::If(conditional) = conditional.syntax().unwrap().kind() else {
        panic!("expected recovered conditional");
    };
    assert_eq!(
        &conditional_source[conditional.else_branch.as_ref().unwrap().span().range()],
        ".b"
    );
    assert_eq!(&conditional_source[conditional.end_keyword_span.range()], "end");

    let pattern_source = ". as [$a, , $c] | .";
    let pattern = parse_query(pattern_source);
    assert_eq!(pattern.diagnostics().len(), 1, "{:?}", pattern.diagnostics());
    let ExprKind::Binding(binding) = pattern.syntax().unwrap().kind() else {
        panic!("expected recovered binding");
    };
    let PatternKind::Array(items) = binding.pattern.kind() else {
        panic!("expected recovered array pattern");
    };
    assert_eq!(items.len(), 3);
    assert!(matches!(items[1].kind(), PatternKind::Error));
    assert_eq!(&pattern_source[items[2].span().range()], "$c");

    let program_source = "def bad: . + ; def good: .; good";
    let program = try_parse_program(source(), program_source).unwrap();
    assert_eq!(program.diagnostics().len(), 1, "{:?}", program.diagnostics());
    let unit = program.syntax().unwrap();
    assert_eq!(unit.items.len(), 2);
    let SourceItem::Def(good) = &unit.items[1] else {
        panic!("expected later definition");
    };
    assert_eq!(&program_source[good.name.range()], "good");
    assert_eq!(
        &program_source[unit.expression.as_ref().unwrap().span().range()],
        "good"
    );

    let params_source = "def f($a junk; $b): .; def good: .; good";
    let params = try_parse_program(source(), params_source).unwrap();
    assert_eq!(params.diagnostics().len(), 1, "{:?}", params.diagnostics());
    let unit = params.syntax().unwrap();
    assert_eq!(unit.items.len(), 2, "{:?}", params.diagnostics());
    let SourceItem::Def(first) = &unit.items[0] else {
        panic!("first definition");
    };
    assert_eq!(first.params.len(), 2);
    assert_eq!(&params_source[first.params[1].name.range()], "$b");
    assert_eq!(
        &params_source[first.parameter_parentheses.unwrap().range()],
        "($a junk; $b)"
    );
    let SourceItem::Def(good) = &unit.items[1] else {
        panic!("later definition");
    };
    assert_eq!(&params_source[good.name.range()], "good");
    assert_eq!(&params_source[unit.expression.as_ref().unwrap().span().range()], "good");

    let missing_close_source = "def f($a: .; def good: .; good";
    let missing_close = try_parse_program(source(), missing_close_source).unwrap();
    assert_eq!(
        missing_close.diagnostics().len(),
        1,
        "{:?}",
        missing_close.diagnostics()
    );
    let unit = missing_close.syntax().unwrap();
    assert_eq!(unit.items.len(), 2, "{:?}", missing_close.diagnostics());
    let SourceItem::Def(first) = &unit.items[0] else {
        panic!("first definition");
    };
    assert_eq!(first.params.len(), 1);
    assert_eq!(&missing_close_source[first.body.span().range()], ".");
    let SourceItem::Def(good) = &unit.items[1] else {
        panic!("later definition");
    };
    assert_eq!(&missing_close_source[good.name.range()], "good");
    assert_eq!(
        &missing_close_source[unit.expression.as_ref().unwrap().span().range()],
        "good"
    );
}

#[test]
fn recovery_cascades_diagnostic_counts_and_preserves_outer_delimiters() {
    for (query, expected_diagnostics) in [
        ("[. + ]", 1),
        ("f(. + ; . * ; .)", 2),
        ("{a: . + , b: . * , c: .}", 2),
        ("{a: . + , b: .} | break", 2),
    ] {
        let parsed = parse_query(query);
        assert_eq!(
            parsed.diagnostics().len(),
            expected_diagnostics,
            "{query:?}: {:?}",
            parsed.diagnostics()
        );
    }

    let bracket_source = "[. + ]";
    let bracket = parse_query(bracket_source);
    let ExprKind::Array { close_span, .. } = bracket.syntax().unwrap().kind() else {
        panic!("expected recovered array");
    };
    assert_eq!(&bracket_source[close_span.range()], "]");
}

#[test]
fn caller_owned_synchronization_skips_junk_without_consuming_boundaries() {
    let call_source = "f(. : junk ; .ok)";
    let call = parse_query(call_source);
    assert_eq!(call.diagnostics().len(), 1, "{:?}", call.diagnostics());
    let ExprKind::Call(call) = call.syntax().unwrap().kind() else {
        panic!("call");
    };
    assert_eq!(call.args.len(), 2);
    assert_eq!(&call_source[call.args[1].expression.span().range()], ".ok");

    let object_source = "{a: . : junk, b: .}";
    let object = parse_query(object_source);
    assert_eq!(object.diagnostics().len(), 1, "{:?}", object.diagnostics());
    let ExprKind::Object { members, .. } = object.syntax().unwrap().kind() else {
        panic!("object");
    };
    assert_eq!(members.len(), 2);
    assert_eq!(&object_source[key_span(&members[1].key).range()], "b");

    let pattern_source = ". as [$a, 1 junk, $c] | .";
    let pattern = parse_query(pattern_source);
    assert_eq!(pattern.diagnostics().len(), 1, "{:?}", pattern.diagnostics());
    let ExprKind::Binding(binding) = pattern.syntax().unwrap().kind() else {
        panic!("binding");
    };
    let PatternKind::Array(items) = binding.pattern.kind() else {
        panic!("pattern");
    };
    assert_eq!(items.len(), 3);
    assert_eq!(&pattern_source[items[2].span().range()], "$c");

    let program_source = "def bad: : junk; def good: .; good";
    let program = try_parse_program(source(), program_source).unwrap();
    assert_eq!(program.diagnostics().len(), 1, "{:?}", program.diagnostics());
    assert_eq!(program.syntax().unwrap().items.len(), 2);
}

#[test]
fn delimiter_and_control_callers_retain_their_own_sync_tokens() {
    for query in ["(1 junk) | .ok", "[1 junk] | .ok"] {
        let parsed = parse_query(query);
        assert_eq!(parsed.diagnostics().len(), 1, "{query:?}: {:?}", parsed.diagnostics());
        let ExprKind::Binary(pipe) = parsed.syntax().unwrap().kind() else {
            panic!("outer pipe");
        };
        assert_eq!(&query[pipe.right.span().range()], ".ok");
    }

    let conditional_source = "if . then : junk else .ok end";
    let conditional = parse_query(conditional_source);
    assert_eq!(conditional.diagnostics().len(), 1, "{:?}", conditional.diagnostics());
    let ExprKind::If(conditional) = conditional.syntax().unwrap().kind() else {
        panic!("conditional");
    };
    assert_eq!(
        &conditional_source[conditional.else_keyword_span.unwrap().range()],
        "else"
    );
    assert_eq!(
        &conditional_source[conditional.else_branch.as_ref().unwrap().span().range()],
        ".ok"
    );
    assert_eq!(&conditional_source[conditional.end_keyword_span.range()], "end");
}

#[test]
fn fold_slots_and_try_recovery_preserve_later_control_content() {
    let reduce_source = "reduce .[] as $x (: junk; . + $x) | .ok";
    let reduce = parse_query(reduce_source);
    assert_eq!(reduce.diagnostics().len(), 1, "{:?}", reduce.diagnostics());
    let ExprKind::Binary(pipe) = reduce.syntax().unwrap().kind() else {
        panic!("outer pipe");
    };
    assert_eq!(&reduce_source[pipe.right.span().range()], ".ok");

    let foreach_source = "foreach .[] as $x (0; : junk; .ok) | .after";
    let foreach = parse_query(foreach_source);
    assert_eq!(foreach.diagnostics().len(), 1, "{:?}", foreach.diagnostics());
    let ExprKind::Binary(pipe) = foreach.syntax().unwrap().kind() else {
        panic!("outer pipe");
    };
    let ExprKind::Foreach(fold) = pipe.left.kind() else {
        panic!("foreach");
    };
    assert_eq!(&foreach_source[fold.extract.as_ref().unwrap().span().range()], ".ok");
    assert_eq!(&foreach_source[pipe.right.span().range()], ".after");

    let try_source = "if . then try : junk catch .ok else .fallback end";
    let try_expression = parse_query(try_source);
    assert_eq!(
        try_expression.diagnostics().len(),
        1,
        "{:?}",
        try_expression.diagnostics()
    );
    let ExprKind::If(conditional) = try_expression.syntax().unwrap().kind() else {
        panic!("conditional");
    };
    let ExprKind::Try(try_expression) = conditional.branches[0].then_branch.kind() else {
        panic!("try");
    };
    assert_eq!(
        &try_source[try_expression.handler.as_ref().unwrap().span().range()],
        ".ok"
    );
    assert_eq!(
        &try_source[conditional.else_branch.as_ref().unwrap().span().range()],
        ".fallback"
    );
}

/// A caller-owned terminator survives every recovery that stops in front of it.
///
/// Each fixture fails inside a call argument, so the `;` that ends the argument belongs to the call: a recovery that
/// consumed it would swallow the rest of the argument list with it.
#[test]
fn recovery_leaves_the_callers_terminator_for_the_caller() {
    for source in ["f(~; .ok)", "f(. as ~; .ok)", "f(. as ; .ok)"] {
        let parsed = parse_query(source);
        assert!(!parsed.diagnostics().is_empty(), "{source:?}");
        let ExprKind::Call(call) = parsed.syntax().unwrap().kind() else {
            panic!("expected recovered call for {source:?}");
        };
        assert_eq!(call.args.len(), 2, "{source:?}");
        assert_eq!(&source[call.args[1].expression.span().range()], ".ok", "{source:?}");
        assert_eq!(&source[call.parentheses.unwrap().range()], &source[1..]);
    }
}

/// A bracket accessor that found no string records a zero-width insertion.
///
/// Recovery here can consume nothing at all — the comma belongs to the comma operator outside the accessor — so
/// recording the offending token as the selector would hand the accessor a token the source wrote for someone else.
#[test]
fn a_missing_accessor_selector_is_a_zero_width_insertion() {
    let source = ".@[,1]";
    let parsed = parse_query(source);
    let ExprKind::Binary(comma) = parsed.syntax().unwrap().kind() else {
        panic!("expected the comma operator to keep its token");
    };
    assert_eq!(&source[comma.op_span.range()], ",");
    let ExprKind::Postfix(postfix) = comma.left.kind() else {
        panic!("expected the accessor chain");
    };
    let PostfixSegment::NodeAccessor {
        selector: AccessorSelector::Bracket { selector, .. },
    } = &postfix.steps()[0].segment
    else {
        panic!("expected a bracket accessor selector");
    };
    assert!(
        selector.is_empty() && selector.start() == 3,
        "selector {selector} is not the insertion in front of the comma"
    );
}

/// A `reduce` has no extract slot, so a third slot leaves the tree holding neither the slot nor the separator that
/// would introduce it.
#[test]
fn a_rejected_reduce_extract_keeps_no_separator() {
    let source = "reduce .[] as $x (0; .; 1)";
    let parsed = parse_query(source);
    assert_eq!(parsed.diagnostics().len(), 1, "{:?}", parsed.diagnostics());
    let ExprKind::Reduce(fold) = parsed.syntax().unwrap().kind() else {
        panic!("expected a recovered reduce");
    };
    assert!(fold.extract.is_none());
    assert!(
        fold.extract_separator_span.is_none(),
        "a dropped extract leaves no separator behind"
    );

    let kept = "foreach .[] as $x (0; .; 1)";
    let ExprKind::Foreach(fold) = parse_query(kept)
        .into_valid_syntax()
        .unwrap()
        .into_root()
        .kind()
        .clone()
    else {
        panic!("expected a foreach");
    };
    assert_eq!(&kept[fold.extract_separator_span.unwrap().range()], ";");
    assert_eq!(&kept[fold.extract.unwrap().span().range()], "1");
}

#[test]
fn valid_roots_have_no_error_nodes_or_missing_required_punctuation() {
    let mut coverage = SyntaxCoverage::default();
    for query in [
        ".",
        "..",
        "empty",
        "null",
        "true",
        "1.",
        r#""literal=\(.)""#,
        "$value",
        "@json",
        r#"@json "value=\(.)""#,
        "-(.)",
        "[]",
        "[.]",
        r#"{name: ., "text": ., $variable, (.key): .}"#,
        ".a = .b + .c",
        "def id($x; f): .; id($x; f)",
        r#".field?."quoted".[0][][1:2].@tag.@["comment"].@(.name).&href.&["aria"].&(.attr)?"#,
        "(.)?",
        "if .a then .b elif .c then .d else .e end",
        "try .value catch .fallback",
        "reduce .[] as [$head, {$tail}] (0; . + $head)",
        "foreach .[] as $item (0; . + $item; .)",
        r#". as [$a, {name: $b, "text": $c, (.key): $d}] ?// {$e} | let $value = . | label $done | break $done"#,
        "~generator(0; . + 1; .) as ~x | [~x.next, ~x.rest]",
    ] {
        let syntax = parse_query(query)
            .into_valid_syntax()
            .unwrap_or_else(|diagnostics| panic!("{query:?}: {diagnostics:?}"));
        assert_valid_expression(&syntax, &mut coverage);
    }

    let program_source = r#"module {name: "demo"};
import "math" as math {search: "."};
import "data.json" as $data;
include "strings" {search: "."};
def twice($value; filter): filter | filter;
math::sqrt(.x)"#;
    let program = try_parse_program(source(), program_source)
        .unwrap()
        .into_valid_syntax()
        .unwrap();
    assert_valid_source_unit(&program, &mut coverage);
    assert_expected_coverage(&coverage);
}

/// The forms the fixtures above are expected to reach.
///
/// A hand-written expectation, not a completeness proof — see the note on [`assert_valid_expression`].
fn assert_expected_coverage(coverage: &SyntaxCoverage) {
    assert_eq!(
        coverage.expressions,
        BTreeSet::from([
            "array",
            "assignment",
            "binary",
            "binding",
            "bool",
            "break",
            "call",
            "definition",
            "empty",
            "engine-call",
            "engine-term",
            "foreach",
            "format",
            "format-template",
            "group",
            "identity",
            "if",
            "label",
            "null",
            "number",
            "object",
            "postfix",
            "recursive-descent",
            "reduce",
            "string",
            "try",
            "unary",
            "variable",
        ])
    );
    assert_eq!(
        coverage.patterns,
        BTreeSet::from(["alternative", "array", "engine-binding", "object", "variable",])
    );
    assert_eq!(
        coverage.object_keys,
        BTreeSet::from(["expression", "name", "string", "variable"])
    );
    assert_eq!(
        coverage.postfix_segments,
        BTreeSet::from([
            "attribute",
            "error-suppression",
            "field",
            "index",
            "node-accessor",
            "slice",
        ])
    );
    assert_eq!(
        coverage.accessor_selectors,
        BTreeSet::from(["bracket", "direct", "dynamic"])
    );
    assert_eq!(
        coverage.source_items,
        BTreeSet::from(["definition", "import", "include", "module"])
    );
}

#[test]
fn missing_punctuation_uses_current_start_without_stealing_neighbor_spans() {
    let program_source = "def f .; .";
    let program = try_parse_program(source(), program_source).unwrap();
    assert_eq!(program.diagnostics().len(), 1, "{:?}", program.diagnostics());
    let SourceItem::Def(definition) = &program.syntax().unwrap().items[0] else {
        panic!("definition");
    };
    assert_eq!(definition.colon_span, Span::new(6, 6));
    assert_eq!(&program_source[definition.body.span().range()], ".");
    assert_eq!(&program_source[definition.semicolon_span.range()], ";");

    let group = parse_query("(.");
    let ExprKind::Group { close_span, .. } = group.syntax().unwrap().kind() else {
        panic!("group");
    };
    assert_eq!(*close_span, Span::new(2, 2));
}

#[test]
fn recovery_keeps_the_template_whole_after_a_broken_interpolation() {
    let source = r#""before \(. + ) after""#;
    let parsed = parse_query(source);
    assert_eq!(parsed.diagnostics().len(), 1, "{:?}", parsed.diagnostics());
    let ExprKind::String(template) = parsed.syntax().unwrap().kind() else {
        panic!("string template");
    };
    // The broken interpolation body is recovered at its closing paren and the trailing literal survives as a template
    // segment.
    assert_eq!(template.segments().len(), 3);
    let TemplateSegment::Expression { span, .. } = template.segments()[1] else {
        panic!("interpolation segment");
    };
    assert_eq!(&source[span.range()], ". + ");
}

#[test]
fn recovery_continues_past_a_malformed_engine_surface_term() {
    let source = "f(~ 123; .ok)";
    let parsed = parse_query(source);
    assert_eq!(parsed.diagnostics().len(), 1, "{:?}", parsed.diagnostics());
    let ExprKind::Call(call) = parsed.syntax().unwrap().kind() else {
        panic!("expected recovered call");
    };
    assert_eq!(call.args.len(), 2);
    assert_eq!(&source[call.args[1].expression.span().range()], ".ok");
    assert_eq!(parsed.diagnostics()[0].code().to_string(), "syntax.expected-token");
    assert_eq!(
        parsed.diagnostics()[0].message(),
        "expected identifier in engine-surface term"
    );
}

fn assert_accessor(value: Option<&Expr>, predicate: impl Fn(&PostfixSegment) -> bool) {
    let ExprKind::Postfix(postfix) = value.unwrap().kind() else {
        panic!("postfix accessor");
    };
    assert!(postfix.steps().iter().any(|step| predicate(&step.segment)));
}

fn key_span(key: &ObjectKey) -> Span {
    match key {
        ObjectKey::Name(span) | ObjectKey::Variable(span) => *span,
        ObjectKey::String(template) => template.span(),
        ObjectKey::Expr(expression) => expression.span(),
        _ => unreachable!("test covers the closed object-key inventory"),
    }
}

fn assert_nonempty(span: Span) {
    assert!(!span.is_empty(), "required punctuation span is empty");
}

#[derive(Default)]
struct SyntaxCoverage {
    expressions: BTreeSet<&'static str>,
    patterns: BTreeSet<&'static str>,
    object_keys: BTreeSet<&'static str>,
    postfix_segments: BTreeSet<&'static str>,
    accessor_selectors: BTreeSet<&'static str>,
    source_items: BTreeSet<&'static str>,
}

#[allow(
    clippy::too_many_lines,
    reason = "one match over every expression form keeps the span invariant and the coverage tally together; splitting it would separate a form's checks from its arm"
)]
// The coverage sets below are hand-written expectations plus a compile-time inventory: the closed
// expression/pattern/postfix/accessor/source-item enums fail to compile here when a form has no arm.
// Tooling/source-form enums stay open and still need a trailing wildcard. Adding an accepted form means adding its arm,
// its fixture and its set entry.
fn assert_valid_expression(expression: &Expr, coverage: &mut SyntaxCoverage) {
    // The one authored form that spans nothing is the identity a root postfix chain implies: `.a`, `.@x` and `.&x`
    // spend their dot on the step's operator, so the base owns no byte and the dot is owned exactly once.
    if !matches!(expression.kind(), ExprKind::Identity) {
        assert_nonempty(expression.span());
    }
    match expression.kind() {
        ExprKind::Error => panic!("valid root contains an error node"),
        ExprKind::Identity => {
            coverage.expressions.insert("identity");
        }
        ExprKind::RecursiveDescent => {
            coverage.expressions.insert("recursive-descent");
        }
        ExprKind::Empty => {
            coverage.expressions.insert("empty");
        }
        ExprKind::Null => {
            coverage.expressions.insert("null");
        }
        ExprKind::Bool(_) => {
            coverage.expressions.insert("bool");
        }
        ExprKind::Number => {
            coverage.expressions.insert("number");
        }
        ExprKind::String(template) => {
            coverage.expressions.insert("string");
            assert_valid_template(template, coverage);
        }
        ExprKind::Variable => {
            coverage.expressions.insert("variable");
        }
        ExprKind::Format => {
            coverage.expressions.insert("format");
        }
        ExprKind::FormatTemplate { format, template } => {
            coverage.expressions.insert("format-template");
            assert_valid_expression(format, coverage);
            assert_valid_template(template, coverage);
        }
        ExprKind::Group {
            expression,
            open_span,
            close_span,
        } => {
            coverage.expressions.insert("group");
            assert_nonempty(*open_span);
            assert_nonempty(*close_span);
            assert_valid_expression(expression, coverage);
        }
        ExprKind::Array {
            expression: Some(expression),
            open_span,
            close_span,
        } => {
            coverage.expressions.insert("array");
            assert_nonempty(*open_span);
            assert_nonempty(*close_span);
            assert_valid_expression(expression, coverage);
        }
        ExprKind::Array {
            expression: None,
            open_span,
            close_span,
        } => {
            coverage.expressions.insert("array");
            assert_nonempty(*open_span);
            assert_nonempty(*close_span);
        }
        ExprKind::Object {
            members,
            open_span,
            close_span,
        } => {
            coverage.expressions.insert("object");
            assert_nonempty(*open_span);
            assert_nonempty(*close_span);
            for member in members {
                assert_nonempty(member.span);
                assert_valid_object_key(&member.key, coverage);
                if let Some(value) = &member.value {
                    assert_valid_expression(value, coverage);
                }
                if let Some(separator) = member.separator_span {
                    assert_nonempty(separator);
                }
            }
        }
        ExprKind::Unary(unary) => {
            coverage.expressions.insert("unary");
            assert_nonempty(unary.op_span);
            assert_valid_expression(&unary.expr, coverage);
        }
        ExprKind::Binary(binary) => {
            coverage.expressions.insert("binary");
            assert_nonempty(binary.op_span);
            assert_valid_expression(&binary.left, coverage);
            assert_valid_expression(&binary.right, coverage);
        }
        ExprKind::Assignment(assignment) => {
            coverage.expressions.insert("assignment");
            assert_nonempty(assignment.op_span);
            assert_valid_expression(&assignment.target, coverage);
            assert_valid_expression(&assignment.value, coverage);
        }
        ExprKind::Definition(definition) => {
            coverage.expressions.insert("definition");
            assert_valid_definition(&definition.definition, coverage);
            assert_valid_expression(&definition.body, coverage);
        }
        ExprKind::Call(call) => {
            coverage.expressions.insert("call");
            assert_nonempty(call.name);
            if let Some(parentheses) = call.parentheses {
                assert_nonempty(parentheses);
            }
            for argument in &call.args {
                assert_valid_expression(&argument.expression, coverage);
                if let Some(separator) = argument.separator_span {
                    assert_nonempty(separator);
                }
            }
        }
        ExprKind::EngineCall { tilde_span, call } => {
            coverage.expressions.insert("engine-call");
            assert_nonempty(*tilde_span);
            assert_nonempty(call.name);
            for argument in &call.args {
                assert_valid_expression(&argument.expression, coverage);
            }
        }
        ExprKind::EngineTerm { tilde_span, name } => {
            coverage.expressions.insert("engine-term");
            assert_nonempty(*tilde_span);
            assert_nonempty(*name);
        }
        ExprKind::Postfix(postfix) => {
            coverage.expressions.insert("postfix");
            assert_valid_expression(postfix.base(), coverage);
            for step in postfix.steps() {
                assert_nonempty(step.operator_span);
                assert_nonempty(step.span);
                if let Some(optional) = step.optional_suffix_span {
                    assert_nonempty(optional);
                }
                match &step.segment {
                    PostfixSegment::Field { selector } => {
                        coverage.postfix_segments.insert("field");
                        match selector {
                            FieldSelector::Name(span) => assert_nonempty(*span),
                            FieldSelector::String(template) => {
                                assert_valid_template(template, coverage);
                            }
                            _ => {}
                        }
                    }
                    PostfixSegment::Index {
                        index,
                        open_span,
                        close_span,
                    } => {
                        coverage.postfix_segments.insert("index");
                        assert_nonempty(*open_span);
                        assert_nonempty(*close_span);
                        if let Some(index) = index {
                            assert_valid_expression(index, coverage);
                        }
                    }
                    PostfixSegment::Slice {
                        start,
                        end,
                        colon_span,
                        open_span,
                        close_span,
                    } => {
                        coverage.postfix_segments.insert("slice");
                        assert_nonempty(*colon_span);
                        assert_nonempty(*open_span);
                        assert_nonempty(*close_span);
                        if let Some(start) = start {
                            assert_valid_expression(start, coverage);
                        }
                        if let Some(end) = end {
                            assert_valid_expression(end, coverage);
                        }
                    }
                    PostfixSegment::NodeAccessor { selector } => {
                        coverage.postfix_segments.insert("node-accessor");
                        assert_valid_accessor(selector, coverage);
                    }
                    PostfixSegment::Attribute { selector } => {
                        coverage.postfix_segments.insert("attribute");
                        assert_valid_accessor(selector, coverage);
                    }
                    PostfixSegment::ErrorSuppression => {
                        coverage.postfix_segments.insert("error-suppression");
                    }
                }
            }
        }
        ExprKind::If(conditional) => {
            coverage.expressions.insert("if");
            for branch in &conditional.branches {
                assert_nonempty(branch.keyword_span);
                assert_nonempty(branch.then_keyword_span);
                assert_valid_expression(&branch.condition, coverage);
                assert_valid_expression(&branch.then_branch, coverage);
            }
            if let Some(alternative) = &conditional.else_branch {
                assert_nonempty(conditional.else_keyword_span.unwrap());
                assert_valid_expression(alternative, coverage);
            }
            assert_nonempty(conditional.end_keyword_span);
        }
        ExprKind::Try(try_expression) => {
            coverage.expressions.insert("try");
            assert_nonempty(try_expression.try_keyword_span);
            assert_valid_expression(&try_expression.expr, coverage);
            if let Some(handler) = &try_expression.handler {
                assert_nonempty(try_expression.catch_keyword_span.unwrap());
                assert_valid_expression(handler, coverage);
            }
        }
        ExprKind::Reduce(fold) => {
            coverage.expressions.insert("reduce");
            assert_valid_loop(fold, coverage);
        }
        ExprKind::Foreach(fold) => {
            coverage.expressions.insert("foreach");
            assert_valid_loop(fold, coverage);
        }
        ExprKind::Binding(binding) => {
            coverage.expressions.insert("binding");
            match binding.form {
                BindingForm::As {
                    as_keyword_span,
                    pipe_span,
                } => {
                    assert_nonempty(as_keyword_span);
                    assert_nonempty(pipe_span);
                }
                BindingForm::Let {
                    let_keyword_span,
                    equals_span,
                    pipe_span,
                } => {
                    assert_nonempty(let_keyword_span);
                    assert_nonempty(equals_span);
                    assert_nonempty(pipe_span);
                }
                _ => {}
            }
            assert_valid_pattern(&binding.pattern, coverage);
            assert_valid_expression(&binding.value, coverage);
            assert_valid_expression(&binding.body, coverage);
        }
        ExprKind::Label {
            label_keyword_span,
            label,
            pipe_span,
            body,
        } => {
            coverage.expressions.insert("label");
            assert_nonempty(*label_keyword_span);
            assert_nonempty(*label);
            assert_nonempty(*pipe_span);
            assert_valid_expression(body, coverage);
        }
        ExprKind::Break {
            break_keyword_span,
            label,
        } => {
            coverage.expressions.insert("break");
            assert_nonempty(*break_keyword_span);
            assert_nonempty(*label);
        }
    }
}

fn assert_valid_loop(fold: &jqf_syntax::LoopExpr, coverage: &mut SyntaxCoverage) {
    assert_nonempty(fold.keyword_span);
    assert_nonempty(fold.as_keyword_span);
    assert_nonempty(fold.open_span);
    assert_nonempty(fold.update_separator_span);
    assert_nonempty(fold.close_span);
    assert_valid_expression(&fold.source, coverage);
    assert_valid_pattern(&fold.binding, coverage);
    assert_valid_expression(&fold.init, coverage);
    assert_valid_expression(&fold.update, coverage);
    if let Some(extract) = &fold.extract {
        assert_nonempty(fold.extract_separator_span.unwrap());
        assert_valid_expression(extract, coverage);
    }
}

fn assert_valid_pattern(pattern: &Pattern, coverage: &mut SyntaxCoverage) {
    assert_nonempty(pattern.span());
    match pattern.kind() {
        PatternKind::Error => panic!("valid root contains an error pattern"),
        PatternKind::Variable => {
            coverage.patterns.insert("variable");
        }
        PatternKind::EngineBinding => {
            coverage.patterns.insert("engine-binding");
        }
        PatternKind::Array(items) => {
            coverage.patterns.insert("array");
            for item in items {
                assert_valid_pattern(item, coverage);
            }
        }
        PatternKind::Object(members) => {
            coverage.patterns.insert("object");
            for member in members {
                assert_nonempty(member.span);
                assert_valid_object_key(&member.key, coverage);
                if let Some(pattern) = &member.pattern {
                    assert_valid_pattern(pattern, coverage);
                }
            }
        }
        PatternKind::Alternative(left, right) => {
            coverage.patterns.insert("alternative");
            assert_valid_pattern(left, coverage);
            assert_valid_pattern(right, coverage);
        }
    }
}

fn assert_valid_object_key(key: &ObjectKey, coverage: &mut SyntaxCoverage) {
    match key {
        ObjectKey::Name(span) => {
            coverage.object_keys.insert("name");
            assert_nonempty(*span);
        }
        ObjectKey::String(template) => {
            coverage.object_keys.insert("string");
            assert_valid_template(template, coverage);
        }
        ObjectKey::Variable(span) => {
            coverage.object_keys.insert("variable");
            assert_nonempty(*span);
        }
        ObjectKey::Expr(expression) => {
            coverage.object_keys.insert("expression");
            assert_valid_expression(expression, coverage);
        }
        _ => {}
    }
}

fn assert_valid_accessor(selector: &AccessorSelector, coverage: &mut SyntaxCoverage) {
    match selector {
        AccessorSelector::Direct { selector } => {
            coverage.accessor_selectors.insert("direct");
            assert_nonempty(*selector);
        }
        AccessorSelector::Bracket {
            selector,
            open_span,
            close_span,
        } => {
            coverage.accessor_selectors.insert("bracket");
            assert_nonempty(*selector);
            assert_nonempty(*open_span);
            assert_nonempty(*close_span);
        }
        AccessorSelector::Dynamic {
            selector,
            open_span,
            close_span,
        } => {
            coverage.accessor_selectors.insert("dynamic");
            assert_nonempty(*open_span);
            assert_nonempty(*close_span);
            assert_valid_expression(selector, coverage);
        }
    }
}

fn assert_valid_template(template: &StringTemplate, coverage: &mut SyntaxCoverage) {
    assert_nonempty(template.span());
    for segment in template.segments() {
        if let TemplateSegment::Expression {
            expression,
            introducer_span,
            close_span,
            ..
        } = segment
        {
            assert_nonempty(*introducer_span);
            assert_nonempty(*close_span);
            assert_valid_expression(expression, coverage);
        }
    }
}

fn assert_valid_definition(definition: &DefItem, coverage: &mut SyntaxCoverage) {
    assert_nonempty(definition.def_keyword_span);
    assert_nonempty(definition.name);
    assert_nonempty(definition.colon_span);
    assert_nonempty(definition.semicolon_span);
    assert_nonempty(definition.span);
    if let Some(parentheses) = definition.parameter_parentheses {
        assert_nonempty(parentheses);
    }
    for parameter in &definition.params {
        assert_nonempty(parameter.name);
        if let Some(separator) = parameter.separator_span {
            assert_nonempty(separator);
        }
    }
    assert_valid_expression(&definition.body, coverage);
}

fn assert_valid_source_unit(unit: &SourceUnit, coverage: &mut SyntaxCoverage) {
    assert_nonempty(unit.span);
    for item in &unit.items {
        match item {
            SourceItem::Module(item) => {
                coverage.source_items.insert("module");
                assert_nonempty(item.module_keyword_span);
                assert_nonempty(item.semicolon_span);
                assert_nonempty(item.span);
                assert_valid_expression(&item.metadata, coverage);
            }
            SourceItem::Import(item) => {
                coverage.source_items.insert("import");
                assert_nonempty(item.import_keyword_span);
                assert_valid_template(&item.path, coverage);
                assert_nonempty(item.as_keyword_span);
                assert_nonempty(item.alias);
                assert_nonempty(item.semicolon_span);
                assert_nonempty(item.span);
                if let Some(metadata) = &item.metadata {
                    assert_valid_expression(metadata, coverage);
                }
            }
            SourceItem::Include(item) => {
                coverage.source_items.insert("include");
                assert_nonempty(item.include_keyword_span);
                assert_valid_template(&item.path, coverage);
                assert_nonempty(item.semicolon_span);
                assert_nonempty(item.span);
                if let Some(metadata) = &item.metadata {
                    assert_valid_expression(metadata, coverage);
                }
            }
            SourceItem::Def(definition) => {
                coverage.source_items.insert("definition");
                assert_valid_definition(definition, coverage);
            }
        }
    }
    if let Some(expression) = &unit.expression {
        assert_valid_expression(expression, coverage);
    }
}

/// A control form's recovery sync must stop at caller-owned closers, not eat them: `(try , )` reports the `,` and
/// leaves the `)` to the GROUP. A control-keyword-only sync set consumed both and then reported the group unclosed at
/// EOF — a delimiter the parser itself ate.
#[test]
fn control_recovery_stops_at_caller_owned_closers() {
    for (source, problem_offset) in [("(try , )", 5), ("(if . then , )", 11)] {
        let parsed = parse_query(source);
        let diagnostics = parsed.diagnostics();
        assert!(!diagnostics.is_empty(), "{source}");
        // The FIRST reported problem is the broken expression itself.
        assert_eq!(
            diagnostics[0].labels().first().expect("primary label").span().start(),
            problem_offset,
            "{source}: {diagnostics:?}"
        );
        // Nothing is reported at end-of-input: a sync that ate the caller's `)` used to surface as an unclosed-group
        // error there.
        for diagnostic in diagnostics {
            let start = diagnostic.labels().first().expect("primary label").span().start();
            assert!(
                usize::try_from(start).unwrap_or(usize::MAX) < source.len(),
                "{source}: diagnostic at EOF means a closer was eaten: {diagnostic:?}"
            );
        }
        let ExprKind::Group {
            expression: _,
            close_span,
            ..
        } = parsed.syntax().as_ref().unwrap().kind()
        else {
            panic!("{source}: expected a recovered group");
        };
        assert_eq!(&source[close_span.range()], ")");
    }
}

/// An alternative-pattern chain releases its links with its subtree, exactly like an operator chain: the deep body
/// after the chain is judged from the OUTER depth. With the charge leaked, this program refused at a depth its tree
/// never reaches.
#[test]
fn an_alternative_pattern_chain_releases_its_nesting_charge_with_the_subtree() {
    // A ~10k-deep tree is the point of the test, so it runs on its own big-stack thread (the same instrument the
    // stack-depth gate uses); a default test thread cannot hold the deep tree's own recursion.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(deep_body_after_alternative_chain_parses_from_the_outer_depth)
        .expect("test thread");
    handle.join().expect("no panic");
}

fn deep_body_after_alternative_chain_parses_from_the_outer_depth() {
    const LINKS: usize = 50;
    // Deep enough that the leaked chain charge would cross the ceiling (9_960 + ~52 > 10_000), shallow enough that the
    // released program fits.
    const BODY_DEPTH: usize = 9_960;
    let mut source = String::from(". as $a");
    for _ in 0..LINKS {
        source.push_str(" ?// $b");
    }
    source.push_str(" | ");
    source.push_str(&"(".repeat(BODY_DEPTH));
    source.push('0');
    source.push_str(&")".repeat(BODY_DEPTH));
    let parsed = parse_query(&source);
    assert!(parsed.diagnostics().is_empty(), "{:?}", parsed.diagnostics());
}
