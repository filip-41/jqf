//! XML differential decode oracle: jqf's XML document route versus
//! `quick-xml` (pinned `=0.41.0`) over a fixed corpus. Exits nonzero with a
//! precise report on any `UNdeclared` divergence — an accept/reject split.
//!
//! The comparison is ACCEPT/REJECT over the shared well-formedness core (plan
//! 057 W4's ruling: the XML→value mapping is jqf doctrine and no reference
//! crate shares it, so the value tree is never compared). jqf decodes through
//! its whole-document route; quick-xml's pull reader is driven to end of
//! input. Error *kinds* never need to match on a shared reject.
//!
//! The DECLARED table is the XML row of plan 057's divergence register: the
//! document policies jqf enforces and a pull parser does not (single root,
//! closed root, declared prefixes, entity resolution, duplicate attributes,
//! encoding-declaration grammar), each written per case with its reason. A
//! declared row that stops disagreeing is STALE and fails the run; a
//! disagreement that is not on the table is a defect.

mod corpus;

use std::collections::BTreeMap;

use corpus::Expect;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::differential::{describe_bytes, format_poll_error, print_categories, side};

static CONTROL: ContinueControl = ContinueControl;

/// One decode outcome, classified for cross-implementation comparison.
#[derive(Debug)]
enum Verdict {
    Accept,
    Reject(String),
}

/// Decodes `bytes` as one XML document through jqf's whole-document route.
fn decode_jqf(bytes: &[u8]) -> Verdict {
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 128 << 20, u64::MAX, 512)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources");
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "differential.xml",
        bytes,
        0,
    );
    match jqf_codec_xml::decode_document(source, &mut resources) {
        Ok(_value) => Verdict::Accept,
        Err(error) => Verdict::Reject(format_poll_error(&error)),
    }
}

/// Decodes `bytes` through quick-xml's pull reader (pinned `=0.41.0`),
/// reading every event to end of input.
fn decode_quick_xml(bytes: &[u8]) -> Verdict {
    let mut reader = Reader::from_reader(bytes);
    let mut buffer = Vec::new();
    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Eof) => return Verdict::Accept,
            Ok(_) => {}
            Err(error) => return Verdict::Reject(format!("{error:?}")),
        }
    }
}

/// One declared-split row of the divergence register. See the TOML
/// differential's `Declared` for the contract.
struct Declared {
    name: &'static str,
    jqf: Option<bool>,
    reference: Option<bool>,
    reason: &'static str,
}

/// The XML rows of the divergence register, written up-front from the
/// product laws (see `corpus.rs`'s declared-splits section for the same rows
/// with their inputs).
const DECLARED: &[Declared] = &[
    Declared {
        name: "declared/second-root",
        jqf: Some(false),
        reference: Some(true),
        reason: "the whole document is ONE document: jqf rejects a second root element; quick-xml is a pull parser and reads events freely",
    },
    Declared {
        name: "declared/empty-document",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf requires a document element; quick-xml's reader reaches EOF without error",
    },
    Declared {
        name: "declared/unclosed-root-at-eof",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf requires the document element to close; quick-xml does not check that the root closed at EOF",
    },
    Declared {
        name: "declared/undeclared-prefix-start",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf resolves namespaces and rejects an undeclared prefix; quick-xml without ns_resolution treats it as an ordinary QName",
    },
    Declared {
        name: "declared/undeclared-prefix-root",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf resolves namespaces and rejects an undeclared prefix; quick-xml treats it as an ordinary QName",
    },
    Declared {
        name: "declared/bad-name",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf enforces XML name validity (`<1a/>` is not a Name); quick-xml's reader does not check element names by default",
    },
    Declared {
        name: "declared/unquoted-attribute",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf requires attribute values to be quoted; quick-xml's default reader accepts an unquoted value",
    },
    Declared {
        name: "declared/attributes-no-space",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf requires whitespace between attributes per the XML grammar; quick-xml accepts `<a b=\"1\"c=\"2\"/>` as-is",
    },
    Declared {
        name: "declared/raw-lt-in-attribute",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf rejects a raw `<` inside an attribute value; quick-xml does not scan attribute content for it by default",
    },
    Declared {
        name: "declared/unbound-entity",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf resolves and REQUIRES entity references to be bound; quick-xml leaves `&b;` in the text",
    },
    Declared {
        name: "declared/duplicate-attribute",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf rejects a duplicated expanded attribute name; a pull parser does not compare attributes across events",
    },
    Declared {
        name: "declared/utf16-encoding-declaration",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf's encoding declaration is a grammar step of the selected format; quick-xml ignores it and reads the bytes as given",
    },
    Declared {
        name: "declared/external-entity-declaration",
        jqf: Some(false),
        reference: Some(true),
        reason: "jqf disables external entities (a secure non-validating processor); quick-xml never resolves them",
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
        let reference = decode_quick_xml(&case.bytes);
        let declared = DECLARED.iter().find(|row| row.name == case.name);
        let jqf_accept = matches!(jqf, Verdict::Accept);
        let reference_accept = matches!(reference, Verdict::Accept);

        match declared {
            Some(row) => {
                let jqf_ok = row.jqf == Some(jqf_accept);
                let reference_ok = row.reference == Some(reference_accept);
                if jqf_ok && reference_ok {
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
                (Verdict::Accept, Verdict::Accept) => {
                    if case.expect == Expect::Reject {
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
                (Verdict::Accept, Verdict::Reject(reason)) => {
                    divergences.push(format!(
                        "{}: accept/reject split jqf=Accept quick-xml=Reject({reason:?}); input={}",
                        case.name,
                        describe_bytes(&case.bytes),
                    ));
                }
                (Verdict::Reject(reason), Verdict::Accept) => {
                    divergences.push(format!(
                        "{}: accept/reject split jqf=Reject({reason:?}) quick-xml=Accept; input={}",
                        case.name,
                        describe_bytes(&case.bytes),
                    ));
                }
            },
        }
    }

    println!("jqf-codec-xml-differential: corpus_size={}", cases.len());
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
    if undeclared == 0 && stale.is_empty() {
        println!(
            "jqf-codec-xml-differential: PASS agreements={} declared={declared_tally} undeclared_divergences=0 stale=0",
            cases.len()
        );
        return Ok(());
    }
    eprintln!(
        "jqf-codec-xml-differential: FAIL divergences={} undeclared={undeclared} stale={}",
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
        Verdict::Accept => "Accept",
        Verdict::Reject(_) => "Reject",
    }
}
