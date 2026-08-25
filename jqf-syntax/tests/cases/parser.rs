use jqf_source::{SourceId, SourceKind, SourceRef, Span};
use jqf_syntax::{
    AccessorSelector, BinaryOp, BindingForm, Expr, ExprKind, FieldSelector, ObjectKey, Parse, PatternKind, PostfixExpr,
    PostfixSegment, SourceItem, SourceUnit, SyntaxNodeRef, SyntaxWalk, WalkEvent, parse_library, parse_program,
    parse_query,
};

struct Parser<'source> {
    source: SourceRef,
    text: &'source str,
}

impl<'source> Parser<'source> {
    fn new(source: SourceRef, text: &'source str) -> Self {
        Self { source, text }
    }

    fn parse_query(self) -> Parse<jqf_syntax::Expr> {
        parse_query(self.source, self.text).expect("test source fits compact syntax spans")
    }

    fn parse_program(self) -> Parse<SourceUnit> {
        parse_program(self.source, self.text).expect("test source fits compact syntax spans")
    }

    fn parse_library(self) -> Parse<SourceUnit> {
        parse_library(self.source, self.text).expect("test source fits compact syntax spans")
    }
}

fn source() -> SourceRef {
    SourceRef::new(SourceId::new(11), SourceKind::Query)
}

fn postfix(expression: &Expr) -> &PostfixExpr {
    let ExprKind::Postfix(chain) = expression.kind() else {
        panic!("expected authored postfix chain");
    };
    chain
}

#[test]
fn parser_builds_expression_trees_with_operator_precedence() {
    let parsed = Parser::new(source(), ". + 1 * 2 | @json").parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("expression should parse");
    let ExprKind::Binary(pipe) = expression.kind() else {
        panic!("expected pipe expression");
    };
    assert_eq!(pipe.op, BinaryOp::Pipe);
    assert_eq!(pipe.op_span, Span::new(10, 11));

    let ExprKind::Binary(add) = pipe.left.kind() else {
        panic!("expected addition on pipe lhs");
    };
    assert_eq!(add.op, BinaryOp::Add);

    let ExprKind::Binary(mul) = add.right.kind() else {
        panic!("expected multiplication to bind tighter than addition");
    };
    assert_eq!(mul.op, BinaryOp::Multiply);
}

#[test]
fn parser_preserves_postfix_path_shape() {
    let parsed = Parser::new(source(), ".users[0].name?").parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("expression should parse");
    let chain = postfix(expression);
    assert_eq!(chain.steps().len(), 3);
    let users = &chain.steps()[0];
    assert_eq!(users.operator_span, Span::new(0, 1));
    assert_eq!(users.span, Span::new(0, 6));
    assert!(matches!(users.segment, PostfixSegment::Field { .. }));

    let index = &chain.steps()[1];
    assert_eq!(index.operator_span, Span::new(6, 7));
    assert_eq!(index.span, Span::new(6, 9));
    assert!(matches!(index.segment, PostfixSegment::Index { .. }));

    let name = &chain.steps()[2];
    assert_eq!(name.operator_span, Span::new(9, 10));
    assert_eq!(name.optional_suffix_span, Some(Span::new(14, 15)));
    assert_eq!(name.span, Span::new(9, 15));
    let PostfixSegment::Field {
        selector: FieldSelector::Name(key),
    } = &name.segment
    else {
        panic!("expected name field segment");
    };
    assert_eq!(*key, Span::new(10, 14));
    assert!(matches!(chain.base().kind(), ExprKind::Identity));
}

#[test]
fn parser_preserves_four_inline_postfix_steps_in_order() {
    let text = ".a.b.c.d";
    let expression = Parser::new(source(), text).parse_query().into_valid_syntax().unwrap();
    let chain = postfix(&expression);

    assert_eq!(
        chain
            .steps()
            .iter()
            .map(|step| &text[step.span.range()])
            .collect::<Vec<_>>(),
        vec![".a", ".b", ".c", ".d"]
    );
}

#[test]
fn parser_preserves_five_inline_postfix_steps_in_order() {
    let text = ".a.b.c.d.e";
    let expression = Parser::new(source(), text).parse_query().into_valid_syntax().unwrap();
    let chain = postfix(&expression);

    assert_eq!(
        chain
            .steps()
            .iter()
            .map(|step| &text[step.span.range()])
            .collect::<Vec<_>>(),
        vec![".a", ".b", ".c", ".d", ".e"]
    );
}

#[test]
fn parser_preserves_six_inline_postfix_steps_in_order() {
    let text = ".a.b.c.d.e.f";
    let expression = Parser::new(source(), text).parse_query().into_valid_syntax().unwrap();
    let chain = postfix(&expression);

    assert_eq!(
        chain
            .steps()
            .iter()
            .map(|step| &text[step.span.range()])
            .collect::<Vec<_>>(),
        vec![".a", ".b", ".c", ".d", ".e", ".f"]
    );
}

#[test]
fn parser_preserves_each_postfix_delimiter_and_optional_suffix_span() {
    let text = r#".field?.[0]?[:1]?.@name?.@["quoted"]?.@(dynamic)?.&href?.&["aria"]?.&("lang")?"#;
    let parsed = Parser::new(source(), text).parse_query().into_valid_syntax().unwrap();
    let chain = postfix(&parsed);
    assert_eq!(chain.steps().len(), 9);
    assert!(matches!(chain.base().kind(), ExprKind::Identity));

    for step in chain.steps() {
        let operator = &text[step.operator_span.range()];
        match step.segment {
            PostfixSegment::Field { .. } => assert_eq!(operator, "."),
            PostfixSegment::Index { .. } | PostfixSegment::Slice { .. } => {
                assert!(matches!(operator, "." | "["));
            }
            PostfixSegment::NodeAccessor { .. } => assert_eq!(operator, ".@"),
            PostfixSegment::Attribute { .. } => assert_eq!(operator, ".&"),
            PostfixSegment::ErrorSuppression => assert_eq!(operator, "?"),
        }
        if let Some(optional) = step.optional_suffix_span {
            assert_eq!(&text[optional.range()], "?");
            assert_eq!(step.span.end(), optional.end());
        }
    }

    let PostfixSegment::Index {
        open_span, close_span, ..
    } = &chain.steps()[1].segment
    else {
        panic!("ordinary bracket index");
    };
    assert_eq!(&text[open_span.range()], "[");
    assert_eq!(&text[close_span.range()], "]");

    let PostfixSegment::Slice {
        open_span, close_span, ..
    } = &chain.steps()[2].segment
    else {
        panic!("ordinary slice");
    };
    assert_eq!(&text[open_span.range()], "[");
    assert_eq!(&text[close_span.range()], "]");

    for index in [4, 7] {
        let (PostfixSegment::NodeAccessor { selector } | PostfixSegment::Attribute { selector }) =
            &&chain.steps()[index].segment
        else {
            panic!("bracket accessor");
        };
        let (open_span, close_span) = match selector {
            AccessorSelector::Bracket {
                open_span, close_span, ..
            } => (*open_span, *close_span),
            _ => panic!("bracket accessor"),
        };
        assert_eq!(&text[open_span.range()], "[");
        assert_eq!(&text[close_span.range()], "]");
        assert_eq!(
            &text[selector.selector_span().range()],
            if index == 4 { r#""quoted""# } else { r#""aria""# }
        );
    }

    for index in [5, 8] {
        let (PostfixSegment::NodeAccessor { selector } | PostfixSegment::Attribute { selector }) =
            &&chain.steps()[index].segment
        else {
            panic!("dynamic accessor");
        };
        let (open_span, close_span) = match selector {
            AccessorSelector::Dynamic {
                open_span, close_span, ..
            } => (*open_span, *close_span),
            _ => panic!("dynamic accessor"),
        };
        assert_eq!(&text[open_span.range()], "(");
        assert_eq!(&text[close_span.range()], ")");
        assert!(matches!(selector, AccessorSelector::Dynamic { .. }));
    }
}

#[test]
fn standalone_optional_is_an_error_suppression_step() {
    let text = "(.)?";
    let expression = Parser::new(source(), text).parse_query().into_valid_syntax().unwrap();
    let chain = postfix(&expression);
    assert_eq!(chain.steps().len(), 1);
    let step = &chain.steps()[0];
    assert!(matches!(step.segment, PostfixSegment::ErrorSuppression));
    assert_eq!(&text[step.operator_span.range()], "?");
    assert_eq!(step.optional_suffix_span, None);
    assert_eq!(step.span, step.operator_span);
    assert!(matches!(chain.base().kind(), ExprKind::Group { .. }));
    let ExprKind::Group { expression: base, .. } = chain.base().kind() else {
        panic!("authored group");
    };
    assert!(matches!(base.kind(), ExprKind::Identity));
}

#[test]
fn authored_call_shape_records_arguments_and_parentheses() {
    let explicit = Parser::new(source(), "map(.; .name)")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Call(call) = explicit.kind() else {
        panic!("explicit call");
    };
    assert_eq!(call.args.len(), 2);
    assert!(call.parentheses.is_some());

    let bare = Parser::new(source(), "foo").parse_query().into_valid_syntax().unwrap();
    let ExprKind::Call(call) = bare.kind() else {
        panic!("bare call");
    };
    assert_eq!(call.args.len(), 0);
    assert!(call.parentheses.is_none());
}

#[test]
fn parser_builds_ten_thousand_postfix_steps_in_authored_order() {
    let mut text = String::new();
    for index in 0..2_500 {
        use std::fmt::Write;
        write!(text, ".field{index}[0].@tag.&href?").unwrap();
    }
    let parsed = Parser::new(source(), &text).parse_query().into_valid_syntax().unwrap();
    let chain = postfix(&parsed);
    assert_eq!(chain.steps().len(), 10_000);
    assert!(matches!(chain.steps()[0].segment, PostfixSegment::Field { .. }));
    assert!(matches!(&chain.steps()[1].segment, PostfixSegment::Index { .. }));
    assert!(matches!(&chain.steps()[2].segment, PostfixSegment::NodeAccessor { .. }));
    assert!(matches!(
        chain.steps().last().unwrap().segment,
        PostfixSegment::Attribute { .. }
    ));
    assert!(
        chain
            .steps()
            .windows(2)
            .all(|steps| steps[0].span.end() <= steps[1].span.start())
    );
}

#[test]
fn parser_reports_missing_expression() {
    let parsed = Parser::new(source(), ". |").parse_query();

    assert!(parsed.syntax().is_some());
    let diagnostics = parsed.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code().to_string(), "syntax.expected-expression");
    assert_eq!(diagnostics[0].labels()[0].span(), Span::new(3, 3));
}

#[test]
fn parser_rejects_a_slice_with_both_bounds_absent() {
    // `.[:]` is a SYNTAX error (the grammar requires at least one authored bound), not an engine rejection: the
    // diagnostic must come from the PARSER so the rejection carries the syntax class rather than an
    // unsupported-construct class.
    let parsed = Parser::new(source(), ".[:]").parse_query();

    let diagnostics = parsed.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code().to_string(), "syntax.expected-expression");
    assert_eq!(diagnostics[0].labels()[0].span(), Span::new(3, 4));
    assert!(parsed.into_valid_syntax().is_err());
}

#[test]
fn parser_accepts_every_slice_spelling_with_at_least_one_bound() {
    // The discriminators around `.[:]`: one open end is legal on either side, and `.[null:null]` is the legal BOTH-open
    // spelling.
    for text in [".[1:2]", ".[:2]", ".[1:]", ".[null:null]", ".[null:]", ".[:null]"] {
        let parsed = Parser::new(source(), text).parse_query();
        assert!(parsed.diagnostics().is_empty(), "{text}");
        let parsed = parsed.into_valid_syntax().expect("valid syntax");
        let chain = postfix(&parsed);
        assert_eq!(chain.steps().len(), 1, "{text}");
        assert!(
            matches!(&chain.steps()[0].segment, PostfixSegment::Slice { .. }),
            "{text} must stay one Slice segment"
        );
    }
}

#[test]
fn parser_keeps_recursive_descent_a_primary_with_postfix_chains() {
    // `..` is an ordinary primary: `..[0]?` is one postfix chain over a `RecursiveDescent` base, so the engine composes
    // descent with normal steps.
    let parsed = Parser::new(source(), "..[0]?")
        .parse_query()
        .into_valid_syntax()
        .expect("valid syntax");
    let chain = postfix(&parsed);
    assert!(matches!(chain.base().kind(), ExprKind::RecursiveDescent));
    assert_eq!(chain.steps().len(), 1);
    let step = &chain.steps()[0];
    assert!(matches!(step.segment, PostfixSegment::Index { .. }));
    assert!(step.optional_suffix_span.is_some());
}

#[test]
fn parser_builds_calls_with_semicolon_arguments() {
    let parsed = Parser::new(source(), "map(.name; .id)").parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("call should parse");
    let ExprKind::Call(call) = expression.kind() else {
        panic!("expected call");
    };
    assert_eq!(call.args.len(), 2);
    assert_eq!(call.name, Span::new(0, 3));
    assert_eq!(call.parentheses, Some(Span::new(3, 15)));
    assert_eq!(call.args[0].separator_span, Some(Span::new(9, 10)));
}

#[test]
fn parser_builds_array_and_object_constructors() {
    let parsed = Parser::new(source(), r#"[{name: .name, "id": .id, $raw}]"#).parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("array should parse");
    let ExprKind::Array {
        expression: Some(element),
        ..
    } = expression.kind()
    else {
        panic!("expected populated array");
    };
    let ExprKind::Object { members, .. } = element.kind() else {
        panic!("expected object inside array");
    };
    assert_eq!(members.len(), 3);
    assert!(matches!(members[0].key, ObjectKey::Name(_)));
    assert!(matches!(members[1].key, ObjectKey::String(_)));
    assert!(matches!(members[2].key, ObjectKey::Variable(_)));
}

/// Object values reject query-head forms (`def`, `label`, `as`). Only a grouped `(…)` reopens the query grammar.
#[test]
fn object_values_reject_query_head_forms() {
    for text in ["{a: def f: 1; f}", "{a: label $l0 | .}", "{a: . as $x | $x}"] {
        let parsed = Parser::new(source(), text).parse_query();
        assert!(
            !parsed.diagnostics().is_empty(),
            "{text} must reject a query-head object value: {:?}",
            parsed.syntax()
        );
    }
    let grouped = Parser::new(source(), "{a: (def f: 1; f)}").parse_query();
    assert!(
        grouped.diagnostics().is_empty(),
        "a grouped query-head value must parse: {:?}",
        grouped.diagnostics()
    );
}

#[test]
fn parser_builds_jqf_accessor_postfixes() {
    let parsed = Parser::new(source(), ".item.@comment.leading | .item.&href?").parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("pipe should parse");
    let ExprKind::Binary(pipe) = expression.kind() else {
        panic!("expected pipe expression");
    };

    let metadata = postfix(&pipe.left);
    assert_eq!(metadata.steps().len(), 3);
    assert!(matches!(
        &metadata.steps()[1].segment,
        PostfixSegment::NodeAccessor { .. }
    ));

    let attribute = postfix(&pipe.right);
    assert_eq!(attribute.steps().len(), 2);
    assert_eq!(attribute.steps()[1].optional_suffix_span, Some(Span::new(36, 37)));
    assert!(matches!(
        &attribute.steps()[1].segment,
        PostfixSegment::Attribute { .. }
    ));
}

#[test]
fn parser_builds_bracket_and_dynamic_jqf_accessors() {
    let parsed = Parser::new(
        source(),
        r#".item.&["data-id"]? | .item.&("href") | .item.@["comment"]"#,
    )
    .parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("accessors should parse");
    let ExprKind::Binary(pipe) = expression.kind() else {
        panic!("expected pipe expression");
    };
    let attribute = postfix(&pipe.left);
    assert_eq!(
        attribute.steps().last().unwrap().optional_suffix_span,
        Some(Span::new(18, 19))
    );
    assert!(matches!(
        attribute.steps().last().unwrap().segment,
        PostfixSegment::Attribute { .. }
    ));

    let ExprKind::Binary(right_pipe) = pipe.right.kind() else {
        panic!("expected right-associated pipe expression");
    };
    let dynamic_attribute = postfix(&right_pipe.left);
    assert!(matches!(
        dynamic_attribute.steps().last().unwrap().segment,
        PostfixSegment::Attribute { .. }
    ));

    let metadata = postfix(&right_pipe.right);
    assert!(matches!(
        metadata.steps().last().unwrap().segment,
        PostfixSegment::NodeAccessor { .. }
    ));
}

#[test]
fn parser_builds_identity_rooted_jqf_accessors() {
    let parsed = Parser::new(source(), r#".@document.@source.path | .&href | .@["comment"]"#).parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("accessors should parse");
    // The accessor spelling must survive as NodeAccessor/Attribute postfix steps (never rewritten into object keys):
    // two `.@name` forms, the bracket `.@["comment"]`, and one `.&href`.
    let accessors = SyntaxWalk::query(expression)
        .filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node),
            WalkEvent::Exit(_) => None,
        })
        .filter_map(|node| match node {
            SyntaxNodeRef::PostfixStep(step) => Some(&step.segment),
            _ => None,
        })
        .filter(|segment| {
            matches!(
                segment,
                PostfixSegment::NodeAccessor { .. } | PostfixSegment::Attribute { .. }
            )
        })
        .count();
    assert_eq!(accessors, 4);
}

#[test]
fn parser_rejects_whitespace_inside_jqf_accessor_introducers() {
    for source_text in [". @json", ". @", r#". @"name""#, ". &href", ".name . @comment"] {
        let parsed = Parser::new(source(), source_text).parse_query();

        assert!(parsed.syntax().is_some());
        let diagnostics = parsed.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{source_text}");
        assert_eq!(
            diagnostics[0].code().to_string(),
            "syntax.separated-accessor",
            "{source_text}"
        );
    }
}

#[test]
fn parser_does_not_report_separated_accessor_after_non_dot_base() {
    for source_text in [".name @json", "@json @uri"] {
        let parsed = Parser::new(source(), source_text).parse_query();

        assert!(parsed.syntax().is_some());
        let diagnostics = parsed.diagnostics();
        assert_eq!(diagnostics.len(), 1, "{source_text}");
        assert_eq!(
            diagnostics[0].code().to_string(),
            "syntax.expected-token",
            "{source_text}"
        );
    }
}

#[test]
fn parser_rejects_non_string_bracket_accessors() {
    let parsed = Parser::new(source(), ".item.&[.name]").parse_query();

    assert!(parsed.syntax().is_some());
    let diagnostics = parsed.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code().to_string(), "syntax.expected-token");
}

#[test]
fn parser_rejects_catch_after_an_ungrouped_infix_operand() {
    let query = r#"try 1 + error("x") catch 2"#;
    let parsed = Parser::new(source(), query).parse_query();

    assert!(parsed.syntax().is_some());
    let diagnostics = parsed.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code().to_string(), "syntax.expected-token");
    let catch_start = query.find("catch").unwrap();
    assert_eq!(
        diagnostics[0].labels()[0].span(),
        Span::from_usize(catch_start, catch_start + "catch".len())
    );
}

#[test]
fn parser_keeps_catchless_try_at_term_scope() {
    let parsed = Parser::new(source(), "try 1 + 2").parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("addition should parse");
    let ExprKind::Binary(addition) = expression.kind() else {
        panic!("expected addition outside catchless try");
    };
    assert!(matches!(addition.left.kind(), ExprKind::Try(_)));
    assert!(matches!(addition.right.kind(), ExprKind::Number));

    let grouped = Parser::new(source(), "try (1 + 2)").parse_query();
    let ExprKind::Try(try_expr) = grouped.syntax().expect("grouped try").kind() else {
        panic!("expected grouped try expression");
    };
    assert!(matches!(try_expr.expr.kind(), ExprKind::Group { .. }));
    assert!(try_expr.handler.is_none());
}

#[test]
fn parser_keeps_try_handler_comma_and_pipe_outside_handler() {
    let parsed = Parser::new(source(), "try 1 catch 2, 3 | .").parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("expression should parse");
    let ExprKind::Binary(pipe) = expression.kind() else {
        panic!("expected top-level pipe");
    };
    let ExprKind::Binary(comma) = pipe.left.kind() else {
        panic!("expected comma on pipe lhs");
    };
    let ExprKind::Try(try_expr) = comma.left.kind() else {
        panic!("expected try on comma lhs");
    };
    assert!(matches!(
        try_expr.handler.as_deref().map(jqf_syntax::Expr::kind),
        Some(ExprKind::Number)
    ));
}

#[test]
fn parser_rejects_label_in_try_body_with_a_single_expected_expression() {
    // The sibling loop in parser_keeps_label_at_query_heads_and_rejects_it_at_expr_operands rejects `try label $l0 | .`
    // as non-empty diagnostics; this row pins the exact code so the rejection cannot drift to a different syntax class.
    let parsed = Parser::new(source(), "try label $l0 | .").parse_query();

    assert!(parsed.syntax().is_some());
    assert_eq!(parsed.diagnostics().len(), 1);
    assert_eq!(parsed.diagnostics()[0].code().to_string(), "syntax.expected-expression");
}

#[test]
fn parser_keeps_label_at_query_heads_and_rejects_it_at_expr_operands() {
    // `label` is a Query-head form: valid at the top level, after a pipe, and in every Query slot, but never as an
    // Expr/Term operand (`try`, `catch`, arithmetic, object values) unless parenthesized. The last row pins the sibling
    // law: `as` is also a Query-head form, so an object value cannot end in an unparenthesized binding either.
    for accepted in [
        "label $l0 | .",
        ". | label $l0 | .",
        "if true then label $l0 | . else . end",
        "try (label $l0 | .)",
        "{a: (label $l0 | .)}",
        "reduce .[] as $x (0; label $l0 | .)",
    ] {
        let parsed = Parser::new(source(), accepted).parse_query();
        assert!(
            parsed.diagnostics().is_empty(),
            "{accepted:?} should parse clean: {:?}",
            parsed.diagnostics()
        );
    }
    for rejected in [
        "try label $l0 | .",
        "try . catch label $l0 | .",
        "5 + label $l0 | .",
        "{a: label $l0 | .}",
        "{a: . | label $l0 | .}",
        "{a: . as $x | $x}",
    ] {
        let parsed = Parser::new(source(), rejected).parse_query();
        assert!(
            !parsed.diagnostics().is_empty(),
            "{rejected:?} should be rejected: the grammar answers a syntax error"
        );
    }
}

#[test]
fn parser_rejects_nested_destructuring_alternatives() {
    let parsed = Parser::new(source(), ". as [$a ?// $b] | .").parse_query();

    assert!(parsed.syntax().is_some());
    let diagnostics = parsed.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code().to_string(), "syntax.unexpected-token");
    assert_eq!(diagnostics[0].labels()[0].span(), Span::new(9, 12));
}

#[test]
fn parser_rejects_shorthand_dynamic_object_keys() {
    let parsed = Parser::new(source(), "{(.foo)}").parse_query();

    assert!(parsed.syntax().is_some());
    let diagnostics = parsed.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code().to_string(), "syntax.unexpected-token");
}

#[test]
fn parser_rejects_unparenthesized_assignment_chains() {
    let parsed = Parser::new(source(), ".a = .b = .c").parse_query();

    assert!(parsed.syntax().is_some());
    let diagnostics = parsed.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code().to_string(), "syntax.chained-assignment");
    assert_eq!(diagnostics[0].labels()[0].span(), Span::new(8, 9));
}

#[test]
fn parser_builds_control_expressions() {
    let parsed = Parser::new(source(), "if .ok then try .value catch null else empty end").parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("conditional should parse");
    let ExprKind::If(conditional) = expression.kind() else {
        panic!("expected conditional");
    };
    assert!(conditional.else_branch.is_some());
    assert_eq!(conditional.branches.len(), 1);
    let ExprKind::Try(try_expr) = conditional.branches[0].then_branch.kind() else {
        panic!("expected try in then branch");
    };
    assert!(try_expr.handler.is_some());
}

#[test]
fn parser_builds_binding_and_loop_expressions() {
    let parsed = Parser::new(
        source(),
        "let {name: $name} = .user | reduce .items as $item (0; . + $item)",
    )
    .parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("let should parse");
    let ExprKind::Binding(binding) = expression.kind() else {
        panic!("expected let binding");
    };
    assert!(matches!(binding.pattern.kind(), PatternKind::Object(_)));
    let ExprKind::Reduce(reduce) = binding.body.kind() else {
        panic!("expected reduce body");
    };
    assert!(matches!(reduce.binding.kind(), PatternKind::Variable));
    assert!(reduce.extract.is_none());
}

#[test]
fn parser_rejects_omitted_loop_bindings_with_explicit_recovery_pattern() {
    let parsed = Parser::new(source(), "foreach .items (0; . + 1; .)").parse_query();

    assert!(!parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("foreach should parse");
    let ExprKind::Foreach(foreach) = expression.kind() else {
        panic!("expected foreach");
    };
    assert!(matches!(foreach.binding.kind(), PatternKind::Error));
    assert!(foreach.extract.is_some());
}

#[test]
fn parser_builds_as_bindings_and_label_break() {
    let parsed = Parser::new(source(), ".items as $value | label $done | break $done").parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("binding should parse");
    let ExprKind::Binding(binding) = expression.kind() else {
        panic!("expected as binding");
    };
    let value = &binding.value;
    assert!(matches!(value.kind(), ExprKind::Postfix(_)));
    assert!(matches!(binding.pattern.kind(), PatternKind::Variable));
    let ExprKind::Label { body, .. } = binding.body.kind() else {
        panic!("expected label body");
    };
    assert!(matches!(body.kind(), ExprKind::Break { .. }));
}

#[test]
fn parser_builds_format_templates() {
    let parsed = Parser::new(source(), r#"@json "value=\(.)""#).parse_query();

    assert!(parsed.diagnostics().is_empty());
    let expression = parsed.syntax().expect("format template should parse");
    assert!(matches!(expression.kind(), ExprKind::FormatTemplate { .. }));
}

#[test]
fn parser_builds_source_units() {
    let parsed = Parser::new(
        source(),
        r#"module {name: "demo"}; import "math" as math; include "strings"; def twice(f): f | f; math::sqrt(.x)"#,
    )
    .parse_program();

    assert!(parsed.diagnostics().is_empty());
    let unit = parsed.syntax().expect("source unit should parse");
    assert_eq!(unit.items.len(), 4);
    assert!(matches!(unit.items[0], SourceItem::Module(_)));
    assert!(matches!(unit.items[1], SourceItem::Import(_)));
    assert!(matches!(unit.items[2], SourceItem::Include(_)));
    assert!(matches!(unit.items[3], SourceItem::Def(_)));
    assert!(unit.expression.is_some());
}

#[test]
fn parser_allows_library_units_without_final_expression() {
    let parsed = Parser::new(source(), r"def id: .;").parse_library();

    assert!(parsed.diagnostics().is_empty());
    let unit = parsed.syntax().expect("library should parse");
    assert_eq!(unit.items.len(), 1);
    assert!(unit.expression.is_none());
}

#[test]
fn definition_names_admit_literal_like_words_and_reject_real_keywords() {
    for accepted in [
        "def empty: 1; 0",
        "def null: 1; 0",
        "def true: 1; 0",
        "def false: 1; 0",
        "def empty($x): $x; 0",
        "def f(empty): empty; f(1)",
        "def f(true): 1; f(1)",
        "def foo::empty: 1; 0",
        "def empty::foo: 1; 0",
        r#"import "x" as empty; 0"#,
        r#"import "x" as true; 0"#,
        "def if::foo: 1; if::foo",
        "def and::x: 3; and::x",
        "def foo::if: 1; foo::if",
        // `let` is contextual: the binder keeps its role and every literal-like name position accepts the spelling.
        "def let: 1; 0",
        "def f(let): let; f(1)",
        r#"import "x" as let; 0"#,
        "def let($x): $x; let(5)",
    ] {
        let parsed = Parser::new(source(), accepted).parse_program();
        assert!(
            parsed.diagnostics().is_empty(),
            "{accepted:?} should parse clean: {:?}",
            parsed.diagnostics()
        );
    }
    for rejected in [
        "def if: 1; 0",
        "def then: 1; 0",
        "def and: 1; 0",
        "def or: 1; 0",
        "def def: 1; 0",
        "def f(if): 1; f(1)",
        r#"import "x" as if; 0"#,
    ] {
        let parsed = Parser::new(source(), rejected).parse_program();
        assert!(
            !parsed.diagnostics().is_empty(),
            "{rejected:?} should stay a syntax error"
        );
    }
}

#[test]
fn literal_like_words_become_calls_only_with_arguments_or_qualification() {
    let empty = Parser::new(source(), "empty")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    assert!(matches!(empty.kind(), ExprKind::Empty));

    let true_lit = Parser::new(source(), "true").parse_query().into_valid_syntax().unwrap();
    assert!(matches!(true_lit.kind(), ExprKind::Bool(true)));

    let empty_call = Parser::new(source(), "empty(5)")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Call(call) = empty_call.kind() else {
        panic!("empty(5) must be a call, got {:?}", empty_call.kind());
    };
    assert_eq!(call.args.len(), 1);
    assert!(call.parentheses.is_some());

    let true_call = Parser::new(source(), "true(5)")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    assert!(matches!(true_call.kind(), ExprKind::Call(_)));

    let qualified = Parser::new(source(), "empty::foo")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Call(call) = qualified.kind() else {
        panic!("empty::foo must be a call");
    };
    assert!(call.parentheses.is_none());
}

#[test]
fn let_is_contextual_between_binder_and_name() {
    // The binder keeps its form.
    let binder = Parser::new(source(), "let $x = 1 | $x")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Binding(binding) = binder.kind() else {
        panic!("let $x = … must stay a binding, got {:?}", binder.kind());
    };
    assert!(matches!(binding.form, BindingForm::Let { .. }));

    for destructuring in ["let [$a, $b] = . | $a", "let {$k: $v} = . | $v"] {
        let parsed = Parser::new(source(), destructuring).parse_query();
        assert!(
            parsed.diagnostics().is_empty(),
            "{destructuring:?} must stay a binder: {:?}",
            parsed.diagnostics()
        );
    }

    // A bare `let` is the user's zero-arity call (after `def let: …`).
    let bare = Parser::new(source(), "def let: 1; let")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Definition(definition) = bare.kind() else {
        panic!("def body must parse, got {:?}", bare.kind());
    };
    let ExprKind::Call(call) = definition.body.kind() else {
        panic!("bare let must be a call, got {:?}", definition.body.kind());
    };
    assert!(call.parentheses.is_none());
    assert!(call.args.is_empty());

    // `let(...)` is a call with arguments.
    let call_args = Parser::new(source(), "let(5)")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Call(call) = call_args.kind() else {
        panic!("let(5) must be a call, got {:?}", call_args.kind());
    };
    assert_eq!(call.args.len(), 1);
    assert!(call.parentheses.is_some());

    // `let::name` is a qualified name.
    let qualified = Parser::new(source(), "let::foo")
        .parse_query()
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Call(call) = qualified.kind() else {
        panic!("let::foo must be a call, got {:?}", qualified.kind());
    };
    assert!(call.parentheses.is_none());
}
