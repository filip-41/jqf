//! TOML differential decode oracle: jqf's TOML route versus the `toml` crate
//! (pinned `=1.1.2`, the bench crate's own pin) over a fixed corpus. Exits
//! nonzero with a precise report on any `UNdeclared` divergence — an
//! accept/reject split, or an accept/accept semantic checksum mismatch
//! (schema `toml-semantic-fnv1a64-v1`). Error *kinds* need not match on a
//! shared reject.
//!
//! The DECLARED table is the TOML row of divergence register: every
//! case whose engines are EXPECTED to disagree, with its written reason. A
//! declared row that stops disagreeing is STALE and fails the run (the
//! jq-suite stale-entry law), so a fix cannot leave its waiver behind. A
//! disagreement that is not on the table is a defect.

mod corpus;
mod semantic;

use std::collections::BTreeMap;

use corpus::Expect;
use jqf_codec_core::{CodecRunContext, DecodeRequest, DiagnosticPolicy, ValidationMode};
use jqf_data::DialectId;
use jqf_engine::{
    CodecInputOutcome, CodecInputResult, CodecRequirementPolicy, EngineResult, try_lower_root_requirement,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::differential::{Verdict, describe_bytes, format_poll_error, print_categories, side, verdict_side};

const CREDIT: u32 = 4_096;
const MEMORY_BYTES: u64 = 128 << 20;

static CONTROL: ContinueControl = ContinueControl;

/// Decodes `bytes` as one TOML document through jqf's whole-document route.
fn decode_jqf(bytes: &[u8]) -> Verdict {
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, MEMORY_BYTES, u64::MAX, 512)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(CREDIT).expect("work"),
    )
    .expect("resources");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "differential.toml",
        bytes,
        0,
    );
    let registration = match jqf_codec_toml::registration_1_0() {
        Ok(registration) => registration,
        Err(error) => return Verdict::Reject(format!("registration: {error:?}")),
    };
    let mut provider = match registration.decoder() {
        Some(decoder) => match decoder.create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_toml::TOML_1_0_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        ) {
            Ok(provider) => provider,
            Err(error) => return Verdict::Reject(format!("provider: {:?}", error.kind())),
        },
        None => return Verdict::Reject("registration lacks a decoder".into()),
    };
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let demand = match try_lower_root_requirement(policy, Some(0), &resources) {
        Ok(demand) => demand,
        Err(error) => return Verdict::Reject(format!("requirement: {:?}", error.kind())),
    };
    let handle = match provider.bind(&demand) {
        Ok(handle) => handle,
        Err(error) => return Verdict::Reject(format!("bind: {error:?}")),
    };
    let mut session = match provider.open(&handle, &mut resources) {
        Ok(session) => session,
        Err(error) => return Verdict::Reject(format!("open: {:?}", error.kind())),
    };
    {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(CREDIT);
        let result = match session.decode(&mut run) {
            Ok(result) => result,
            Err(error) => return Verdict::Reject(format_poll_error(&error)),
        };
        let (outcome, _report) = match CodecInputResult::try_from_access(result) {
            Ok(result) => result.into_parts(),
            Err(error) => return Verdict::Reject(format!("handoff: {:?}", error.kind())),
        };
        let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
            return Verdict::Reject(format!("unexpected outcome: {outcome:?}"));
        };
        match located.product().document().materialize_root(&mut resources) {
            Ok(value) => Verdict::Accept(semantic::jqf_value(&value)),
            Err(error) => Verdict::Reject(format!("materialize: {error:?}")),
        }
    }
}

/// Decodes `bytes` through the `toml` crate (pinned `=1.1.2`).
fn decode_toml(bytes: &[u8]) -> Verdict {
    match std::str::from_utf8(bytes) {
        Ok(text) => match toml::from_str::<toml::Value>(text) {
            Ok(value) => Verdict::Accept(semantic::toml_value(&value)),
            Err(error) => Verdict::Reject(error.to_string()),
        },
        Err(_) => Verdict::Reject("input is not UTF-8".into()),
    }
}

/// One declared-split row of the divergence register.
///
/// `jqf` and `reference` are the two verdicts the row EXPECTS
/// (Some means that side must accept; the checksum is ignored — a declared
/// split's checksums need not meet). The `reason` is written at authoring
/// time, before the comparator ever ran.
struct Declared {
    name: &'static str,
    jqf: Option<bool>,
    reference: Option<bool>,
    reason: &'static str,
}

/// The TOML rows of the divergence register, written up-front from the
/// product laws (see `corpus.rs`'s declared-splits section for the same rows
/// with their inputs). The lowercase-`z` offset row retired to a datetime
/// agreement when the decoder accepted it as UTC.
const DECLARED: &[Declared] = &[
    Declared {
        name: "declared/exact-decimal-split",
        jqf: Some(true),
        reference: Some(true),
        reason: "jqf retains the exact decimal spelling (D1 numbers law); the `toml` crate rounds floats to f64, so the semantic checksums cannot meet",
    },
    Declared {
        name: "declared/huge-exponent-split",
        jqf: Some(true),
        reference: Some(false),
        reason: "jqf's exact arithmetic accepts 1e400; the `toml` crate's f64 storage errors out of range",
    },
    Declared {
        name: "declared/negative-zero-offset-split",
        jqf: Some(true),
        reference: Some(true),
        reason: "jqf keeps the unknown-local-offset fact (`-00:00`); the `toml` crate normalizes it to a zero offset, so the canonical texts differ",
    },
];

#[expect(
    clippy::too_many_lines,
    reason = "one case loop: classification, declared table, receipt"
)]
pub(crate) fn run() -> Result<(), String> {
    let cases = corpus::build();
    let mut category_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut divergences = Vec::new();
    let mut declared_tally = 0_u64;
    let mut stale = Vec::new();

    for case in &cases {
        *category_counts.entry(case.category).or_default() += 1;
        let jqf = decode_jqf(&case.bytes);
        let reference = decode_toml(&case.bytes);
        let declared = DECLARED.iter().find(|row| row.name == case.name);
        let jqf_accept = matches!(jqf, Verdict::Accept(_));
        let reference_accept = matches!(reference, Verdict::Accept(_));

        match declared {
            Some(row) => {
                // The register row fires when the two sides land where the
                // written reason says they land. Any OTHER outcome is either
                // stale (the divergence went away) or wrong (it landed
                // differently than the reason claims) — both fail.
                let jqf_ok = row.jqf == Some(jqf_accept);
                let reference_ok = row.reference == Some(reference_accept);
                if jqf_ok && reference_ok {
                    // A both-accept row must still DISAGREE on the semantic
                    // checksums — the checksum divergence is the reason the
                    // register row exists. A fix that makes the sides
                    // CONVERGE while both still accept lands as a STALE row.
                    if let (Verdict::Accept(jqf_sum), Verdict::Accept(reference_sum)) = (&jqf, &reference)
                        && jqf_sum == reference_sum
                    {
                        stale.push(format!(
                            "{}: declared both-accept but the semantic checksums now meet \
                             (0x{jqf_sum:016x}) — the row is STALE",
                            case.name,
                        ));
                        continue;
                    }
                    declared_tally += 1;
                    divergences.push(format!(
                        "DECLARED[{}] {case_name}: {}; jqf={jqf_side} reference={reference_side}",
                        case.name,
                        row.reason,
                        case_name = case.name,
                        jqf_side = verdict_side(&jqf),
                        reference_side = verdict_side(&reference),
                    ));
                } else {
                    stale.push(format!(
                        "{}: declared as {}/{} but landed jqf={} reference={} — the row is STALE or wrong",
                        case.name,
                        side(row.jqf),
                        side(row.reference),
                        verdict_side(&jqf),
                        verdict_side(&reference),
                    ));
                }
            }
            None => match (&jqf, &reference) {
                (Verdict::Accept(jqf_checksum), Verdict::Accept(reference_checksum)) => {
                    if jqf_checksum != reference_checksum {
                        divergences.push(format!(
                            "{}: checksum mismatch (schema {}) jqf=0x{jqf_checksum:016x} toml=0x{reference_checksum:016x}; input={}",
                            case.name,
                            semantic::SCHEMA,
                            describe_bytes(&case.bytes),
                        ));
                    } else if case.expect == Expect::Reject {
                        divergences.push(format!(
                            "{}: expected a shared reject, both accepted (schema {})",
                            case.name,
                            semantic::SCHEMA,
                        ));
                    }
                }
                (Verdict::Reject(_), Verdict::Reject(_)) => {
                    if case.expect == Expect::Accept {
                        divergences.push(format!(
                            "{}: expected a shared accept, both rejected; input={}",
                            case.name,
                            describe_bytes(&case.bytes),
                        ));
                    }
                }
                (Verdict::Accept(checksum), Verdict::Reject(reason)) => {
                    divergences.push(format!(
                        "{}: accept/reject split jqf=Accept(0x{checksum:016x}) toml=Reject({reason:?}); input={}",
                        case.name,
                        describe_bytes(&case.bytes),
                    ));
                }
                (Verdict::Reject(reason), Verdict::Accept(checksum)) => {
                    divergences.push(format!(
                        "{}: accept/reject split jqf=Reject({reason:?}) toml=Accept(0x{checksum:016x}); input={}",
                        case.name,
                        describe_bytes(&case.bytes),
                    ));
                }
            },
        }
    }

    println!("jqf-codec-toml-differential: corpus_size={}", cases.len());
    print_categories(&category_counts);
    for divergence in &divergences {
        println!("  {divergence}");
    }
    for entry in &stale {
        println!("  STALE-DECLARATION {entry}");
    }

    // A declared row whose corpus case was deleted (or renamed) deadens
    // silently: declared_tally + stale covers the rows that FIRED, so the
    // shortfall names the rows that never did.
    if declared_tally + stale.len() as u64 != DECLARED.len() as u64 {
        let unfired: Vec<&str> = DECLARED
            .iter()
            .filter(|row| !cases.iter().any(|case| case.name == row.name))
            .map(|row| row.name)
            .collect();
        stale.push(format!(
            "{} declared row(s) never fired (their corpus case is missing): {unfired:?}",
            DECLARED.len() as u64 - (declared_tally + stale.len() as u64)
        ));
    }
    if divergences.is_empty() && stale.is_empty() {
        println!(
            "jqf-codec-toml-differential: PASS agreements={} declared={declared_tally} divergences=0 stale=0",
            cases.len()
        );
        return Ok(());
    }
    let undeclared = divergences
        .iter()
        .filter(|divergence| !divergence.starts_with("DECLARED["))
        .count();
    if undeclared == 0 && stale.is_empty() {
        println!(
            "jqf-codec-toml-differential: PASS agreements={} declared={declared_tally} undeclared_divergences=0 stale=0",
            cases.len()
        );
        return Ok(());
    }
    eprintln!(
        "jqf-codec-toml-differential: FAIL divergences={} undeclared={undeclared} stale={}",
        divergences.len(),
        stale.len()
    );
    Err(format!(
        "divergences={} undeclared={undeclared} stale={}",
        divergences.len(),
        stale.len()
    ))
}
