//! Examples-as-tests: every registered builtin overload's documentation
//! examples are compiled and executed through the real pipeline, and their
//! published bytes are asserted against the recorded expected output.
//!
//! `jqf-engine` owns the registry but cannot depend on `jqf-sdk`, so the harness
//! lives here — the SDK is the first place the full compile-to-publish pipeline
//! (JSON codec + engine + publication) is available together, exactly the path
//! `jqf-cli` drives. It reuses the `execute_sequence` harness style from
//! `tests/execute_sequence.rs`: the same `CollectingSink`, `ResourceContext`,
//! and strict-JSON RFC 8259 route.
//!
//! At zero registered overloads this iterates nothing and passes vacuously —
//! the intended green. A future overload's example flows through with no harness
//! change: its `program` compiles via `jqf_engine::try_compile_program`, the
//! derived requirement runs against its `input` through `execute_sequence`, and
//! the published bytes are compared to its `expected` output.
//!
//! The second lane in this file is the ARGUMENT-DEGENERACY AUDIT, and it exists
//! because the first lane is not enough on its own. `037` S4 and `040` A1 both
//! shipped wrong answers past a full green example battery: `log/1` discarded
//! its base and `json_pointer` kept only the last of a multi-valued path, and
//! every registered example agreed with the broken law by accident. The audit
//! MUTATES each example's arguments and requires the mutation to be observable,
//! so a registered example that cannot distinguish an argument-ignoring
//! implementation is a failing test rather than a footnote.

/// A process-lifetime built-in dialect for request construction (123 X5).
fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use core::cell::{Cell, RefCell};
use core::ops::Range;

use jqf_codec_core::{DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{
    BuiltinExample, BuiltinExecution, BuiltinOverloadRecord, CodecRequirementPolicy, ParameterKind, builtin_overloads,
    try_compile_program,
};
use jqf_resource::{Control, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, PipelinePolicy, Request};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;

/// How many host control observations one audit run may make before it is
/// cancelled.
///
/// A mutated argument can turn a terminating fixpoint into a non-terminating
/// one (`until(. > 4; . + 1)` with the condition mutated to `false` never
/// stops), and the engine checks host control on exactly those rounds. The
/// budget converts a hang into an INCONCLUSIVE outcome the audit discards; no
/// registered example comes anywhere near it.
const CONTROL_BUDGET: u64 = 200_000;

/// A host control that cancels a run after [`CONTROL_BUDGET`] observations.
struct BudgetControl {
    remaining: Cell<u64>,
}

impl BudgetControl {
    fn new() -> Self {
        Self {
            remaining: Cell::new(CONTROL_BUDGET),
        }
    }

    /// Whether this run ended because the budget ran out, rather than on its
    /// own terms.
    fn exhausted(&self) -> bool {
        self.remaining.get() == 0
    }
}

impl Control for BudgetControl {
    fn check(&self) -> ControlOutcome {
        let remaining = self.remaining.get();
        if remaining == 0 {
            return ControlOutcome::Cancelled;
        }
        self.remaining.set(remaining - 1);
        ControlOutcome::Continue
    }
}

/// Collects every published byte, mirroring `tests/execute_sequence.rs`.
struct CollectingSink {
    bytes: Vec<u8>,
}

impl CollectingSink {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }
}

impl ItemSink for CollectingSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Collects every byte a program writes to the host's stderr channel.
///
/// `stderr/0` and `debug`/`debug/1` are OBSERVABLE effects with no stdout
/// signature at all — `debug`'s whole purpose is to pass its input through
/// unchanged — so a lane that watched only published bytes could not tell the
/// real `debug` from one that wrote nothing (037 S5).
struct CapturingStderr {
    bytes: RefCell<Vec<u8>>,
}

impl CapturingStderr {
    fn new() -> Self {
        Self {
            bytes: RefCell::new(Vec::new()),
        }
    }

    fn take(self) -> Vec<u8> {
        self.bytes.into_inner()
    }
}

impl jqf_resource::StderrSink for CapturingStderr {
    fn write_compact(&self, bytes: &[u8]) -> Result<(), jqf_resource::ResourceError> {
        self.bytes.borrow_mut().extend_from_slice(bytes);
        Ok(())
    }
}

fn resources<'host>(control: &'host BudgetControl, stderr: &'host CapturingStderr) -> ResourceContext<'host> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, 1 << 24, 64 << 20, 0, 128)).expect("account allocates"),
        control,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work meter starts"),
    )
    .expect("resources start")
    .with_stderr(stderr)
}

/// What one program run against one input did, as far as a caller can observe.
///
/// STDERR and the halt status are part of the observation, not decoration: they
/// are the only surface `debug`/`debug/1` and `halt_error/1` have, so an audit
/// that watched published bytes alone could not see whether those overloads
/// read their arguments at all.
#[derive(Debug, Eq, PartialEq)]
enum Outcome {
    /// The run completed, publishing these exact bytes and writing this exact
    /// diagnostic stream.
    Published { stdout: Vec<u8>, stderr: Vec<u8> },
    /// `halt`/`halt_error` terminated the run at this exit status, carrying
    /// this rendering of its message value.
    Halted {
        status: u32,
        message: String,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The run failed; the payload is the failure's own rendering, so two
    /// different failures are two different outcomes.
    Failed(String),
    /// The program did not compile, or the run outgrew the audit's budget.
    /// It is evidence of nothing and never counts as a distinguished outcome.
    Inconclusive,
}

impl Outcome {
    /// The published bytes, for the lane that asserts an example's recorded
    /// output. A halt publishes whatever it wrote before terminating.
    fn stdout(self) -> Option<Vec<u8>> {
        match self {
            Outcome::Published { stdout, .. } | Outcome::Halted { stdout, .. } => Some(stdout),
            Outcome::Failed(_) | Outcome::Inconclusive => None,
        }
    }
}

/// Compiles one program, runs it against one input over the strict-JSON RFC
/// 8259 route, and returns what an observer sees. The compiled program, its
/// derived requirement, and the execution all share one `ResourceContext`
/// account (the requirement authorizes exactly the execution it is charged
/// against).
fn run_program(program: &str, input: &str) -> Outcome {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let control = BudgetControl::new();
    let stderr = CapturingStderr::new();
    let mut resources = resources(&control, &stderr);
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);

    let Ok(compiled) = try_compile_program(program, policy, &resources) else {
        return Outcome::Inconclusive;
    };
    let Ok(requirement) = compiled.try_requirement(&resources) else {
        return Outcome::Inconclusive;
    };

    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "example.json",
        input.as_bytes(),
        0,
    );
    let mut sink = CollectingSink::new();
    let request = Request::new(&compiled, Input::Whole(input.as_bytes()))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(format(), dialect())
        .with_output_format(format(), dialect())
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: json_dialect(),
                options: None,
                allow_adjacent_values: true,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            encode_options: None,
            cooperative_credits: COOPERATIVE_CREDITS,
            split: None,

            max_iterations: None,
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    let result = jqf_sdk::execute(request, &mut sink);
    if control.exhausted() {
        return Outcome::Inconclusive;
    }
    drop(resources);
    let stderr = stderr.take();
    match result {
        Ok(_) => Outcome::Published {
            stdout: sink.bytes,
            stderr,
        },
        // `halt`/`halt_error` terminate the run at an exit status, with the
        // message the host would print — the process-level law their examples
        // pin, and the only thing that tells one `halt_error` status from
        // another.
        Err(error) => match error.pipeline_failure() {
            Some(jqf_sdk::PipelineFailure::Halt { status, message }) => Outcome::Halted {
                status: *status,
                message: format!("{message:?}"),
                stdout: sink.bytes,
                stderr,
            },
            failure => Outcome::Failed(format!("{failure:?}")),
        },
    }
}

/// Runs one example and returns the exact published bytes, panicking on any
/// outcome an example is not allowed to have.
fn run_example(example: &BuiltinExample) -> Vec<u8> {
    let outcome = run_program(example.program, example.input);
    let rendered = format!("{outcome:?}");
    outcome.stdout().unwrap_or_else(|| {
        panic!(
            "example program {:?} on input {:?} did not publish: {rendered}",
            example.program, example.input
        )
    })
}

/// Every registered overload's examples compile, execute, and publish exactly
/// their recorded expected bytes. Vacuously green while the inventory is empty;
/// a future example runs here with no change to this harness.
#[test]
fn registered_builtin_examples_match_expected_output() {
    for overload in builtin_overloads() {
        for example in overload.examples {
            let published = run_example(example);
            assert_eq!(
                published,
                example.expected.as_bytes(),
                "overload {} example {:?} on input {:?} published {:?}, expected {:?}",
                overload.canonical_name,
                example.program,
                example.input,
                String::from_utf8_lossy(&published),
                example.expected,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The argument-degeneracy audit
// ---------------------------------------------------------------------------

/// The stand-ins one argument position is replaced with, one at a time.
///
/// An implementation that READS argument `i` answers differently for at least
/// one of these; an implementation that evaluates and DISCARDS it answers
/// identically for all of them, which is the whole signal. They span every
/// value kind on purpose, so a position that only accepts one kind is still
/// distinguished — by the type error the wrong kinds raise.
const SUBSTITUTIONS: &[&str] = &["null", "0", "1", "\"jqf-argument-audit\"", "[]", "true"];

/// The obligation one waiver suspends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Obligation {
    /// No example distinguishes an implementation that ignores this argument
    /// position's VALUE.
    Position(u8),
    /// No example passes a MULTI-VALUED filter argument, so no example
    /// exercises the cardinality half of the reference's argument law at all.
    MultiValued,
    /// No example's program could be parsed back to this overload's argument
    /// list, so neither obligation could be checked at all.
    Unscannable,
}

/// One suspended obligation, its reason, and the exact condition that retires
/// it.
struct Waiver {
    name: &'static str,
    arity: u8,
    obligation: Obligation,
    reason: &'static str,
}

/// Overloads whose registered examples cannot yet distinguish an
/// argument-ignoring implementation.
///
/// This is a VISIBLE debt table, not a mute exclusion: every entry names the
/// reason and the exact retirement condition, and a STALE entry — one whose
/// obligation is now met — FAILS the lane, so a fix cannot leave its waiver
/// behind.
///
/// It is EMPTY, and that is the intended steady state: every registered
/// multi-argument overload can currently distinguish an argument-ignoring
/// implementation. A new overload whose examples cannot either fixes its
/// examples or lands here with a reason and a retirement condition.
const WAIVERS: &[Waiver] = &[
    Waiver {
        name: "declare_index",
        arity: 2,
        obligation: Obligation::Position(0),
        reason: "`declare_index` is a TRANSPARENT acceleration declaration: its output is always \
                 the input (byte-identity with `.`), and its CONTAINER argument is a \
                 pattern-matched static path that is NAVIGATED, never evaluated as a filter. \
                 An implementation that \"evaluates and discards\" the argument is \
                 indistinguishable BY DESIGN — and a mutated container path simply builds \
                 nothing and declines to the naive scan, byte-identically. The argument is \
                 observable only through the SPEED of a later probe, which published bytes \
                 cannot see. Retire when the surface changes so the declaration's output can \
                 depend on its arguments (a design change), or when the audit gains a \
                 mechanism for pattern-matched arguments.",
    },
    Waiver {
        name: "declare_index",
        arity: 2,
        obligation: Obligation::Position(1),
        reason: "same transparency law as Position(0): the KEY argument is a pattern-matched \
                 static path, never evaluated, and a mutated key path builds an index the \
                 probe never consults — the naive scan answers, byte-identically. Retire \
                 with Position(0): when the declaration's output can depend on its \
                 arguments, or the audit gains a pattern-matched-argument mechanism.",
    },
    Waiver {
        name: "declare_index",
        arity: 2,
        obligation: Obligation::MultiValued,
        reason: "the arguments are pattern-matched static paths, never evaluated: a \
                 multi-valued argument is a non-`Stage` node, so the declaration DECLINES \
                 (builds nothing, passes the input through) — exactly the single-output \
                 law's behavior, so no example can exercise the cardinality half of the reference's \
                 argument law. Retire when the surface changes so the declaration evaluates \
                 its arguments (a design change), or the audit gains a \
                 pattern-matched-argument mechanism.",
    },
];

/// The byte ranges of one call's top-level arguments inside `program`.
///
/// Finds the first `name(` at a token boundary whose argument list has exactly
/// `arity` top-level members. A `;` inside a nested call, a bracket, a string
/// literal, or a `\(…)` interpolation is never a separator, so the scan is a
/// real reference-shaped scan rather than a `split(';')`.
fn call_argument_spans(program: &str, name: &str, arity: usize) -> Option<Vec<Range<usize>>> {
    let bytes = program.as_bytes();
    let mut search = 0usize;
    while let Some(offset) = program[search..].find(name) {
        let start = search + offset;
        search = start + 1;
        let follows_identifier = start
            .checked_sub(1)
            .is_some_and(|before| is_identifier_byte(bytes[before]));
        let open = start + name.len();
        if follows_identifier || bytes.get(open) != Some(&b'(') {
            continue;
        }
        let Some((separators, close)) = scan_argument_list(bytes, open + 1) else {
            continue;
        };
        if separators.len() + 1 != arity {
            continue;
        }
        let mut spans = Vec::with_capacity(arity);
        let mut from = open + 1;
        for separator in separators {
            spans.push(from..separator);
            from = separator + 1;
        }
        spans.push(from..close);
        return Some(spans);
    }
    None
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// One frame of the argument-list scan: reference code at some bracket depth, or the
/// inside of a string literal.
enum ScanFrame {
    Code { depth: usize },
    String,
}

/// Walks one argument list from just after its opening parenthesis, returning
/// the top-level `;` positions and the closing parenthesis position.
fn scan_argument_list(bytes: &[u8], from: usize) -> Option<(Vec<usize>, usize)> {
    let mut separators = Vec::new();
    let mut frames = vec![ScanFrame::Code { depth: 1 }];
    let mut index = from;
    while index < bytes.len() {
        let byte = bytes[index];
        match frames.last_mut()? {
            ScanFrame::String => match byte {
                b'\\' => {
                    if bytes.get(index + 1) == Some(&b'(') {
                        frames.push(ScanFrame::Code { depth: 1 });
                    }
                    index += 2;
                }
                b'"' => {
                    frames.pop();
                    index += 1;
                }
                _ => index += 1,
            },
            ScanFrame::Code { depth } => {
                match byte {
                    b'"' => frames.push(ScanFrame::String),
                    b'(' | b'[' | b'{' => *depth += 1,
                    b')' | b']' | b'}' => {
                        *depth -= 1;
                        if *depth == 0 {
                            frames.pop();
                            if frames.is_empty() {
                                return Some((separators, index));
                            }
                        }
                    }
                    b';' if *depth == 1 && frames.len() == 1 => separators.push(index),
                    _ => {}
                }
                index += 1;
            }
        }
    }
    None
}

/// The program with the argument at `span` replaced by `replacement`.
fn substitute(program: &str, span: &Range<usize>, replacement: &str) -> String {
    let mut mutant = String::with_capacity(program.len() + replacement.len());
    mutant.push_str(&program[..span.start]);
    mutant.push_str(replacement);
    mutant.push_str(&program[span.end..]);
    mutant
}

/// Every name that resolves to this overload: its canonical name.
fn callable_names(overload: &BuiltinOverloadRecord) -> Vec<&'static str> {
    vec![overload.canonical_name]
}

/// The argument spans of `example`'s call to `overload`, under any name that
/// resolves to it.
fn example_argument_spans(overload: &BuiltinOverloadRecord, example: &BuiltinExample) -> Option<Vec<Range<usize>>> {
    callable_names(overload)
        .into_iter()
        .find_map(|name| call_argument_spans(example.program, name, usize::from(overload.arity)))
}

/// Runs one example twice and returns its outcome only if both runs agree.
///
/// An IMPURE overload (`rand`, `sample`, `uuid_v4`) whose example is not
/// invariant-shaped cannot be audited by comparison — every mutant would look
/// distinguished for the wrong reason. Detecting that mechanically beats
/// maintaining a list of which overloads are reproducible.
fn reproducible_outcome(program: &str, input: &str) -> Option<Outcome> {
    let first = run_program(program, input);
    if matches!(first, Outcome::Inconclusive) || first != run_program(program, input) {
        return None;
    }
    Some(first)
}

/// Every registered overload with arguments has at least one example that
/// distinguishes an implementation which IGNORES each argument position, and at
/// least one that distinguishes an implementation which keeps only ONE output
/// of a multi-valued filter argument.
///
/// The check is mutation, not inspection: each argument is replaced by a
/// stand-in (and, for the cardinality half, doubled into a two-output
/// generator), and the mutant must be observable in the published bytes or the
/// failure. `037` S4 and `040` A1 are exactly the mutants that survive.
#[test]
fn registered_examples_distinguish_argument_ignoring_implementations() {
    let mut findings: Vec<(String, Obligation, String)> = Vec::new();
    for overload in builtin_overloads() {
        // An OPERATOR is spelled as syntax (`+`, `.[]`), not as a call with an
        // argument list, so there is no argument list to mutate.
        if overload.arity == 0 || overload.execution == BuiltinExecution::Operator {
            continue;
        }
        let arity = usize::from(overload.arity);
        let mut scanned = false;
        let mut pinned_positions = vec![false; arity];
        let mut pinned_multi_valued = false;
        for example in overload.examples {
            let Some(spans) = example_argument_spans(overload, example) else {
                continue;
            };
            let Some(baseline) = reproducible_outcome(example.program, example.input) else {
                continue;
            };
            scanned = true;
            for (position, span) in spans.iter().enumerate() {
                if !pinned_positions[position] {
                    pinned_positions[position] = SUBSTITUTIONS.iter().any(|substitution| {
                        distinguishes(
                            &baseline,
                            &substitute(example.program, span, substitution),
                            example.input,
                        )
                    });
                }
                if !pinned_multi_valued && overload.parameters.get(position) == Some(&ParameterKind::Filter) {
                    pinned_multi_valued = exercises_multiple_values(&baseline, example, span);
                }
            }
        }
        let where_ = format!("{}/{}", overload.canonical_name, overload.arity);
        if !scanned {
            findings.push((
                where_,
                Obligation::Unscannable,
                "no example's program could be scanned back to this overload's argument list".to_owned(),
            ));
            continue;
        }
        for (position, pinned) in pinned_positions.iter().enumerate() {
            if !pinned {
                let position = u8::try_from(position).expect("arity fits in a u8");
                findings.push((
                    where_.clone(),
                    Obligation::Position(position),
                    format!(
                        "no example changes its output when argument {position} is replaced, so an \
                         implementation that evaluates and discards it passes every example"
                    ),
                ));
            }
        }
        if !pinned_multi_valued && overload.parameters.contains(&ParameterKind::Filter) {
            findings.push((
                where_,
                Obligation::MultiValued,
                "every example passes a single-valued filter argument, so an implementation that \
                 keeps one output of a multi-valued one passes every example"
                    .to_owned(),
            ));
        }
    }

    let mut unwaived: Vec<String> = Vec::new();
    let mut used = vec![false; WAIVERS.len()];
    for (where_, obligation, detail) in &findings {
        let waiver = WAIVERS.iter().position(|waiver| {
            format!("{}/{}", waiver.name, waiver.arity) == *where_ && waiver.obligation == *obligation
        });
        match waiver {
            Some(index) => used[index] = true,
            None => unwaived.push(format!("{where_} {obligation:?}: {detail}")),
        }
    }
    let stale: Vec<String> = WAIVERS
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|(waiver, _)| {
            format!(
                "{}/{} {:?} — the obligation is now met; delete the waiver ({})",
                waiver.name, waiver.arity, waiver.obligation, waiver.reason
            )
        })
        .collect();

    assert!(
        unwaived.is_empty() && stale.is_empty(),
        "argument-degeneracy audit\n\nDEGENERATE ({}):\n  {}\n\nSTALE WAIVERS ({}):\n  {}\n",
        unwaived.len(),
        unwaived.join("\n  "),
        stale.len(),
        stale.join("\n  "),
    );
}

/// Whether this example says anything at all about the CARDINALITY half of
/// the reference's argument law at this filter position.
///
/// Two independent signals, either of which discharges the obligation:
///
/// - the argument DOUBLED into a two-output generator changes the published
///   outcome, which no implementation that keeps one output can produce; or
/// - the argument text is itself a multi-output expression, so the example's
///   pinned bytes ARE the multi-valued answer.
///
/// The first is the sharp one and is what `040` A1's fix pins. The second
/// exists because the first is silent for the collect-style key laws
/// (`sort_by`, `group_by`, `del`), where doubling an identical argument is
/// idempotent by the LAW and not by any defect — demanding the sharp signal
/// there would be a false alarm. Collapsing with `first(…)` instead is not an
/// option: it also strips path-capability, so a path-taking overload would look
/// distinguished for the wrong reason.
fn exercises_multiple_values(baseline: &Outcome, example: &BuiltinExample, span: &Range<usize>) -> bool {
    let argument = &example.program[span.clone()];
    let doubled = format!("(({argument}),({argument}))");
    distinguishes(baseline, &substitute(example.program, span, &doubled), example.input)
        || argument_output_count(argument, example.input) > 1
}

/// How many outputs one argument's TEXT denotes.
///
/// The argument is run standalone against the example's input and against
/// `null`, and the larger answer wins: a key filter like `.a, .b` is evaluated
/// per ELEMENT inside `sort_by`, so it raises against the array the example
/// feeds the call, while `null` still shows both of its outputs.
fn argument_output_count(argument: &str, input: &str) -> u64 {
    let program = format!("[{argument}] | length");
    [input, "null"]
        .into_iter()
        .filter_map(|input| match run_program(&program, input) {
            Outcome::Published { stdout, .. } => core::str::from_utf8(&stdout)
                .ok()
                .and_then(|text| text.trim().parse::<u64>().ok()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// Whether `mutant` is observably different from the baseline outcome.
///
/// An INCONCLUSIVE mutant — one that did not compile, or that outgrew the
/// audit's budget — never counts: it says nothing about whether the argument
/// was read.
fn distinguishes(baseline: &Outcome, mutant: &str, input: &str) -> bool {
    match run_program(mutant, input) {
        Outcome::Inconclusive => false,
        outcome => outcome != *baseline,
    }
}
