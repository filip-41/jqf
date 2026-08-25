#![allow(
    clippy::too_many_lines,
    clippy::collapsible_if,
    clippy::doc_markdown,
    clippy::needless_borrow,
    reason = "the conformance runner mirrors the vendored suite in long sequential receipts"
)]

//! The `xml.xpath@1` conformance runner over the vendored libxml2 XPath
//! suite (see `corpus/PROVENANCE.md` for the pin).
//!
//! The runner decodes each fixture through the XML codec's whole route,
//! classifies every suite expression against the closed profile grammar
//! (compile rejections are conformance facts, not noise), and compares the
//! in-profile selections against the vendored oracle's element entries in
//! document order. Receipt line:
//!
//! ```text
//! xpath-conformance: total=N in-profile=I out-profile=O pass=P fail=F
//! ```
//!
//! `I + O == N` and `P + F == I`; any failure exits 1.

use std::path::{Path, PathBuf};

use jqf_builtins::selector::SelectorBudget;
use jqf_codec_core::{
    AccessGuarantees, AccessOutcome, AccessRequirement, CodecDemand, CodecRunContext, DecodeRequest, DemandClause,
    DiagnosticPolicy, ValidationMode,
};
use jqf_data::DialectId;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

static CONTROL: ContinueControl = ContinueControl;

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

/// Decodes one fixture through the XML whole route, returning the document
/// and its root handle.
fn decode(bytes: Vec<u8>) -> Result<(&'static jqf_data::Document<'static>, jqf_data::NodeHandle), String> {
    let mut resources = resources();
    let registration = jqf_codec_xml::registration().map_err(|error| format!("{error:?}"))?;
    // The product and the document borrow the source bytes, so the fixture is
    // leaked with the product: the harness runs each fixture once and the
    // leak is a bounded, process-lifetime allocation.
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(9), SourceKind::Input),
        "fixture.xml",
        bytes,
        0,
    );
    let mut provider = registration
        .decoder()
        .expect("xml decoder factory")
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new("rfc8259").expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {error:?}"))?;
    let mut demand = CodecDemand::try_new(&resources);
    demand
        .try_insert(&DemandClause::SemanticRoot)
        .map_err(|error| format!("demand root: {error:?}"))?;
    demand
        .try_insert(&DemandClause::ValueShape)
        .map_err(|error| format!("demand shape: {error:?}"))?;
    let requirement = AccessRequirement::try_whole(
        demand,
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .map_err(|error| format!("requirement: {error:?}"))?;
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {error:?}"))?;
    let product = {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).map_err(|error| format!("decode: {error:?}"))?;
        let (outcome, _report) = result.into_parts();
        let AccessOutcome::FullDocument(product) = outcome else {
            return Err("expected full document".into());
        };
        product
    };
    // The product and the document borrow one another, so the document is
    // handed out through a leaked product: the harness runs each fixture once
    // and the leak is deliberate (a bounded, process-lifetime allocation).
    let product: &'static _ = Box::leak(Box::new(product));
    let document = product.document();
    let root = document.root_handle();
    Ok((document, root))
}

/// Decodes one `#XX` byte-escape the libxml2 dump writes for non-ASCII
/// names.
fn decode_escapes(name: &str) -> String {
    let mut out = String::new();
    let mut bytes = Vec::new();
    let mut cursor = 0;
    while cursor < name.len() {
        let rest = &name[cursor..];
        if let Some(hex) = rest.strip_prefix('#') {
            let hex: String = hex.chars().take(2).collect();
            if hex.len() == 2 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
                bytes.push(u8::from_str_radix(&hex, 16).expect("hex"));
                cursor += 3;
                continue;
            }
        }
        if !bytes.is_empty() {
            out.push_str(&String::from_utf8_lossy(&bytes));
            bytes.clear();
        }
        out.push(rest.chars().next().expect("non-empty rest"));
        cursor += 1;
    }
    if !bytes.is_empty() {
        out.push_str(&String::from_utf8_lossy(&bytes));
    }
    out
}

/// Parses one result file into `(expression, top-level entries)` pairs.
///
/// A block starts at `Expression: <text>`; the top-level selected nodes are
/// the `N  KIND ...` lines at depth 1 (one leading space, a number, two
/// spaces). Nested dump lines are indented deeper.
fn parse_result(text: &str) -> Vec<(String, Vec<String>)> {
    let mut blocks: Vec<(String, Vec<String>)> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;
    for line in text.lines() {
        if let Some(expression) = line.strip_prefix("Expression: ") {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = Some((expression.to_owned(), Vec::new()));
            continue;
        }
        let Some((_, entries)) = current.as_mut() else {
            continue;
        };
        // A top-level selected node: digits, two spaces, the kind (`1  ELEMENT
        // EXAMPLE`, or the document node's `1   /`). Nested dump lines are
        // indented and never start with digits.
        let bytes = line.as_bytes();
        let mut at = 0;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == 0 || bytes.get(at..at + 2) != Some(b"  ") {
            continue;
        }
        at += 2;
        entries.push(line[at..].to_owned());
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }
    blocks
}

/// The selected elements' names from one oracle block, in dump order.
///
/// The `xml.xpath@1` result law restricts selections to ELEMENTS; a block
/// whose dump selects the document node (`/`), text, comments, CDATA, or PIs
/// is compared on its element entries alone (the element-filtered law, part
/// of the profile, not a harness convenience).
fn oracle_elements(entries: &[String]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| entry.strip_prefix("ELEMENT "))
        .map(|name| {
            let name = name.trim_end();
            decode_escapes(name)
        })
        .collect()
}

/// One suite file: its fixture, expressions, and oracle.
struct SuiteFile {
    fixture: Vec<u8>,
    cases: Vec<(String, Vec<String>)>,
}

fn load_suite(corpus: &Path, name: &str, fixture_name: &str) -> Result<SuiteFile, String> {
    let fixture =
        std::fs::read(corpus.join("docs").join(fixture_name)).map_err(|error| format!("{name}: fixture: {error}"))?;
    let expressions =
        std::fs::read_to_string(corpus.join("tests").join(name)).map_err(|error| format!("{name}: tests: {error}"))?;
    let result = std::fs::read_to_string(corpus.join("result").join(name))
        .map_err(|error| format!("{name}: result: {error}"))?;
    let cases = parse_result(&result);
    let expressions: Vec<String> = expressions
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    if expressions.len() != cases.len() {
        return Err(format!(
            "{name}: {} expressions but {} oracle blocks",
            expressions.len(),
            cases.len()
        ));
    }
    for (expression, (expected, _)) in expressions.iter().zip(&cases) {
        if expression != expected {
            return Err(format!(
                "{name}: expression {expression:?} does not match oracle header {expected:?}"
            ));
        }
    }
    Ok(SuiteFile { fixture, cases })
}

/// The in-profile case count, PINNED rather than self-consistent: the old
/// check (in_profile == pass && in_profile + out_profile == total) passes
/// when a compile break silently drops in-profile 34 -> 33, because
/// every dropped case stays internally consistent. A deliberate suite
/// change updates this pin in the same commit.
const PINNED_IN_PROFILE: usize = 34;

fn main() {
    let corpus = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("tools/jqf-xpath-conformance/corpus"), PathBuf::from);
    // (suite file, fixture doc) pairs, following libxml2's own testXPath
    // mapping: several expression files run over the same document.
    let suites: &[(&str, &str)] = &[
        ("chaptersbase", "chapters"),
        ("chaptersprefol", "chapters"),
        ("idsimple", "id"),
        ("langsimple", "lang"),
        ("mixedpat", "mixed"),
        ("nodespat", "nodes"),
        ("nssimple", "ns"),
        ("simpleabbr", "simple"),
        ("simplebase", "simple"),
        ("strbase", "str"),
        ("unicodesimple", "unicode"),
        ("usr1check", "usr1"),
        ("vidbase", "vid"),
    ];
    let mut total = 0usize;
    let mut in_profile = 0usize;
    let mut out_profile = 0usize;
    let mut pass = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (name, fixture_name) in suites {
        let suite = match load_suite(&corpus, name, fixture_name) {
            Ok(suite) => suite,
            Err(error) => {
                println!("xpath-conformance: FAIL: {error}");
                std::process::exit(1);
            }
        };
        let (document, root) = match decode(suite.fixture.clone()) {
            Ok(value) => value,
            Err(error) => {
                println!("xpath-conformance: FAIL: {name}: fixture decode: {error}");
                std::process::exit(1);
            }
        };
        for (expression, entries) in &suite.cases {
            total += 1;
            let compiled =
                jqf_builtins::selector::compile(jqf_builtins::selector::SelectorLanguage::XmlXPath1, expression);
            let Ok(compiled) = compiled else {
                // The closed grammar's boundary is conformance: the suite's
                // out-of-profile expressions must keep failing to compile.
                out_profile += 1;
                continue;
            };
            in_profile += 1;
            let mut resources = resources();
            let selected = match jqf_builtins::selector::select(
                &document,
                root,
                &compiled,
                SelectorBudget::default(),
                &mut resources,
            ) {
                Ok(jqf_builtins::selector::SelectorResult::Elements(selected)) => selected,
                Ok(jqf_builtins::selector::SelectorResult::Scalar(_)) => {
                    // A top-level function result is a scalar, which the
                    // element-dump oracle cannot compare; the vendored suite
                    // has no such expression, so this is a harness contract
                    // guard.
                    failures.push(format!(
                        "{name}: {expression:?} produced a scalar result the element oracle cannot compare"
                    ));
                    continue;
                }
                Err(error) => {
                    failures.push(format!("{name}: {expression:?} failed at run time: {error:?}"));
                    continue;
                }
            };
            let mut actual = Vec::new();
            for node in selected {
                let mut reader = match document.fact_reader(&mut resources) {
                    Ok(reader) => reader,
                    Err(error) => {
                        failures.push(format!("{name}: {expression:?}: fact reader: {error:?}"));
                        break;
                    }
                };
                let owner = jqf_data::LocalOwnerRef::Node(node);
                let limit = jqf_data::BatchLimit::new(usize::MAX).expect("batch limit");
                loop {
                    match reader.poll_batch(limit, &mut resources) {
                        Ok(jqf_data::ReaderPoll::Batch(batch)) => {
                            for fact in batch.iter() {
                                if fact.owner() == owner && fact.role().as_str() == jqf_codec_core::markup::NAME_FACT {
                                    if let jqf_data::FactPayloadView::Text(text) = fact.payload() {
                                        actual.push(text.to_owned());
                                    }
                                }
                            }
                        }
                        Ok(jqf_data::ReaderPoll::Pending) => {
                            resources.try_begin_next_cooperative_entry(4_096).expect("resume");
                        }
                        Ok(jqf_data::ReaderPoll::End(_)) => break,
                        Err(error) => {
                            failures.push(format!("{name}: {expression:?}: facts: {error:?}"));
                            break;
                        }
                    }
                }
            }
            let expected = oracle_elements(entries);
            if actual == expected {
                pass += 1;
            } else {
                failures.push(format!(
                    "{name}: {expression:?} selected {actual:?}, oracle elements {expected:?}"
                ));
            }
        }
    }
    println!(
        "xpath-conformance: total={total} in-profile={in_profile} out-profile={out_profile} \
         pass={pass} fail={}",
        failures.len()
    );
    for failure in &failures {
        println!("xpath-conformance:   {failure}");
    }
    if !failures.is_empty()
        || in_profile != pass
        || in_profile + out_profile != total
        || in_profile != PINNED_IN_PROFILE
    {
        eprintln!(
            "xpath-conformance: receipt mismatch (in-profile={in_profile} pinned={PINNED_IN_PROFILE} pass={pass} total={total})"
        );
        std::process::exit(1);
    }
    println!("xpath-conformance: all receipts pass");
}
