//! Typed tree walks: the closed node inventory, child order, spans, and structural metrics.
//!
//! Run with `cargo run -p jqf-syntax --example traversal`.

use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{ExprKind, SyntaxNodeKind, SyntaxNodeRef, SyntaxWalk, WalkEvent, parse_query};

fn main() {
    let source = SourceRef::new(SourceId::new(1), SourceKind::Query);
    let text = ".price.@tag // \"untagged\"";
    let syntax = parse_query(source, text)
        .expect("example source fits compact syntax spans")
        .into_valid_syntax()
        .expect("example query is valid syntax");
    let expression = syntax.root();

    // The walk visits each node pre-order with balanced enter/exit events, and every visited kind is a member of the
    // closed inventory.
    let mut enters = 0;
    let mut exits = 0;
    let mut visited = Vec::new();
    for event in SyntaxWalk::query(expression) {
        match event {
            WalkEvent::Enter(node) => {
                enters += 1;
                visited.push(node.kind());
            }
            WalkEvent::Exit(_) => exits += 1,
        }
    }
    assert_eq!(enters, exits);
    for kind in &visited {
        assert!(SyntaxNodeKind::ALL.contains(kind), "{kind:?}");
    }
    // The alternative's left is the `.@` accessor postfix, its right the string literal.
    let ExprKind::Binary(_) = expression.kind() else {
        panic!("expected the alternative operator");
    };
    assert_eq!(
        visited,
        vec![
            SyntaxNodeKind::Binary,
            SyntaxNodeKind::Postfix,
            SyntaxNodeKind::Identity,
            SyntaxNodeKind::Field,
            SyntaxNodeKind::FieldNameSelector,
            SyntaxNodeKind::NodeAccessor,
            SyntaxNodeKind::DirectSelector,
            SyntaxNodeKind::String,
            SyntaxNodeKind::StringTemplate,
            SyntaxNodeKind::StringLiteralSegment,
        ]
    );
    // Children iterate in authored order without allocating; the base, the steps, and the selectors are separate walk
    // nodes, each owning its own direct source spans.
    let root = SyntaxNodeRef::query(expression);
    let child_kinds: Vec<_> = root.children().map(SyntaxNodeRef::kind).collect();
    assert_eq!(child_kinds, vec![SyntaxNodeKind::Postfix, SyntaxNodeKind::String]);
    let first_child = root.children().next().expect("two children");
    assert_eq!(&text[first_child.span().range()], ".price.@tag");

    let accessor = SyntaxWalk::query(expression)
        .find_map(|event| match event {
            WalkEvent::Enter(node) if node.kind() == SyntaxNodeKind::NodeAccessor => Some(node),
            _ => None,
        })
        .expect("one node accessor step");
    let spans: Vec<_> = accessor.source_spans().collect();
    assert!(
        spans.iter().any(|span| &text[span.range()] == ".@"),
        "the accessor step retains its introducer span: {spans:?}"
    );

    println!("walked {enters} nodes; the accessor introducer spans {spans:?}");
}
