use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    collections::BTreeSet,
    hint::black_box,
};

use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{Expr, SyntaxNodeKind, SyntaxNodeRef, SyntaxWalk, WalkEvent, parse_program, parse_query};

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct TestAllocator;

#[global_allocator]
static ALLOCATOR: TestAllocator = TestAllocator;

unsafe impl GlobalAlloc for TestAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: this test allocator delegates the caller-provided layout unchanged.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            MEASURING.with(|measuring| {
                if measuring.get() {
                    ALLOCATIONS.with(|allocations| allocations.set(allocations.get() + 1));
                }
            });
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: this test allocator delegates the original pointer and layout unchanged.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: this test allocator delegates the caller-provided layout unchanged.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            MEASURING.with(|measuring| {
                if measuring.get() {
                    ALLOCATIONS.with(|allocations| allocations.set(allocations.get() + 1));
                }
            });
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: this test allocator delegates all caller-provided values unchanged.
        let pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !pointer.is_null() {
            MEASURING.with(|measuring| {
                if measuring.get() {
                    ALLOCATIONS.with(|allocations| allocations.set(allocations.get() + 1));
                }
            });
        }
        pointer
    }
}

fn source() -> SourceRef {
    SourceRef::new(SourceId::new(37), SourceKind::Query)
}

fn query(text: &str) -> Expr {
    parse_query(source(), text)
        .unwrap()
        .into_valid_syntax()
        .unwrap_or_else(|diagnostics| panic!("invalid fixture {text:?}: {diagnostics:?}"))
        .into_root()
}

fn allocation_calls(operation: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|allocations| allocations.set(0));
    MEASURING.with(|measuring| measuring.set(true));
    operation();
    MEASURING.with(|measuring| measuring.set(false));
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn closed_inventory_is_exhaustively_reachable() {
    assert_eq!(
        SyntaxNodeKind::ALL.iter().copied().collect::<BTreeSet<_>>().len(),
        SyntaxNodeKind::ALL.len(),
        "closed syntax inventory contains duplicate kinds"
    );
    let programs = [
        r#"module {name: "inventory"};
           import "support-\(.)" as support {search: "."};
           include "strings-\(.)";
           def top($x; f): if $x then [$x] else {value: $x} end;
           top(.)"#,
        r#"empty, null, true, false, 1, "text", $value, @json, @json "value=\(.)""#,
        r#"(([-.])), {name: ., "string": ., $variable: ., (.key): .}"#,
        r". + . | . = .",
        r"1 | def local($x): $x; local(.)",
        r"if . then . elif . then . else . end",
        r"try . catch .",
        r"reduce .[] as $item (0; . + $item)",
        r"foreach .[] as {$item} (0; . + 1; .)",
        r".[] as [$first, {$second, key: $third}] ?// $fallback | .",
        r"let $bound = . | $bound",
        r"label $done | if . then break $done else . end",
        r"~generator(0; .+1; .) as ~x | [~x.next, ~x.rest]",
        r#".field."quoted-\(.)"[0][][1:2].@tag.@["comment"].@(.).&href.&["aria"].&(.key)?"#,
        r"..?",
        r". + ",
        r". as invalid_pattern | .",
    ];
    let mut visited = BTreeSet::new();

    for program in programs {
        let parsed = parse_program(source(), program).unwrap();
        let unit = parsed.syntax().unwrap_or_else(|| {
            panic!(
                "inventory input produced no root: {program:?}: {:?}",
                parsed.diagnostics()
            )
        });
        for event in SyntaxWalk::source_unit(unit) {
            if let WalkEvent::Enter(node) = event {
                visited.insert(node.kind());
            }
        }
    }

    let missing: Vec<_> = SyntaxNodeKind::ALL
        .iter()
        .copied()
        .filter(|kind| !visited.contains(kind))
        .collect();
    assert!(missing.is_empty(), "unvisited syntax node kinds: {missing:?}");
}

#[test]
fn query_and_source_unit_walks_are_balanced_and_depth_first() {
    let expression = query("f(.a; [.b])");
    let events: Vec<_> = SyntaxWalk::query(&expression).collect();
    let enters = events
        .iter()
        .filter(|event| matches!(event, WalkEvent::Enter(_)))
        .count();
    let exits = events
        .iter()
        .filter(|event| matches!(event, WalkEvent::Exit(_)))
        .count();
    assert_eq!(enters, exits);
    assert!(matches!(
        events.first(),
        Some(WalkEvent::Enter(node)) if node.kind() == SyntaxNodeKind::Call
    ));
    assert!(matches!(
        events.last(),
        Some(WalkEvent::Exit(node)) if node.kind() == SyntaxNodeKind::Call
    ));

    let unit = parse_program(source(), "def id($x): $x; id(.)")
        .unwrap()
        .into_valid_syntax()
        .unwrap();
    let mut depth = 0_usize;
    for event in SyntaxWalk::source_unit(&unit) {
        match event {
            WalkEvent::Enter(_) => depth += 1,
            WalkEvent::Exit(_) => {
                assert!(depth > 0);
                depth -= 1;
            }
        }
    }
    assert_eq!(depth, 0);
}

#[test]
fn immediate_children_follow_authored_binding_order_without_allocating() {
    let as_binding = query(".a as [$x] | f(.b; .c)");
    let let_binding = query("let [$x] = .a | f(.b; .c)");

    let as_kinds: Vec<_> = SyntaxNodeRef::query(&as_binding)
        .children()
        .map(SyntaxNodeRef::kind)
        .collect();
    assert_eq!(
        as_kinds,
        [
            SyntaxNodeKind::Postfix,
            SyntaxNodeKind::PatternArray,
            SyntaxNodeKind::Call,
        ]
    );
    let let_kinds: Vec<_> = SyntaxNodeRef::query(&let_binding)
        .children()
        .map(SyntaxNodeRef::kind)
        .collect();
    assert_eq!(
        let_kinds,
        [
            SyntaxNodeKind::PatternArray,
            SyntaxNodeKind::Postfix,
            SyntaxNodeKind::Call,
        ]
    );

    assert_eq!(
        allocation_calls(|| {
            for child in SyntaxNodeRef::query(&let_binding).children() {
                black_box(child.kind());
                for span in child.source_spans() {
                    black_box(span);
                }
            }
        }),
        0
    );
}

#[test]
fn accessor_steps_and_selectors_preserve_every_authored_form_and_suffix() {
    let text = r#".@tag?.@["comment"]?.@(.key)?.&href?.&["aria-label"]?.&(.name)?"#;
    let expression = query(text);
    let kinds: Vec<_> = SyntaxWalk::query(&expression)
        .filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node.kind()),
            WalkEvent::Exit(_) => None,
        })
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|kind| **kind == SyntaxNodeKind::NodeAccessor)
            .count(),
        3
    );
    assert_eq!(
        kinds.iter().filter(|kind| **kind == SyntaxNodeKind::Attribute).count(),
        3
    );
    for selector in [
        SyntaxNodeKind::DirectSelector,
        SyntaxNodeKind::BracketSelector,
        SyntaxNodeKind::DynamicSelector,
    ] {
        assert_eq!(
            kinds.iter().filter(|kind| **kind == selector).count(),
            2,
            "{selector:?}"
        );
    }

    let steps: Vec<_> = SyntaxWalk::query(&expression)
        .filter_map(|event| match event {
            WalkEvent::Enter(node)
                if matches!(node.kind(), SyntaxNodeKind::NodeAccessor | SyntaxNodeKind::Attribute) =>
            {
                Some(node)
            }
            _ => None,
        })
        .collect();
    assert_eq!(steps.len(), 6);
    for step in steps {
        let spans: Vec<_> = step.source_spans().collect();
        assert!(
            spans
                .iter()
                .any(|span| &text[span.range()] == ".@" || &text[span.range()] == ".&")
        );
        assert!(spans.iter().any(|span| &text[span.range()] == "?"));
    }
}

#[test]
fn direct_source_spans_include_authored_punctuation_and_recovery_insertions() {
    let text = "if . then f(.a; .b) else . end";
    let expression = query(text);
    let punctuation: Vec<_> = SyntaxWalk::query(&expression)
        .filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node),
            WalkEvent::Exit(_) => None,
        })
        .flat_map(SyntaxNodeRef::source_spans)
        .map(|span| &text[span.range()])
        .collect();
    for expected in ["if", "then", "(", ";", ")", "else", "end"] {
        assert!(punctuation.contains(&expected), "missing {expected:?}");
    }

    let pattern_text = ". as [$x, {key: $y}] ?// $z | {a: ., b}";
    let pattern = query(pattern_text);
    let pattern_punctuation: Vec<_> = SyntaxWalk::query(&pattern)
        .filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node),
            WalkEvent::Exit(_) => None,
        })
        .flat_map(SyntaxNodeRef::source_spans)
        .map(|span| &pattern_text[span.range()])
        .collect();
    for expected in ["[", ",", "{", ":", "}", "]", "?//", "|"] {
        assert!(
            pattern_punctuation.contains(&expected),
            "missing pattern punctuation {expected:?}"
        );
    }
    assert_eq!(pattern_punctuation.iter().filter(|text| **text == ":").count(), 2);

    let recovered = parse_query(source(), "(.").unwrap();
    assert!(!recovered.is_valid());
    let root = recovered.syntax().unwrap();
    let zero_width: Vec<_> = SyntaxWalk::query(root)
        .filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node),
            WalkEvent::Exit(_) => None,
        })
        .flat_map(SyntaxNodeRef::source_spans)
        .filter(|span| span.is_empty())
        .collect();
    assert_eq!(zero_width.len(), 1);
    assert_eq!(zero_width[0].start(), 2);

    for nested_source in [
        r#""outer=\("inner=\(.value.@tag)")""#,
        ". as [$x, {$y}] | .",
        "{a: ., b: .}",
    ] {
        let nested = query(nested_source);
        for event in SyntaxWalk::query(&nested) {
            let WalkEvent::Enter(node) = event else {
                continue;
            };
            for owned in node.source_spans() {
                assert!(
                    node.span().start() <= owned.start() && owned.end() <= node.span().end(),
                    "{:?} span {} does not contain {}",
                    node.kind(),
                    node.span(),
                    owned
                );
            }
        }
    }
}

/// Every authored form of a program is one entered node, and only one.
fn entered_nodes(expression: &Expr) -> usize {
    SyntaxWalk::query(expression)
        .filter(|event| matches!(event, WalkEvent::Enter(_)))
        .count()
}

#[test]
fn the_walk_enters_every_authored_node_exactly_once() {
    assert_eq!(entered_nodes(&query("f(.a; {x: $x})")), 11);
    assert_eq!(entered_nodes(&query(". as {$x, key: [$y]} | .")), 10);
}

/// The walk is iterative, so it costs no call stack at any depth the grammar admits.
///
/// The fixture is the deepest program the nesting ceiling accepts. The parse that builds it and the `Drop` that frees
/// it both recurse, so the whole thing runs on a thread sized for them — the walk between the two is the part under
/// test, and it carries its own stack.
#[test]
fn maximum_audited_nesting_walks_iteratively() {
    let depth = jqf_syntax::MAX_SYNTAX_NESTING_DEPTH as usize - 1;
    let entered = std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(move || {
            let text = format!("{}.{}", "[".repeat(depth), "]".repeat(depth));
            let parsed = parse_query(source(), &text).unwrap().into_valid_syntax().unwrap();
            entered_nodes(&parsed)
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(entered, depth + 1, "one array per bracket pair, plus the leaf");
}

/// Collects every source span owned by any node of `text`, in walk order.
fn owned_span_inventory(text: &str) -> Vec<jqf_source::Span> {
    let expression = query(text);
    SyntaxWalk::query(&expression)
        .filter_map(|event| match event {
            WalkEvent::Enter(node) => Some(node),
            WalkEvent::Exit(_) => None,
        })
        .flat_map(SyntaxNodeRef::source_spans)
        .collect()
}

/// A dot may introduce a bracketed step, and then the step owns two tokens.
///
/// The load-bearing byte is the second `.` of `.a.[1]`: it ends no field step and starts no bracket, so unless the
/// bracketed step reports it, no node in the tree accounts for it and a source-preserving consumer drops it. The
/// once-only half of the same law is `.[1]`, where the step's operator IS the opening bracket and reporting both would
/// double-count one authored token.
#[test]
fn a_dot_introducing_a_bracket_step_is_owned_exactly_once() {
    let introduced = ".a.[1]";
    let inventory = owned_span_inventory(introduced);
    let mut owned = vec![false; introduced.len()];
    for span in &inventory {
        for byte in span.range() {
            owned[byte] = true;
        }
    }
    let unowned: Vec<_> = owned
        .iter()
        .enumerate()
        .filter_map(|(offset, owned)| (!owned).then_some(offset))
        .collect();
    assert!(
        unowned.is_empty(),
        "{introduced:?} leaves bytes {unowned:?} owned by no node"
    );
    assert_eq!(
        inventory
            .iter()
            .filter(|span| span.start() == 2 && span.end() == 3)
            .count(),
        1,
        "the introducing dot of {introduced:?} is reported once"
    );

    let plain = ".[1]";
    assert_eq!(
        owned_span_inventory(plain)
            .iter()
            .filter(|span| span.start() == 1 && span.end() == 2)
            .count(),
        1,
        "the opening bracket of {plain:?} is its own operator and is reported once"
    );
}

/// Offsets of `text` that more than one node reports as its own.
fn doubly_owned_bytes(text: &str, inventory: &[jqf_source::Span]) -> Vec<usize> {
    let mut owners = vec![0_usize; text.len()];
    for span in inventory {
        for byte in span.range() {
            owners[byte] += 1;
        }
    }
    owners
        .iter()
        .enumerate()
        .filter_map(|(offset, count)| (*count > 1).then_some(offset))
        .collect()
}

/// The dot of a root postfix form is the step's operator, so the chain's identity base is implied and owns nothing.
///
/// `.[1]` states the same law from the other side: there the dot IS the identity and the bracket is the step's
/// operator. Either way the dot has one owner, and every authored byte of these forms still has one.
#[test]
fn the_dot_of_a_root_postfix_form_is_owned_exactly_once() {
    for text in [".a", ".@x", ".&x", ".a.b", ".[1]", ".a.[1]", r#"."q""#] {
        let inventory = owned_span_inventory(text);
        assert!(
            doubly_owned_bytes(text, &inventory).is_empty(),
            "{text:?} reports bytes {:?} twice",
            doubly_owned_bytes(text, &inventory)
        );
        let mut owned = vec![false; text.len()];
        for span in &inventory {
            for byte in span.range() {
                owned[byte] = true;
            }
        }
        assert!(
            owned.iter().all(|owned| *owned),
            "{text:?} leaves an authored byte owned by no node"
        );
    }
}

/// Implied identity reports no owned source span; spelled `.` reports the dot.
#[test]
fn implied_identity_owns_no_source_span() {
    let expression = query(".a");
    let identities: Vec<_> = SyntaxWalk::query(&expression)
        .filter_map(|event| match event {
            WalkEvent::Enter(node) if node.kind() == SyntaxNodeKind::Identity => Some(node),
            _ => None,
        })
        .collect();
    assert_eq!(identities.len(), 1);
    assert!(identities[0].span().is_empty());
    assert_eq!(identities[0].source_spans().count(), 0);

    let spelled = query(".");
    let spelled_identities: Vec<_> = SyntaxWalk::query(&spelled)
        .filter_map(|event| match event {
            WalkEvent::Enter(node) if node.kind() == SyntaxNodeKind::Identity => Some(node),
            _ => None,
        })
        .collect();
    assert_eq!(spelled_identities.len(), 1);
    let spans: Vec<_> = spelled_identities[0].source_spans().collect();
    assert_eq!(spans.len(), 1);
    assert_eq!(&"."[spans[0].range()], ".");
}

/// Recovery nodes span only what they consumed, so a token the caller still owns is never reported twice.
///
/// Each fixture leaves the parser holding a token it did not consume: the `+` after `.a.`, the closer of an
/// unterminated form, the caller's separator. A node that merged such a token into its own span would report it
/// alongside the form that really owns it.
#[test]
fn recovery_nodes_never_report_a_token_their_caller_owns() {
    for text in [
        ".a.+",
        "[.a.]",
        "{a:.b.}",
        "f(.a.;1)",
        "(.a.)",
        ".@[,1]",
        "[.@[,1]",
        "f(~)",
        "f(~;.ok)",
        ". as {]}|.",
        "{,a:1}",
        "reduce .[] as $x (0;.;1)",
    ] {
        let parsed = parse_query(source(), text).unwrap();
        let root = parsed.syntax().expect("recovery keeps a root").root();
        let inventory: Vec<_> = SyntaxWalk::query(root)
            .filter_map(|event| match event {
                WalkEvent::Enter(node) => Some(node),
                WalkEvent::Exit(_) => None,
            })
            .flat_map(SyntaxNodeRef::source_spans)
            .collect();
        assert!(
            doubly_owned_bytes(text, &inventory).is_empty(),
            "{text:?} reports bytes {:?} twice",
            doubly_owned_bytes(text, &inventory)
        );
    }
}

/// A recovery node stays inside the form that holds it.
///
/// The fixtures end an inner form on a token its caller owns: an object-pattern member that found no key, a bracket
/// accessor that found no string. A member span reaching past its list's closer would put a child outside its parent.
#[test]
fn recovery_child_spans_stay_inside_their_parent() {
    for text in [". as {]}|.", ". as [{]}]|.", ".@[,1]", "f(.a.;1)", "{a:.b.}"] {
        let parsed = parse_query(source(), text).unwrap();
        let root = parsed.syntax().expect("recovery keeps a root").root();
        for event in SyntaxWalk::query(root) {
            let WalkEvent::Enter(node) = event else {
                continue;
            };
            for child in node.children() {
                assert!(
                    node.span().start() <= child.span().start() && child.span().end() <= node.span().end(),
                    "{text:?}: {:?} span {} escapes {:?} span {}",
                    child.kind(),
                    child.span(),
                    node.kind(),
                    node.span()
                );
            }
        }
    }
}
