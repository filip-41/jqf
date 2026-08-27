//! HTML demand-ladder differential: every pushed-down demand route against the
//! whole-document floor.
//!
//! The property is the one the codec's own laws state and nothing executed:
//! **for every document and every exact path, the answer a pushed-down demand
//! route publishes must equal the answer the whole-document floor computes.**
//! One route is driven per row — located — and its answer is compared
//! against an independent walk of the recovered document
//! (`floor.rs`, which shares no code with the codec's `locate` module).
//!
//! The DECLARED table is this lane's divergence register, in the shape the XML
//! differential established: a row states a document/path/route where the two
//! sides are EXPECTED to disagree, with the reason. A declared row that stops
//! disagreeing is STALE and fails the run; a disagreement that is not on the
//! table is a defect.
//!
//! The [`OPEN_DEFECTS`] table is a SECOND and deliberately different register. A
//! declared row says "these two answers legitimately differ"; an open-defect row
//! says "this route publishes no answer at all, because it is broken today".
//! Nothing is waived by it: the failure is printed on every run with the owning
//! file and the fix, the row only absorbs the exact contract violation it names,
//! and a row that stops firing is STALE and fails the run — so the table empties
//! itself the moment the codec is fixed.

mod corpus;
mod floor;
mod hash;
mod route;

use std::collections::BTreeMap;

use jqf_data::ValueKind;

/// One route's answer for one row, in the vocabulary both sides share.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Answer {
    /// A published value, by structural hash (see `hash.rs`).
    Value(u64),
    /// The path was absent at this zero-based step.
    Missing(usize),
    /// A step addressed a non-iterable category.
    Mismatch(usize, ValueKind),
    /// The route declined by design (`RequirementMismatch`): a range or
    /// plural-member hit is a stream, so the session refuses and the
    /// binder's whole-document floor serves the demand — the floor's
    /// answer IS the request's answer.
    Declined,
    /// The harness could not obtain an answer at all.
    Failed(String),
}

/// The per-path answer of one side.
pub(crate) struct RouteAnswers {
    pub(crate) located: Answer,
}

impl RouteAnswers {
    pub(crate) fn failed(reason: &str) -> Self {
        Self {
            located: Answer::Failed(reason.to_owned()),
        }
    }
}

/// One declared-split row: a document/path/route where the demand route and the
/// floor are EXPECTED to disagree, with the reason. Empty today — every row this
/// lane has found so far is a defect, and a defect is reported, never waived.
struct Declared {
    case: &'static str,
    path: &'static str,
    route: &'static str,
    reason: &'static str,
}

const DECLARED: &[Declared] = &[];

/// One open-defect row: a demand route that publishes NO answer today because
/// driving it raises a contract violation. The row absorbs exactly that
/// violation on exactly that route — a different failure, or a real value
/// divergence, still fails the run — and it must be DELETED when the defect is
/// fixed, which the stale check forces.
struct OpenDefect {
    route: &'static str,
    contract: &'static str,
    defect: &'static str,
    owner: &'static str,
}

/// No demand route is dead today; a route that fails when this lane drives it
/// must be reported as a finding, never waived.
const OPEN_DEFECTS: &[OpenDefect] = &[];

/// The tallies one run accumulates.
#[derive(Default)]
struct Run {
    /// Comparisons made, per route.
    counts: BTreeMap<&'static str, usize>,
    /// Comparisons absorbed by an open-defect row, per route.
    quarantined: BTreeMap<&'static str, usize>,
    divergences: Vec<String>,
    declared_tally: u64,
    stale: Vec<String>,
    comparisons: usize,
}

pub(crate) fn run() -> Result<(), String> {
    let mut run = Run::default();

    for case in corpus::CASES {
        for path in corpus::PATHS {
            let route = RouteAnswers {
                located: route::located(case.bytes, path.steps),
            };
            let reference = floor::answers(case.bytes, path.steps);
            run.compare(case.name, path.name, "located", &route.located, &reference.located);
        }
    }

    match run.report() {
        None => Ok(()),
        Some(message) => Err(message),
    }
}

impl Run {
    fn compare(&mut self, case: &'static str, path: &'static str, route: &'static str, left: &Answer, right: &Answer) {
        self.comparisons += 1;
        *self.counts.entry(route).or_default() += 1;

        if let Some(row) = DECLARED
            .iter()
            .find(|row| row.case == case && row.path == path && row.route == route)
        {
            if left == right {
                self.stale.push(format!(
                    "{case} {path} route={route}: declared as a divergence ({}) but the two sides agree — the row is STALE",
                    row.reason
                ));
            } else {
                self.declared_tally += 1;
                self.divergences.push(format!(
                    "DECLARED[{case} {path} route={route}] {}: route={left:?} floor={right:?}",
                    row.reason
                ));
            }
            return;
        }

        if left == right {
            return;
        }
        // A declined demand route is agreement by fallback: the binder
        // re-runs the whole-document floor, which is exactly `right` here.
        if matches!(left, Answer::Declined) {
            return;
        }
        // A quarantined route publishes no answer: the row absorbs exactly the
        // contract violation it names, and nothing else.
        if open_defect(route, left).is_some() {
            *self.quarantined.entry(route).or_default() += 1;
            return;
        }
        self.divergences
            .push(format!("{case} {path} route={route}: route={left:?} floor={right:?}"));
    }

    fn report(&self) -> Option<String> {
        println!("jqf-codec-html-differential: comparisons={}", self.comparisons);
        for (route, count) in &self.counts {
            println!("  route={route} comparisons={count}");
        }
        for divergence in &self.divergences {
            println!("  {divergence}");
        }
        for entry in &self.stale {
            println!("  STALE-DECLARATION {entry}");
        }

        let mut stale_defects = Vec::new();
        for row in OPEN_DEFECTS {
            let hits = self.quarantined.get(row.route).copied().unwrap_or_default();
            if hits == 0 {
                stale_defects.push(row.route);
                println!(
                    "  STALE-OPEN-DEFECT route={} contract={:?}: the route no longer fails — the row is fixed, DELETE it and let the comparisons stand",
                    row.route, row.contract
                );
                continue;
            }
            println!(
                "  OPEN-DEFECT route={} quarantined={hits}: {}\n    fix: {}",
                row.route, row.defect, row.owner
            );
        }

        let quarantined: usize = self.quarantined.values().sum();
        let undeclared = self
            .divergences
            .iter()
            .filter(|divergence| !divergence.starts_with("DECLARED["))
            .count();
        if undeclared == 0 && self.stale.is_empty() && stale_defects.is_empty() {
            println!(
                "jqf-codec-html-differential: PASS agreements={} declared={} undeclared_divergences=0 stale=0 open_defect_routes={} quarantined={quarantined}",
                self.comparisons - quarantined,
                self.declared_tally,
                OPEN_DEFECTS.len()
            );
            return None;
        }
        eprintln!(
            "jqf-codec-html-differential: FAIL divergences={} undeclared={undeclared} stale={} stale_open_defects={}",
            self.divergences.len(),
            self.stale.len(),
            stale_defects.len()
        );
        Some(format!(
            "divergences={} undeclared={undeclared} stale={} stale_open_defects={}",
            self.divergences.len(),
            self.stale.len(),
            stale_defects.len()
        ))
    }
}

/// The open-defect row that owns this failure, when the route is quarantined and
/// the failure is the exact contract violation the row names.
fn open_defect(route: &str, answer: &Answer) -> Option<&'static OpenDefect> {
    let Answer::Failed(reason) = answer else {
        return None;
    };
    OPEN_DEFECTS
        .iter()
        .find(|row| row.route == route && reason.contains(row.contract))
}
