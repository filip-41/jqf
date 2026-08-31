//! Totality-as-a-test: no registered builtin, in any path-mode argument
//! position, may answer with an internal-contract violation.
//!
//! Path mode is the engine's eager owned evaluator — it is what `path/1`,
//! `paths`, `getpath`, `setpath`, `delpaths`, `del/1` and the assignment
//! operators run their arguments through, and it is also the evaluator the
//! graph machine reuses for its own builtin filter arguments. For most of its
//! life its call dispatch was grown one family at a time, so a builtin outside
//! the families it had learned reached a `path mode reached an unhandled
//! evaluator` internal-contract violation on a program the reference answers.
//!
//! This harness exists so that hole cannot be reopened by ADDING a builtin.
//! It enumerates the registry rather than a list of cases: every registered
//! overload is called, at its exact arity, inside each of the path-mode
//! argument positions below. An overload registered tomorrow is covered by
//! this test the day it lands, with no row to remember to write.
//!
//! What it asserts is deliberately weak on VALUES and absolute on CLASS: any
//! answer, any reference-shaped raise, any resource refusal, and `halt` are all fine.
//! `CodecFailureKind::InternalContractViolation` is not — an internal contract
//! is a statement about the engine's own invariants, and no user program may
//! provoke one. Exact per-builtin outputs are pinned against the live
//! reference by the
//! `tools/jqf-cli-jq-compat.sh` corpus, which is the right place for values.
//!
//! `jqf-engine` owns the registry but cannot depend on `jqf-sdk`, so the
//! harness lives here for the same reason `tests/builtin_examples.rs` does:
//! the SDK is the first place the whole compile-to-publish pipeline is
//! available together.

/// A process-lifetime built-in dialect for request construction (123 X5).
fn json_dialect() -> &'static DialectId {
    Box::leak(Box::new(DialectId::try_new("rfc8259").expect("dialect")))
}

use core::sync::atomic::{AtomicU64, Ordering};
use jqf_codec_core::{CodecFailureKind, DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{BuiltinExecution, CodecRequirementPolicy, CompileOptions, builtin_overloads, try_compile_program};

use jqf_resource::{Control, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, Input, ItemSink, PipelineFailure, PipelinePolicy, Request,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const COOPERATIVE_CREDITS: u32 = 64;

/// How many host control observations one probe may spend before the harness
/// cancels it.
///
/// Several registered generators do not terminate when handed `.` as their
/// argument — `repeat(.)` re-applies the identity forever, and the reference
/// hangs on it
/// too — so a harness that enumerates the registry MUST bound its probes from
/// the outside. The budget is the cooperative-cancellation channel the engine
/// already polls, so a runaway ends in a control error, which is a perfectly
/// acceptable outcome for this test and is not an internal contract violation.
/// The ceiling is far above any terminating probe over a two-byte document.
const CONTROL_BUDGET: u64 = 20_000;

/// A host control that cancels after [`CONTROL_BUDGET`] observations.
struct BudgetControl {
    remaining: AtomicU64,
}

impl BudgetControl {
    fn new() -> Self {
        Self {
            remaining: AtomicU64::new(CONTROL_BUDGET),
        }
    }
}

impl Control for BudgetControl {
    fn check(&self) -> ControlOutcome {
        if self.remaining.load(Ordering::Relaxed) == 0 {
            return ControlOutcome::Cancelled;
        }
        self.remaining.fetch_sub(1, Ordering::Relaxed);
        ControlOutcome::Continue
    }
}

/// The document every probe runs against. `{"a":1}` is `getpath`'s own
/// historical reproducer: the argument filter sees the string `"a"`, which is
/// a legal key, so a builtin that merely passes it through still names a real
/// location.
const INPUT: &str = "{\"a\":1}";

/// The nesting ceiling the probe ledger grants.
///
/// Path mode recurses on the NATIVE stack, one level per `recurse`/`repeat`/
/// `while`/`until` iteration, and several registered generators are unbounded
/// when handed `.` as their argument (`repeat(.)` never stops). The engine's
/// own `PATH_RECURSION_LIMIT` is 1000, which a debug test thread's stack
/// cannot afford, so the probe ledger bounds the same dimension far earlier:
/// the refusal is a resource error either way, which is exactly what this test
/// wants to see instead of a stack overflow.
const PROBE_NESTING_DEPTH: u32 = 32;

/// Every path-mode position a builtin call can occupy as a frozen ARGUMENT,
/// with `{}` standing for the call.
///
/// The list spans the whole path-mode surface rather than `getpath` alone,
/// because each entry point seeds its own run: `path/1` publishes locations,
/// the read/write/delete laws publish values, `del/1` lowers to
/// `delpaths([path(f)])`, and the two assignment operators drive a modify fold
/// over a nested run.
const POSITIONS: &[&str] = &[
    "getpath([({}) ])",
    "setpath([({}) ]; 2)",
    "delpaths([[({}) ]])",
    "path(.[({}) ])",
    "del(.[({}) ])",
    ".[({}) ] = 2",
    ".[({}) ] |= 2",
];

/// Overloads that are registered but not callable by their canonical name from
/// program source, so no probe program compiles for them.
///
/// EMPTY today — every registered overload, including the operator spellings
/// (`_negate/1`), resolves from source. The list is PINNED rather than skipped
/// silently: a newly registered builtin that fails to compile here is either a
/// genuine internal spelling (add it, with the reason) or a resolution bug (fix
/// it). Silently skipping what does not compile would let a new family slip
/// past the whole harness.
const NOT_SOURCE_CALLABLE: &[(&str, u8)] = &[];

/// Collects every published byte, mirroring `tests/builtin_examples.rs`.
struct CollectingSink;

impl ItemSink for CollectingSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn resources(control: &BudgetControl) -> ResourceContext<'_> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            64 << 20,
            0,
            PROBE_NESTING_DEPTH,
        ))
        .expect("account allocates"),
        control,
        WorkMeter::try_new_v1(COOPERATIVE_CREDITS).expect("work meter starts"),
    )
    .expect("resources start")
}

/// The call spelling of one overload at its exact arity, with `.` for every
/// filter argument (the identity is the one argument every parameter kind
/// accepts as a spelling, whatever it then makes of the value).
fn call_spelling(name: &str, arity: u8) -> String {
    if arity == 0 {
        return name.to_owned();
    }
    let args = vec!["."; usize::from(arity)].join("; ");
    format!("{name}({args})")
}

/// Runs one program over [`INPUT`] and reports the internal-contract violation
/// it provoked, if any.
///
/// A compile failure is reported separately (`Err`), because "this overload has
/// no source spelling" is a different fact from "this overload broke path
/// mode", and the caller pins the two differently.
fn contract_violation(program_text: &str) -> Result<Option<&'static str>, ()> {
    let registration = jqf_codec_json::registration().expect("json registration is valid");
    let registrations = [&registration];
    let catalog = CodecCatalog::new(&registrations);
    let format = || FormatId::try_new(jqf_codec_json::FORMAT_ID).expect("format id is valid");
    let dialect = || DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect id is valid");
    let control = BudgetControl::new();
    let mut resources = resources(&control);
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);

    let Ok(program) = try_compile_program(program_text, policy, CompileOptions::new(), &resources) else {
        return Err(());
    };
    let Ok(requirement) = program.try_requirement(&resources) else {
        return Err(());
    };

    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "probe.json",
        INPUT.as_bytes(),
        0,
    );
    let mut sink = CollectingSink;
    let request = Request::new(&program, Input::Whole(INPUT.as_bytes()))
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
    match jqf_sdk::execute(request, &mut sink) {
        Ok(_) => Ok(None),
        Err(error) => match error.pipeline_failure() {
            Some(PipelineFailure::Codec(codec)) => match codec.kind() {
                CodecFailureKind::InternalContractViolation { contract } => Ok(Some(contract)),
                _ => Ok(None),
            },
            _ => Ok(None),
        },
    }
}

/// Every registered builtin, called at its exact arity inside every path-mode
/// argument position, answers WITHOUT an internal-contract violation.
#[test]
fn no_registered_builtin_violates_an_internal_contract_in_a_path_argument() {
    let mut violations: Vec<String> = Vec::new();
    let mut uncompilable: Vec<(&str, u8)> = Vec::new();
    for overload in builtin_overloads() {
        // A `Definition`/`Operator` overload has no call spelling of its own.
        if !matches!(
            overload.execution,
            BuiltinExecution::Evaluator | BuiltinExecution::Lowering
        ) {
            continue;
        }
        let call = call_spelling(overload.canonical_name, overload.arity);
        let mut compiled_anywhere = false;
        for position in POSITIONS {
            let program_text = position.replace("{}", &format!("\"a\" | {call}"));
            match contract_violation(&program_text) {
                Ok(None) => compiled_anywhere = true,
                Ok(Some(contract)) => {
                    compiled_anywhere = true;
                    violations.push(format!("{program_text}  =>  {contract}"));
                }
                Err(()) => {}
            }
        }
        if !compiled_anywhere {
            uncompilable.push((overload.canonical_name, overload.arity));
        }
    }
    assert!(
        violations.is_empty(),
        "{} path-mode argument programs raised an internal-contract violation:\n{}",
        violations.len(),
        violations.join("\n"),
    );
    assert_eq!(
        uncompilable, NOT_SOURCE_CALLABLE,
        "the set of overloads with no source call spelling changed; pin the new \
         entry with its reason or fix its resolution",
    );
}
