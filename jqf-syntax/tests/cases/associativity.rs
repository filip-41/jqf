use jqf_syntax::{BinaryOp, BindingForm, ExprKind, ObjectKey, SourceItem};

use super::common::{parse_program, parse_query, source};

fn assert_valid(query: &str) {
    let parsed = parse_query(source(), query);
    assert!(parsed.is_valid(), "{query:?}: {:?}", parsed.diagnostics());
}

fn assert_invalid(query: &str) {
    let parsed = parse_query(source(), query);
    assert!(
        !parsed.diagnostics().is_empty(),
        "{query:?} unexpectedly parsed as {:?}",
        parsed.syntax()
    );
}

#[test]
fn pipe_and_alternative_associate_right() {
    let pipe = parse_query(source(), "a | b | c").into_valid_syntax().unwrap();
    let ExprKind::Binary(root) = pipe.kind() else {
        panic!("expected pipe");
    };
    assert_eq!(root.op, BinaryOp::Pipe);
    assert!(matches!(root.left.kind(), ExprKind::Call(_)));
    let ExprKind::Binary(right) = root.right.kind() else {
        panic!("pipe must associate right");
    };
    assert_eq!(right.op, BinaryOp::Pipe);

    let alternative = parse_query(source(), "a // b // c").into_valid_syntax().unwrap();
    let ExprKind::Binary(root) = alternative.kind() else {
        panic!("expected alternative");
    };
    assert_eq!(root.op, BinaryOp::Alternative);
    let ExprKind::Binary(right) = root.right.kind() else {
        panic!("alternative must associate right");
    };
    assert_eq!(right.op, BinaryOp::Alternative);
}

#[test]
fn try_catch_operand_stops_before_following_infix() {
    for invalid in [
        "try .a + .b catch .",
        "try -.a + .b catch .",
        "try .a and .b catch .",
        "try .a // .b catch .",
        "try .a = .b catch .",
        "try .a, .b catch .",
        "try .a | .b catch .",
        "try .a as $x | $x catch .",
        "try let $x = .a | $x catch .",
        "try label $done | break $done catch .",
    ] {
        assert_invalid(invalid);
    }

    for valid in [
        "try .a catch .",
        "try .a?",
        "try (.a + .b) catch .",
        "try (-.a + .b) catch .",
        "try (.a and .b) catch .",
        "try (.a // .b) catch .",
        "try (.a = .b) catch .",
        "try (.a, .b) catch .",
        "try (.a | .b) catch .",
        "try (.a as $x | $x) catch .",
        "try (let $x = .a | $x) catch .",
        "try (label $done | break $done) catch .",
        "try if .ok then .value else empty end catch .",
        "try reduce .[] as $x (0; . + $x) catch .",
    ] {
        assert_valid(valid);
    }
}

#[test]
fn catch_handler_binds_at_term_level_not_operator_breadth() {
    // The catch handler is one TERM; a trailing operator composes on the whole `try … catch …` OUTSIDE it. `try .
    // catch . + 1` is `(try . catch .) + 1`, so the root is `+` and the Try's handler is a bare identity `.`, never `.
    // + 1`.
    let add = parse_query(source(), "try . catch . + 1").into_valid_syntax().unwrap();
    let ExprKind::Binary(root) = add.kind() else {
        panic!("root must be the `+` binary, got {:?}", add.kind());
    };
    assert_eq!(root.op, BinaryOp::Add);
    let ExprKind::Try(try_expr) = root.left.kind() else {
        panic!("the `+` left operand must be the try, got {:?}", root.left.kind());
    };
    let handler = try_expr.handler.as_ref().expect("catch handler present");
    assert!(
        matches!(handler.kind(), ExprKind::Identity),
        "handler must be the bare identity term, got {:?}",
        handler.kind()
    );

    // `try . catch false // 9` is `(try . catch false) // 9`: root is `//`, its left is the try, and the handler is the
    // bare `false` term.
    let alt = parse_query(source(), "try . catch false // 9")
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Binary(root) = alt.kind() else {
        panic!("root must be the `//` binary, got {:?}", alt.kind());
    };
    assert_eq!(root.op, BinaryOp::Alternative);
    let ExprKind::Try(try_expr) = root.left.kind() else {
        panic!("the `//` left operand must be the try");
    };
    let handler = try_expr.handler.as_ref().expect("catch handler present");
    assert!(
        matches!(handler.kind(), ExprKind::Bool(false)),
        "handler must be the bare `false` term, got {:?}",
        handler.kind()
    );
}

/// An object key spelled as a bare name is a qualified name, the same production the call grammar uses: `a::b` is one
/// key, keyword segments included, and it means exactly the string it is spelled with. The binding pattern reads the
/// same key form, so a program can destructure what it can construct.
#[test]
fn a_bare_object_key_is_a_whole_qualified_name() {
    for valid in [
        "{a::b: 1}",
        "{a::b::c: 1}",
        "{true::b: 1}",
        "{a: 1}",
        ". as {a::b: $x} | $x",
        ". as {true::b: $x} | $x",
    ] {
        assert_valid(valid);
    }

    let object = parse_query(source(), "{a::b: 1}").into_valid_syntax().unwrap();
    let ExprKind::Object { members, .. } = object.kind() else {
        panic!("expected an object, got {:?}", object.kind());
    };
    let ObjectKey::Name(span) = &members[0].key else {
        panic!("a bare key is a name key, got {:?}", members[0].key);
    };
    assert_eq!(span.range(), 1..5, "the key span must cover `a::b` whole");
}

/// A format may introduce a string object key, in the constructor and in the binding pattern alike: `{@base64 "x": 1}`
/// applies the format to the string and uses the result as the key. The value-less constructor shorthand `{@text "k"}`
/// is the same key with an implied lookup of that key on the input. The format and the string are one key — a format
/// with no string after it is not one, and neither is a key form the grammar admits nowhere else. The pattern form
/// still requires a value: a value-less format key is constructor shorthand only.
#[test]
fn a_format_may_introduce_a_string_object_key() {
    for valid in [
        r#"{@base64 "x": 1}"#,
        r#"{@text "k": 1}"#,
        r#"{@text "k"}"#,
        r#"{@base64 "\(1+1)": 3}"#,
        r#". as {@text "k": $v} | $v"#,
        r#". as {@base64 "a": $x} | $x"#,
    ] {
        assert_valid(valid);
    }

    for invalid in ["{@text: 1}", "{(.key)}", "{@text}"] {
        assert_invalid(invalid);
    }

    let object = parse_query(source(), r#"{@base64 "x": 1}"#)
        .into_valid_syntax()
        .unwrap();
    let ExprKind::Object { members, .. } = object.kind() else {
        panic!("expected an object, got {:?}", object.kind());
    };
    let ObjectKey::Expr(key) = &members[0].key else {
        panic!("a format key is an expression key, got {:?}", members[0].key);
    };
    assert!(
        matches!(key.kind(), ExprKind::FormatTemplate { .. }),
        "the key is the format applied to the string, got {:?}",
        key.kind()
    );
}

#[test]
fn trailing_dot_numbers_do_not_consume_jqf_accessors() {
    for valid in ["1..field", r#"1.."quoted""#, "1..[0]", "1.@tag", "1.&unit"] {
        assert_valid(valid);
    }
    for invalid in ["1..2", "1...field", ".5..field"] {
        assert_invalid(invalid);
    }
}

#[test]
fn dot_postfix_bracket_is_index_or_each_never_a_slice() {
    // A dot postfix admits `[query]` — an index or an each — but never a slice: the colon inside the brackets is a
    // syntax error there.
    for invalid in ["[[].[1:]]", "\"ab\".[0:1]", "null.[1:]", ".[1:].[2:]"] {
        assert_invalid(invalid);
    }

    // The legal neighbors stay legal: the root identity slice, bracket-term and field-result slices (no dot between
    // term and bracket), a dot index, dot-each after a term, `5.[1:2]` — where the trailing dot belongs to the
    // number, leaving a bracket-term slice whose only failure is at runtime — and the `.@`/`.&` accessor surface.
    for valid in [
        "5.[1:2]",
        ".[1:]",
        "[0][1:2]",
        ".a[1:]",
        ".a[1]",
        ".[]",
        "[0,1] as $x | $x.[]",
        ".@name",
        ".@[\"name\"]",
        ".&href",
    ] {
        assert_valid(valid);
    }
}

#[test]
fn syntax_nodes_preserve_authored_keywords_and_delimiters() {
    let program = r#"module {name: "demo"};
import "math" as math {search: "."};
include "strings";
def combine($left; right): [$left, right];
combine(.a; .b)"#;
    let unit = parse_program(source(), program).into_valid_syntax().unwrap();
    let SourceItem::Module(module) = &unit.items[0] else {
        panic!("module item");
    };
    assert_eq!(&program[module.module_keyword_span.range()], "module");
    assert_eq!(&program[module.semicolon_span.range()], ";");

    let SourceItem::Import(import) = &unit.items[1] else {
        panic!("import item");
    };
    assert_eq!(&program[import.import_keyword_span.range()], "import");
    assert_eq!(&program[import.path.span().range()], r#""math""#);
    assert_eq!(&program[import.as_keyword_span.range()], "as");
    assert_eq!(&program[import.semicolon_span.range()], ";");

    let SourceItem::Include(include) = &unit.items[2] else {
        panic!("include item");
    };
    assert_eq!(&program[include.include_keyword_span.range()], "include");
    assert_eq!(&program[include.path.span().range()], r#""strings""#);

    let SourceItem::Def(definition) = &unit.items[3] else {
        panic!("definition item");
    };
    assert_eq!(&program[definition.def_keyword_span.range()], "def");
    assert_eq!(
        &program[definition.parameter_parentheses.unwrap().range()],
        "($left; right)"
    );
    assert_eq!(definition.params.len(), 2);
    assert_eq!(&program[definition.params[0].separator_span.unwrap().range()], ";");
    assert_eq!(&program[definition.colon_span.range()], ":");
    assert_eq!(&program[definition.semicolon_span.range()], ";");
}

/// `$__loc__` is a reserved location binding: a read in expression position and the object-constructor shorthand stay
/// valid; a binder, pattern, label, `break` target, definition parameter, or import alias does not.
#[test]
fn location_binding_is_reserved_as_a_binder() {
    for valid in ["$__loc__", "{$__loc__}", ". as $x | $x", "{$__loc__: 1}"] {
        assert_valid(valid);
    }

    for invalid in [
        "1 as $__loc__ | $__loc__",
        "[1] as [$__loc__] | $__loc__",
        r#"{"a":1} as {$__loc__} | $__loc__"#,
        "reduce 1 as $__loc__ (0; .)",
        "foreach 1 as $__loc__ (0; .)",
        "label $__loc__ | 1",
        "def f($__loc__): 1; f(1)",
        "let $__loc__ = 1 | $__loc__",
        "break $__loc__",
    ] {
        assert_invalid(invalid);
    }

    let imported = parse_program(source(), r#"import "x" as $__loc__; ."#);
    assert!(
        !imported.diagnostics().is_empty(),
        "import alias unexpectedly parsed as {:?}",
        imported.syntax()
    );
}

#[test]
fn binding_nodes_distinguish_as_from_let_and_retain_separators() {
    let as_query = ".items as $item | $item";
    let binding = parse_query(source(), as_query).into_valid_syntax().unwrap();
    let ExprKind::Binding(binding) = binding.kind() else {
        panic!("as binding");
    };
    let BindingForm::As {
        as_keyword_span,
        pipe_span,
    } = binding.form
    else {
        panic!("as form");
    };
    assert_eq!(&as_query[as_keyword_span.range()], "as");
    assert_eq!(&as_query[pipe_span.range()], "|");

    let let_query = "let $item = .items | $item";
    let binding = parse_query(source(), let_query).into_valid_syntax().unwrap();
    let ExprKind::Binding(binding) = binding.kind() else {
        panic!("let binding");
    };
    let BindingForm::Let {
        let_keyword_span,
        equals_span,
        pipe_span,
    } = binding.form
    else {
        panic!("let form");
    };
    assert_eq!(&let_query[let_keyword_span.range()], "let");
    assert_eq!(&let_query[equals_span.range()], "=");
    assert_eq!(&let_query[pipe_span.range()], "|");
}

/// `as` binds the alternative operand; `let` binds the comma. Sibling
/// `binding_nodes_distinguish_as_from_let_and_retain_separators` pins the form and punctuation; this pins the value
/// tree.
#[test]
fn as_binds_the_alternative_let_binds_the_comma() {
    let as_query = parse_query(source(), "1, 2 as $x | $x").into_valid_syntax().unwrap();
    let ExprKind::Binary(comma) = as_query.kind() else {
        panic!("`1, 2 as $x` must be a comma whose right operand is the binding");
    };
    assert_eq!(comma.op, BinaryOp::Comma);
    assert!(matches!(comma.left.kind(), ExprKind::Number));
    let ExprKind::Binding(binding) = comma.right.kind() else {
        panic!("right of the comma must be the `as` binding");
    };
    assert!(matches!(binding.form, BindingForm::As { .. }));
    assert!(matches!(binding.value.kind(), ExprKind::Number));

    let let_query = parse_query(source(), "let $x = 1, 2 | $x").into_valid_syntax().unwrap();
    let ExprKind::Binding(binding) = let_query.kind() else {
        panic!("`let` must be the root");
    };
    assert!(matches!(binding.form, BindingForm::Let { .. }));
    let ExprKind::Binary(comma) = binding.value.kind() else {
        panic!("`let` value must be the comma");
    };
    assert_eq!(comma.op, BinaryOp::Comma);
}

#[test]
fn collection_and_control_nodes_retain_punctuation() {
    let query = r"if .ok then try [(.value), {name: .name,}] catch [] else reduce .[] as $x (0; . + $x) end";
    let expression = parse_query(source(), query).into_valid_syntax().unwrap();
    let ExprKind::If(conditional) = expression.kind() else {
        panic!("conditional");
    };
    assert_eq!(&query[conditional.branches[0].keyword_span.range()], "if");
    assert_eq!(&query[conditional.branches[0].then_keyword_span.range()], "then");
    assert_eq!(&query[conditional.else_keyword_span.unwrap().range()], "else");
    assert_eq!(&query[conditional.end_keyword_span.range()], "end");

    let ExprKind::Try(try_expr) = conditional.branches[0].then_branch.kind() else {
        panic!("try");
    };
    assert_eq!(&query[try_expr.try_keyword_span.range()], "try");
    assert_eq!(&query[try_expr.catch_keyword_span.unwrap().range()], "catch");
    let ExprKind::Array {
        expression: Some(group),
        open_span,
        close_span,
    } = try_expr.expr.kind()
    else {
        panic!("array");
    };
    assert_eq!(&query[open_span.range()], "[");
    assert_eq!(&query[close_span.range()], "]");
    let ExprKind::Binary(comma) = group.kind() else {
        panic!("array generator comma");
    };
    let ExprKind::Group {
        open_span, close_span, ..
    } = comma.left.kind()
    else {
        panic!("group");
    };
    assert_eq!(&query[open_span.range()], "(");
    assert_eq!(&query[close_span.range()], ")");
    let ExprKind::Object { members, .. } = comma.right.kind() else {
        panic!("object");
    };
    assert_eq!(&query[members[0].separator_span.unwrap().range()], ",");

    let reduce = conditional.else_branch.as_deref().unwrap();
    let ExprKind::Reduce(loop_expr) = reduce.kind() else {
        panic!("reduce");
    };
    assert_eq!(&query[loop_expr.keyword_span.range()], "reduce");
    assert_eq!(&query[loop_expr.as_keyword_span.range()], "as");
    assert_eq!(&query[loop_expr.open_span.range()], "(");
    assert_eq!(&query[loop_expr.update_separator_span.range()], ";");
    assert_eq!(&query[loop_expr.close_span.range()], ")");
}
