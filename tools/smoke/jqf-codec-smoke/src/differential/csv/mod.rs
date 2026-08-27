//! CSV (delimited) differential oracle: jqf's RFC 4180 record route versus
//! the `csv` crate (pinned `=1.4.0`, the lineage audit's pin) over a fixed
//! corpus. Exits nonzero with a precise report on any `UNdeclared` divergence —
//! an accept/reject split, or an accept/accept record-product mismatch. Error
//! *kinds* need not match on a shared reject.
//!
//! The comparison is at the record/field product: every corpus input is
//! framed and every payload decoded to its field strings on both sides, and
//! the two products must be identical (the record/field product of the
//! shared RFC 4180 grammar, below the header/aggregation doctrine). jqf's
//! framing is the strict profile: a record ends at LF or CRLF, a bare CR is a
//! framing fault, and a missing final terminator is a fault raised after the
//! final record publishes. The `csv` crate's reader accepts a bare-CR/CRLF
//! mix and an unterminated final record.
//!
//! The DECLARED table is the CSV row of divergence register,
//! seeded from the lineage's own matrix (the three classes the attempt
//! carried as bools: `lf-policy`, `bare-cr-policy`, `invalid-utf8`), each
//! written per case with its reason. A declared row that stops disagreeing is
//! STALE and fails the run; a disagreement that is not on the table is a
//! defect.

mod corpus;

use std::collections::BTreeMap;

use corpus::Expect;
use jqf_codec_core::{
    AccessOutcome, CodecDemand, CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, RecordBatchLimit,
    RecordPoll, ValidationMode,
};
use jqf_codec_delimited::{CsvDecodeOptions, RECORD_ROUTE_SLOT};
use jqf_data::DialectId;
use jqf_data::Value;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::differential::{describe_bytes, format_poll_error, print_categories, side};

static CONTROL: ContinueControl = ContinueControl;

const CREDIT: u32 = 4_096;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 128 << 20, u64::MAX, 512)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(CREDIT).expect("work"),
    )
    .expect("resources")
}

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "differential.csv",
        bytes,
        0,
    )
}

/// One decode outcome: either the full record product (one field-string array
/// per record) or a rejection.
#[derive(Debug)]
enum Verdict {
    Accept(Vec<Vec<String>>),
    Reject(String),
}

/// Decodes `bytes` as a CSV stream through jqf's strict record route: frame
/// the records, then decode each payload through the payload provider's
/// whole-document route, exactly as the record drive does.
fn decode_jqf(bytes: &[u8]) -> Verdict {
    let mut resources = resources();
    let options = match CsvDecodeOptions::try_new(None, None, u64::MAX, false) {
        Ok(options) => options,
        Err(error) => return Verdict::Reject(format!("options: {error:?}")),
    };
    let mut provider = match jqf_codec_delimited::create_record_provider(
        source(bytes),
        options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        &mut resources,
    ) {
        Ok(provider) => provider,
        Err(error) => return Verdict::Reject(format!("provider: {:?}", error.kind())),
    };
    let mut stream = match provider.open_record_route(RECORD_ROUTE_SLOT, &mut resources) {
        Ok(stream) => stream,
        Err(error) => return Verdict::Reject(format!("open route: {:?}", error.kind())),
    };
    let Some(limit) = RecordBatchLimit::new(256, 256 * 1024) else {
        return Verdict::Reject("batch limit".into());
    };
    let mut batch = jqf_codec_core::RecordBatch::new();
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    for _ in 0..16_384 {
        let mut run = CodecRunContext::new(&mut resources);
        match stream.poll(limit, &mut batch, &mut run) {
            Ok(RecordPoll::Filled) => {
                for entry in batch.entries() {
                    if let jqf_codec_core::RecordEntry::Record(item) = entry {
                        payloads.push(item.lease().payload().to_vec());
                    }
                }
                batch.clear();
            }
            Ok(RecordPoll::Pending) => {
                if resources.try_begin_next_cooperative_entry(CREDIT).is_err() {
                    return Verdict::Reject("cooperative entry refused".into());
                }
            }
            Ok(RecordPoll::End(_)) => break,
            Err(error) => return Verdict::Reject(format_poll_error(&error)),
        }
    }
    // Every payload is one record: decode it to its field strings.
    let mut records = Vec::with_capacity(payloads.len());
    for payload in &payloads {
        match decode_record_payload(payload, &mut resources) {
            Ok(fields) => records.push(fields),
            Err(error) => return Verdict::Reject(error),
        }
    }
    Verdict::Accept(records)
}

/// Decodes ONE record payload through the payload provider's whole-document
/// route and returns its field strings.
fn decode_record_payload(bytes: &[u8], resources: &mut ResourceContext<'_>) -> Result<Vec<String>, String> {
    let registration = jqf_codec_delimited::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("csv payload decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_delimited::JQF_RFC4180_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            resources,
        )
        .map_err(|error| format!("payload provider: {:?}", error.kind()))?;
    let mut demand = CodecDemand::try_new(resources);
    demand
        .try_insert(&DemandClause::SemanticRoot)
        .map_err(|error| format!("demand root: {error:?}"))?;
    demand
        .try_insert(&DemandClause::ValueShape)
        .map_err(|error| format!("demand shape: {error:?}"))?;
    let requirement = jqf_codec_core::AccessRequirement::try_whole(
        demand,
        jqf_codec_core::AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .map_err(|error| format!("requirement: {error:?}"))?;
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(CREDIT);
        let result = session.decode(&mut run).map_err(|error| format!("decode: {error:?}"))?;
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            return Err("expected full document".into());
        };
        let value = product
            .document()
            .materialize_root(resources)
            .map_err(|error| format!("materialize: {error:?}"))?;
        fields_of(&value)
    }
}

/// Extracts one record's field strings from the materialized document.
fn fields_of(value: &Value) -> Result<Vec<String>, String> {
    let Value::Array(array) = value else {
        return Err(format!("record is not an array: {value:?}"));
    };
    let mut fields = Vec::with_capacity(array.len());
    for item in array {
        let Value::String(text) = item else {
            return Err(format!("field is not a string: {item:?}"));
        };
        fields.push(text.to_string());
    }
    Ok(fields)
}

/// Decodes `bytes` through the `csv` crate (pinned `=1.4.0`), without
/// headers, collecting the record/field product.
fn decode_csv_crate(bytes: &[u8]) -> Verdict {
    let mut reader = csv::ReaderBuilder::new().has_headers(false).from_reader(bytes);
    let mut records = Vec::new();
    for record in reader.records() {
        match record {
            Ok(record) => {
                let fields: Vec<String> = record.iter().map(str::to_owned).collect();
                records.push(fields);
            }
            Err(error) => return Verdict::Reject(error.to_string()),
        }
    }
    Verdict::Accept(records)
}

/// One declared-split row of the divergence register. See the TOML
/// differential's `Declared` for the contract.
struct Declared {
    name: &'static str,
    jqf: Option<bool>,
    reference: Option<bool>,
    reason: &'static str,
}

/// The CSV rows of the divergence register — the lineage matrix's three
/// policy classes written per case — plus the BOM rejection. (The two
/// unterminated-final-record rows retired to agreement fixtures when the
/// missing-terminator fault became an advisory, RFC 4180 §2.2.)
const DECLARED: &[Declared] = &[
    Declared {
        name: "declared/bare-cr-framing-fault",
        jqf: Some(false),
        reference: Some(true),
        reason: "strict profile: a bare CR not followed by LF is a framing fault; the csv crate treats a lone CR as a terminator (bare-cr-policy)",
    },
    Declared {
        name: "declared/bare-cr-only",
        jqf: Some(false),
        reference: Some(true),
        reason: "strict profile: a lone bare CR is a framing fault; the csv crate treats it as a terminator (bare-cr-policy)",
    },
    Declared {
        name: "declared/initial-byte-order-mark",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf rejects a source-start byte-order mark as a framing fault; the csv crate skips it silently",
    },
    Declared {
        name: "declared/blank-line-between",
        jqf: Some(true),
        reference: Some(true),
        reason: "jqf publishes a blank line as a zero-field record; the csv crate omits the empty line, so the record products differ",
    },
    Declared {
        name: "declared/mixed-lengths",
        jqf: Some(true),
        reference: Some(false),
        reason: "jqf's rfc4180 dialect has no width law (each record is its own array); the csv crate's default enforces uniform record width",
    },
    Declared {
        name: "declared/mixed-lengths-wider",
        jqf: Some(true),
        reference: Some(false),
        reason: "jqf's rfc4180 dialect has no width law (each record is its own array); the csv crate's default rejects a WIDER row",
    },
    Declared {
        name: "declared/quote-mid-field",
        jqf: Some(false),
        reference: Some(true),
        reason: "RFC 4180 forbids a quote inside an unquoted field; jqf now REJECTS the lone mid-field quote as a malformed field (InvalidInput) while the csv crate keeps the quote literal (REVISED 2026-08-09, batch-6 B8: the toggle-anywhere grammar was the bug — a quote opens quoted state only at a field start; the 2026-08-07 ruling's toggle-anywhere product is superseded)",
    },
    Declared {
        name: "declared/quote-in-unquoted-field",
        jqf: Some(false),
        reference: Some(true),
        reason: "the mid-field quote policy as a split: jqf opens a quoted field that never closes; the csv crate keeps the quote literal",
    },
    Declared {
        name: "declared/unclosed-quote",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf requires a quoted field to close; the csv crate extends it to end of input",
    },
    Declared {
        name: "declared/quote-then-garbage",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf requires a quoted field to close; the csv crate is lenient at end of input",
    },
    Declared {
        name: "declared/quote-not-closed-at-eof",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf requires a quoted field to close; the csv crate extends it to end of input",
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
        let reference = decode_csv_crate(&case.bytes);
        let declared = DECLARED.iter().find(|row| row.name == case.name);
        let jqf_accept = matches!(jqf, Verdict::Accept(_));
        let reference_accept = matches!(reference, Verdict::Accept(_));

        match declared {
            Some(row) => {
                let jqf_ok = row.jqf == Some(jqf_accept);
                let reference_ok = row.reference == Some(reference_accept);
                if jqf_ok && reference_ok {
                    // A both-accept row must still DISAGREE on the record
                    // products — the product divergence is the reason the
                    // register row exists. A fix that makes the sides
                    // CONVERGE while both still accept lands as a STALE row.
                    if let (Verdict::Accept(jqf_products), Verdict::Accept(reference_products)) = (&jqf, &reference)
                        && jqf_products == reference_products
                    {
                        stale.push(format!(
                            "{}: declared both-accept but the record products now meet — the row is STALE",
                            case.name,
                        ));
                        continue;
                    }
                    declared_tally += 1;
                    divergences.push(format!(
                        "DECLARED[{}] {}: {}; jqf={} reference={}",
                        case.name,
                        case.name,
                        row.reason,
                        verdict_side(&jqf),
                        verdict_side(&reference),
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
                (Verdict::Accept(jqf_records), Verdict::Accept(reference_records)) => {
                    if jqf_records != reference_records {
                        divergences.push(format!(
                            "{}: record-product mismatch jqf={:?} csv={:?}; input={}",
                            case.name,
                            jqf_records,
                            reference_records,
                            describe_bytes(&case.bytes),
                        ));
                    } else if case.expect == Expect::Reject {
                        divergences.push(format!(
                            "{}: expected a shared reject, both accepted; input={}",
                            case.name,
                            describe_bytes(&case.bytes),
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
                (Verdict::Accept(records), Verdict::Reject(reason)) => {
                    divergences.push(format!(
                        "{}: accept/reject split jqf=Accept({records:?}) csv=Reject({reason:?}); input={}",
                        case.name,
                        describe_bytes(&case.bytes),
                    ));
                }
                (Verdict::Reject(reason), Verdict::Accept(records)) => {
                    divergences.push(format!(
                        "{}: accept/reject split jqf=Reject({reason:?}) csv=Accept({records:?}); input={}",
                        case.name,
                        describe_bytes(&case.bytes),
                    ));
                }
            },
        }
    }

    println!("jqf-codec-csv-differential: corpus_size={}", cases.len());
    print_categories(&category_counts);
    for divergence in &divergences {
        println!("  {divergence}");
    }
    for entry in &stale {
        println!("  STALE-DECLARATION {entry}");
    }

    let undeclared = divergences
        .iter()
        .filter(|divergence| !divergence.starts_with("DECLARED["))
        .count();
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
    if undeclared == 0 && stale.is_empty() {
        println!(
            "jqf-codec-csv-differential: PASS agreements={} declared={declared_tally} undeclared_divergences=0 stale=0",
            cases.len()
        );
        return Ok(());
    }
    eprintln!(
        "jqf-codec-csv-differential: FAIL divergences={} undeclared={undeclared} stale={}",
        divergences.len(),
        stale.len()
    );
    Err(format!(
        "divergences={} undeclared={undeclared} stale={}",
        divergences.len(),
        stale.len()
    ))
}

fn verdict_side(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Accept(_) => "Accept",
        Verdict::Reject(_) => "Reject",
    }
}
