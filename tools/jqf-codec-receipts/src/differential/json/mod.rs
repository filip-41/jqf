//! Differential decode oracle: jqf's strict-JSON route versus `serde_json`
//! over a fixed corpus (the old `tools/jqf-codec-json-differential`
//! migrated into the plan-124 harness, Y2). Exits nonzero with a precise
//! report on any divergence — an accept/reject split, or an accept/accept
//! semantic checksum mismatch. Error *kinds* need not match on a shared
//! reject. The verdict/report frame lives in [`crate::differential`]; the
//! corpus, oracles, and parity drives below are verbatim.

mod corpus;
mod oracle;
mod scoped;
mod semantic;

use std::collections::BTreeMap;

use crate::differential::{Verdict, describe_bytes, print_categories, side, verdict_side};

use corpus::Expect;

/// Runs the corpus walk. `args` carries whatever the harness received after
/// the codec name, so the `--dump-accepts` escape hatch survives verbatim.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    let cases = corpus::build();
    if args.iter().any(|argument| argument == "--dump-accepts") {
        dump_accepts(&cases);
        return Ok(());
    }
    let mut category_counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut divergences = Vec::new();
    let mut surprises = Vec::new();

    for case in &cases {
        *category_counts.entry(case.category).or_default() += 1;
        let jqf_verdict = oracle::decode_jqf(&case.bytes, case.depth_limit);
        let serde_verdict = oracle::decode_serde(&case.bytes);
        // The corpus's own expectation, labelled through the frame's side
        // helper (None never occurs here — every case declares a side).
        let expected = side(match case.expect {
            Expect::Accept => Some(true),
            Expect::Reject => Some(false),
        });
        match (&jqf_verdict, &serde_verdict) {
            (Verdict::Accept(jqf_checksum), Verdict::Accept(serde_checksum)) => {
                if jqf_checksum == serde_checksum {
                    if case.expect == Expect::Reject {
                        surprises.push(format!(
                            "{}: expected a shared {}, both accepted with checksum 0x{jqf_checksum:016x} (schema {}); input={}",
                            case.name,
                            expected.to_ascii_lowercase(),
                            semantic::SCHEMA,
                            describe_bytes(&case.bytes),
                        ));
                    }
                } else {
                    divergences.push(format!(
                        "{}: checksum mismatch (schema {}) jqf=0x{jqf_checksum:016x} serde=0x{serde_checksum:016x}; input={}",
                        case.name,
                        semantic::SCHEMA,
                        describe_bytes(&case.bytes),
                    ));
                }
            }
            (Verdict::Reject(_), Verdict::Reject(_)) => {
                if case.expect == Expect::Accept {
                    surprises.push(format!(
                        "{}: expected a shared {}, both rejected; input={}",
                        case.name,
                        expected.to_ascii_lowercase(),
                        describe_bytes(&case.bytes),
                    ));
                }
            }
            (Verdict::Accept(checksum), Verdict::Reject(reason)) => {
                divergences.push(format!(
                    "{}: accept/reject split jqf={}(0x{checksum:016x}) serde={}({reason:?}); input={}",
                    case.name,
                    verdict_side(&jqf_verdict),
                    verdict_side(&serde_verdict),
                    describe_bytes(&case.bytes),
                ));
            }
            (Verdict::Reject(reason), Verdict::Accept(checksum)) => {
                divergences.push(format!(
                    "{}: accept/reject split jqf={}({reason:?}) serde={}(0x{checksum:016x}); input={}",
                    case.name,
                    verdict_side(&jqf_verdict),
                    verdict_side(&serde_verdict),
                    describe_bytes(&case.bytes),
                ));
            }
        }
    }

    println!("jqf-codec-json-differential: corpus_size={}", cases.len());
    print_categories(&category_counts);
    if !surprises.is_empty() {
        println!(
            "corpus-authoring notes (not divergences; both engines agreed against this corpus's own expectation):"
        );
        for surprise in &surprises {
            println!("  {surprise}");
        }
    }

    let (scoped_comparisons, scoped_divergences) = scoped::run(&cases);

    if divergences.is_empty() && scoped_divergences.is_empty() {
        println!(
            "jqf-codec-json-differential: PASS agreements={} divergences=0 scoped_comparisons={scoped_comparisons} scoped_divergences=0",
            cases.len()
        );
        return Ok(());
    }

    for divergence in &scoped_divergences {
        divergences.push(format!("scoped {divergence}"));
    }

    eprintln!("jqf-codec-json-differential: FAIL divergences={}", divergences.len());
    for divergence in &divergences {
        eprintln!("  {divergence}");
    }
    Err(format!("corpus divergences={}", divergences.len()))
}

/// Prints every ACCEPT case as `name<TAB>base64(bytes)`, one per line, and runs
/// nothing.
///
/// The corpus is the repo's escaping authority — the 1024-item escape array, the
/// every-escape-form cycle, the raw-Unicode-space strings, the surrogate pairs —
/// and a second gate that needed those payloads would otherwise keep its own
/// copy of them and drift. This is the same `--dump-rows` contract
/// `tools/jqf-cli-jq-compat.sh` established, for the same reason.
///
/// base64 because the payloads carry raw control bytes and deliberately invalid
/// UTF-8; REJECT cases are excluded because a consumer that cannot decode a case
/// has nothing to compare.
fn dump_accepts(cases: &[corpus::Case]) {
    for case in cases {
        if case.expect != Expect::Accept {
            continue;
        }
        println!("{}\t{}", case.name, base64(&case.bytes));
    }
}

/// Standard base64 with padding, written out rather than pulled in: this tool's
/// dependency set is the codec and `serde_json`, and one encoder is smaller than
/// a crate.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0_u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = u32::from(block[0]) << 16 | u32::from(block[1]) << 8 | u32::from(block[2]);
        for position in 0..4 {
            if position <= chunk.len() {
                let index = (packed >> (18 - position * 6)) & 0x3f;
                out.push(char::from(ALPHABET[index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}
