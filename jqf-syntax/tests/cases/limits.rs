use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{SyntaxWalk, parse_program, parse_query};

fn source() -> SourceRef {
    SourceRef::new(SourceId::new(29), SourceKind::Query)
}

#[test]
fn parsing_accepts_the_ceiling_and_refuses_one_level_past_it() {
    // The deepest valid program reaches exactly `MAX_SYNTAX_NESTING_DEPTH`: the leaf expression is one level past the
    // `depth - 1` groups that contain it. The refusal is the level past that. The grammar recurses per level, so the
    // check runs on a thread with the stack it needs.
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(|| {
            let depth = jqf_syntax::MAX_SYNTAX_NESTING_DEPTH as usize;

            let at_ceiling = format!("{} .{}", "(".repeat(depth - 1), ")".repeat(depth - 1));
            let parsed = parse_query(source(), &at_ceiling).unwrap();
            assert!(
                parsed.is_valid(),
                "the deepest valid nesting must parse: {:?}",
                parsed.diagnostics()
            );

            let past_ceiling = format!("{} .{}", "(".repeat(depth), ")".repeat(depth));
            let parsed = parse_query(source(), &past_ceiling).unwrap();
            assert!(!parsed.is_valid());
            let codes: Vec<_> = parsed
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.code().to_string())
                .collect();
            assert_eq!(codes, vec!["syntax.nesting-too-deep"]);
            assert_eq!(
                parsed.diagnostics()[0].message(),
                format!(
                    "nesting depth limit exceeded: the ceiling is {} levels",
                    jqf_syntax::MAX_SYNTAX_NESTING_DEPTH
                )
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

/// The nesting refusal is the only diagnostic when it happens inside an interpolation, not just at the top level.
///
/// An interpolated body is parsed by its own parser on the same call stack, inheriting the shared depth — so its
/// refusal IS this parse's refusal. The load-bearing part of the fixture is the SECOND interpolation: it is
/// independently malformed, and unless the outer parse adopts the first one's refusal, that second body's diagnostics
/// append behind the ceiling error and it stops being the only thing the caller reports. The twin above pins the same
/// law for the direct form.
#[test]
fn a_nesting_refusal_inside_an_interpolation_is_still_the_only_diagnostic() {
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(|| {
            let depth = jqf_syntax::MAX_SYNTAX_NESTING_DEPTH as usize;
            let interpolated = format!(r#""\({} .{})\(*)""#, "(".repeat(depth), ")".repeat(depth));
            let parsed = parse_query(source(), &interpolated).unwrap();
            assert!(!parsed.is_valid());
            let codes: Vec<_> = parsed
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.code().to_string())
                .collect();
            assert_eq!(codes, vec!["syntax.nesting-too-deep"]);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// Pins the charge/release law on the parser nesting entry: one level per chain link, released with the subtree, so two
/// 3/4-ceiling chains around `|` both parse.
#[test]
fn an_operator_chain_charges_one_level_per_link_and_releases_them_together() {
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(|| {
            let ceiling = jqf_syntax::MAX_SYNTAX_NESTING_DEPTH as usize;
            let chain = |links: usize| format!(".{}", " + .".repeat(links));

            let at_ceiling = chain(ceiling - 1);
            let parsed = parse_query(source(), &at_ceiling).unwrap();
            assert!(
                parsed.is_valid(),
                "the deepest chain must parse: {:?}",
                parsed.diagnostics()
            );

            let past_ceiling = chain(ceiling);
            let parsed = parse_query(source(), &past_ceiling).unwrap();
            let codes: Vec<_> = parsed
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.code().to_string())
                .collect();
            assert_eq!(codes, vec!["syntax.nesting-too-deep"]);

            // Half again the ceiling in links across the two halves, with neither half anywhere near the ceiling on its
            // own.
            let half = chain(ceiling * 3 / 4);
            let released = format!("{half} | {half}");
            let parsed = parse_query(source(), &released).unwrap();
            assert!(
                parsed.is_valid(),
                "a completed chain must release its links: {:?}",
                parsed.diagnostics()
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

/// `label` / `as` / query-`def` spines charge one level per link and release the chain when it completes, so siblings
/// do not inherit leftover depth.
#[test]
fn query_head_spines_charge_and_release_like_operator_chains() {
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(|| {
            let ceiling = jqf_syntax::MAX_SYNTAX_NESTING_DEPTH as usize;

            let label_at = format!("{} .", "label $x | ".repeat(ceiling - 1));
            let parsed = parse_query(source(), &label_at).unwrap();
            assert!(
                parsed.is_valid(),
                "the deepest label spine must parse: {:?}",
                parsed.diagnostics()
            );
            let label_past = format!("{} .", "label $x | ".repeat(ceiling));
            let parsed = parse_query(source(), &label_past).unwrap();
            let codes: Vec<_> = parsed
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.code().to_string())
                .collect();
            assert_eq!(codes, vec!["syntax.nesting-too-deep"]);

            let as_at = format!("{} .", "1 as $x | ".repeat(ceiling - 1));
            let parsed = parse_query(source(), &as_at).unwrap();
            assert!(
                parsed.is_valid(),
                "the deepest as spine must parse: {:?}",
                parsed.diagnostics()
            );
            let as_past = format!("{} .", "1 as $x | ".repeat(ceiling));
            let parsed = parse_query(source(), &as_past).unwrap();
            let codes: Vec<_> = parsed
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.code().to_string())
                .collect();
            assert_eq!(codes, vec!["syntax.nesting-too-deep"]);

            let half = ceiling * 3 / 4;
            let args = (0..half).map(|_| "1 as $x | 1").collect::<Vec<_>>().join("; ");
            let siblings = format!("f({args})");
            let parsed = parse_query(source(), &siblings).unwrap();
            assert!(
                parsed.is_valid(),
                "completed as/def/label spines must release: {:?}",
                parsed.diagnostics()
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn audited_maximum_handles_long_iterative_operator_trees() {
    let mut query = String::from(".");
    for _ in 0..1_000 {
        query.push_str(" + .");
    }
    let parsed = parse_program(source(), &query).unwrap();
    assert!(parsed.is_valid());
    let visited = SyntaxWalk::source_unit(parsed.syntax().unwrap()).count();
    assert!(visited > 2_000);
}
