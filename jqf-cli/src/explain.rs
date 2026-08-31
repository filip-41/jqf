//! The `--explain` renderer: the plan facts, the route that served the request, the run's timing and cost, and the
//! recovered tree on a parse failure.
//!
//! `--explain` prints the PLAN block after compile — the routing facts the engine derived — and the ROUTE block after
//! the run — the rung the selector actually took, the wall-clock run time, and the request's cost snapshot. Every fact
//! is read through the same accessors the route selector reads (the engine's [`jqf_engine:ExplainPlan`], the retained
//! `ROUTE_SELECTED` record, the account's [`jqf_resource:UsageSnapshot`]), so the explain block cannot drift from the
//! route it describes.
//!
//! A program that fails to parse never reaches those blocks. [`render_parse_failure`] is the substitute: JSON-free
//! `jqf: explain:` lines naming the invalid parse and a linear recovered-tree outline. There is no run, so this path
//! prints no route, no counters, and no cost line. It is a diagnostic surface: it never changes stdout bytes.

use std::fmt::Write as _;
use std::time::Duration;

use jqf_engine::{BoundaryConsumer, ProjectionClass, ShortcutKind, StaticForwardStep};
use jqf_resource::ResourceContext;
use jqf_sdk::Diagnostics;
use jqf_source::{SourceId, SourceKind, SourceRef};
use jqf_syntax::{Parse, SourceUnit, SyntaxNodeKind, SyntaxWalk, WalkEvent, parse_program};

/// Renders the parse-failure explain block: the parse is invalid, then a linear recovered-tree outline of the node
/// kinds the parser kept.
///
/// The compiler's `into_valid_syntax` drops the recovery tree when it surfaces a parse rejection. Carrying that tree
/// through the shared compile-error type would grow every `Parse` arm; this path re-parses the same source as a program
/// unit (the compiler's one parse entry), then walks the public [`SyntaxWalk`] surface. A source the parser could not
/// recover a root for prints `recovered: none`.
#[must_use]
pub fn render_parse_failure(source: &str) -> Vec<String> {
    let source_ref = SourceRef::new(SourceId::new(0), SourceKind::Query);
    let recovered = match parse_program(source_ref, source) {
        Ok(parse) => outline_program(&parse),
        Err(_) => "none".to_owned(),
    };
    vec![
        "jqf: explain: parse: invalid".to_owned(),
        format!("jqf: explain: recovered: {recovered}"),
    ]
}

fn outline_program(parse: &Parse<SourceUnit>) -> String {
    parse.syntax().map_or_else(
        || "none".to_owned(),
        |syntax| outline_nodes(SyntaxWalk::source_unit(syntax.root())),
    )
}

/// Enter-only walk: node kinds in visit order, with byte spans on recovery holes so later surviving members stay
/// visible after an `Error`.
fn outline_nodes(walk: SyntaxWalk<'_>) -> String {
    let mut parts = Vec::new();
    for event in walk {
        let WalkEvent::Enter(node) = event else {
            continue;
        };
        let kind = node.kind();
        if matches!(kind, SyntaxNodeKind::Error | SyntaxNodeKind::PatternError) {
            let span = node.span();
            parts.push(format!("{kind:?}@{}..{}", span.start(), span.end()));
        } else {
            parts.push(format!("{kind:?}"));
        }
    }
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join(" ")
    }
}

/// Renders the post-compile plan block, one `jqf: explain:` line per fact.
#[must_use]
pub fn render_plan(source: &str, compiled: &jqf_engine::CompiledProgram, compile_time: Duration) -> Vec<String> {
    let plan = compiled.explain();
    let mut lines = Vec::new();
    lines.push(format!("jqf: explain: program: {source}"));
    lines.push(format!(
        "jqf: explain: class: identity={} modifies={} whole_document={} input_family={} morsel_static={}",
        yes(plan.shortcut == ShortcutKind::Identity),
        yes(plan.modifies),
        yes(plan.consumes_whole_document),
        yes(plan.uses_input_family),
        yes(plan.morsel_static_path),
    ));
    lines.push(format!(
        "jqf: explain: demand: class={} boundary={}",
        render_class(plan.projection_class.clone()),
        plan.boundary_consumer.map_or("none", render_consumer)
    ));
    lines.push(format!(
        "jqf: explain: shortcut: {} inputs_cursor={}",
        plan.shortcut.as_str(),
        yes(plan.uses_inputs_cursor),
    ));
    lines.push(format!("jqf: explain: pushdown: {}", render_path(&plan.pushdown)));
    lines.push(format!(
        "jqf: explain: ladder: morsel={} range_locate={}",
        yes(plan.rungs.morsel),
        yes(plan.shortcut == ShortcutKind::RangeLocate),
    ));
    lines.push(format!("jqf: explain: topk: rows={}", plan.topk_rows));
    lines.push(format!("jqf: explain: compile_time: {}", render_duration(compile_time)));
    lines
}

/// Renders the post-run route block: the rung the selector took, the run time, and the request's cost snapshot.
#[must_use]
pub fn render_route(
    diagnostics: Option<&Diagnostics>,
    resources: &ResourceContext<'_>,
    run_time: Duration,
) -> Vec<String> {
    let route = last_route(diagnostics).unwrap_or_else(|| "?".to_owned());
    // The same snapshot the diagnostics cost record reads: one request account, shared into ambient when the CLI is
    // metering.
    let snapshot = crate::reported_cost_snapshot(resources);
    vec![
        format!("jqf: explain: route: {route}"),
        format!("jqf: explain: run_time: {}", render_duration(run_time)),
        format!(
            "jqf: explain: cost: peak={} input={} output={} spill_disk={}",
            snapshot.memory_peak_bytes(),
            snapshot.input_bytes(),
            snapshot.output_bytes(),
            snapshot.spill_disk_bytes(),
        ),
        // The lazy document's activity: how many container spans the codec deferred (W3-T1's default defers containers
        // below the frontier) and how many the run materialized on demand. Both ride the request context, set by the
        // decode drives; a run that never decoded a lazy document reads zeros, which is the honest answer.
        format!(
            "jqf: explain: lazy: deferred={} materialized={}",
            resources.lazy_deferred_spans(),
            resources.lazy_materialized_spans(),
        ),
    ]
}

/// The last recorded `ROUTE_SELECTED` operand — the rung the selector actually served the request with.
fn last_route(diagnostics: Option<&Diagnostics>) -> Option<String> {
    diagnostics?
        .records()
        .into_iter()
        .rev()
        .find(|record| record.code == jqf_resource::diag::codes::ROUTE_SELECTED)
        .and_then(|record| record.operand().map(str::to_owned))
}

/// Renders a static pushdown path in jq postfix spelling; an empty path is the root selection.
fn render_path(path: &[StaticForwardStep<'_>]) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let mut out = String::new();
    for step in path {
        match step {
            StaticForwardStep::ObjectKey(key) => {
                out.push('.');
                out.push_str(key);
            }
            StaticForwardStep::ArrayIndex(index) => {
                let _ = write!(out, "[{index}]");
            }
            StaticForwardStep::ArrayRange { start, end } => {
                out.push('[');
                if let Some(start) = start {
                    out.push_str(&start.to_string());
                }
                out.push(':');
                if let Some(end) = end {
                    out.push_str(&end.to_string());
                }
                out.push(']');
            }
        }
    }
    out
}

/// Renders the per-element demand class, with its projected field set.
fn render_class(class: ProjectionClass<'_>) -> String {
    match class {
        ProjectionClass::Structure => "Structure".to_owned(),
        ProjectionClass::Fields(fields) => format!("Fields({})", fields.names().join(",")),
        ProjectionClass::Subtree => "Subtree".to_owned(),
    }
}

/// Renders one element-boundary consumer.
const fn render_consumer(consumer: BoundaryConsumer) -> &'static str {
    match consumer {
        BoundaryConsumer::Residual => "residual",
        BoundaryConsumer::Fold => "fold",
        BoundaryConsumer::Binding => "binding",
        BoundaryConsumer::Collect => "collect",
    }
}

/// Renders a duration at the finest unit that has at least one whole value.
fn render_duration(elapsed: Duration) -> String {
    if elapsed.as_secs() >= 1 {
        format!("{:.3}s", elapsed.as_secs_f64())
    } else if elapsed.as_millis() >= 1 {
        format!("{:.2}ms", elapsed.as_secs_f64() * 1000.0)
    } else {
        format!("{}us", elapsed.as_micros())
    }
}

/// The yes/no spelling the ladder rows use.
const fn yes(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
