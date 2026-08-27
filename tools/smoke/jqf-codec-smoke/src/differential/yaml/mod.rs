//! YAML differential decode oracle: jqf's YAML route versus the vendored
//! yaml-test-suite corpus's own `json:` canonical projections.
//!
//! Every corpus case is decoded through jqf's whole-document route and
//! compared against the corpus's expectation:
//!
//! - `fail: true` — jqf must REJECT the input (error kinds are not compared
//!   against the corpus; the corpus's own `error:`/`tree:` fields describe
//!   the reference implementation's event stream, which is not jqf's law).
//! - `json:` present — jqf must ACCEPT and its decoded value's semantic
//!   checksum must equal the checksum of the corpus's JSON projection.
//! - `json: ''` (empty stream/comment-only cases) — the corpus oracle is
//!   ZERO items. The whole-document route publishes a core null; that split
//!   is allowlisted (`empty-stream-null`).
//!
//! Every case that accepts also round-trips through the canonical renderer:
//! decode → encode → decode must preserve the semantic checksum. This is the
//! encode oracle the corpus's `dump:` fields could provide, exercised as
//! self-identity instead (the canonical renderer is jqf's own profile and
//! does not claim source identity; byte-comparing it to the corpus's `dump:`
//! fields would compare two different profiles).
//!
//! The receipt is `cases=… accepted=… rejected=… round_trip_drift=0
//! divergences=… allowlisted=… unwaived=0` — the GATE is `unwaived=0`.
//!
//! The ALLOWLIST (below) records every divergence that is BY DESIGN:
//! (1) the §4.8 key law — complex/anchored/alias/tagged/empty mapping keys
//! are never coerced to object keys; (2) the v1 single-document route;
//! (3) reference disagreements — libyaml rejects (or resolves to different
//! bytes than the corpus) and jqf matches libyaml; (4) corpus `fail:true`
//! rows that libyaml AND jqf both accept; (5) spec-bias byte rows (EOF clip
//! line breaks, `?` in flow keys). The stale-entry rule applies: an entry
//! whose case stops diverging fails the gate, so a fix cannot leave its
//! waiver behind.
//!
//! `serde_yaml` here is the CORPUS METADATA reader (the corpus files are
//! YAML), used only to extract each case's `yaml:`/`json:`/`fail` fields —
//! the comparator-in-tool-crate exception the hygiene law allows. jqf's own
//! decode is never compared against `serde_yaml`'s.

mod semantic;

use std::path::Path;

use jqf_codec_core::{CodecFailureKind, CodecRunContext, DiagnosticPolicy};
use jqf_data::Value;
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const CORPUS: &str = "corpus/yaml-test-suite/src";

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

/// One corpus case's decoded outcome through jqf.
#[derive(Debug)]
enum Outcome {
    /// Accepted; the payload is the semantic checksums of the materialized
    /// roots, one per document in the stream (a single-document stream has
    /// one), and the round-trip checksums when the canonical re-encode also
    /// decodes (both must agree per document).
    Accept {
        checksums: Vec<u64>,
        round_trip_checksums: Option<Vec<u64>>,
    },
    /// Rejected; the payload is a short diagnosis.
    Reject(String),
}

/// Decodes one YAML stream, collecting one value per document (the codec's
/// multi-yield whole-document session: each document is one ordered
/// unit-stream item per §4.8).
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<Value>, CodecFailureKind> {
    let mut resources = resources();
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "differential.yaml",
        bytes,
        0,
    );
    jqf_codec_yaml::decode_documents(source, &mut resources).map_err(|error| error.kind())
}

/// The canonical re-encoder: owned value -> stream-canonical YAML bytes.
fn encode_yaml(value: &Value) -> Result<Vec<u8>, CodecFailureKind> {
    let registration = jqf_codec_yaml::registration().map_err(|_| CodecFailureKind::InternalContractViolation {
        contract: "registration",
    })?;
    let mut resources = resources();
    let options = jqf_codec_yaml::YamlTargetSchema::Core;
    let format =
        jqf_data::FormatId::try_new(jqf_codec_yaml::FORMAT_ID).map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let dialect = jqf_data::DialectId::try_new(jqf_codec_yaml::YAML_STREAM_CANONICAL_DIALECT_ID)
        .map_err(|_| CodecFailureKind::RequirementMismatch)?;
    let request = jqf_codec_core::EncodeRequest {
        format: &format,
        dialect: &dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: jqf_codec_core::PreservationRequest::None,
        options: Some(&options as &(dyn core::any::Any + Send + Sync)),
    };
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(request, &mut resources)
        .map_err(|error| error.kind())?;
    let mut session = factory
        .start(
            jqf_codec_core::EncodeItem::Owned(value),
            jqf_codec_core::PreservationRequest::None,
            &mut resources,
        )
        .map_err(|error| error.kind())?;
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut context = CodecRunContext::new(&mut resources);
        context.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut context).map_err(|error| error.kind())?;
    }
    Ok(out)
}

/// Runs one corpus case through jqf and classifies it.
fn exercise_case(yaml: &[u8]) -> Outcome {
    match decode(yaml) {
        Err(reason) => Outcome::Reject(format!("{reason:?}")),
        Ok(values) => {
            let checksums: Vec<u64> = values.iter().map(semantic::jqf_value).collect();
            // Round-trip each document through the canonical re-encoder and
            // re-decode; every document's round-trip must decode back to the
            // same checksum.
            let mut round_trip: Option<Vec<u64>> = Some(Vec::new());
            for value in &values {
                match encode_yaml(value) {
                    Err(_reason) => {
                        round_trip = None;
                        break;
                    }
                    Ok(encoded) => match decode(&encoded) {
                        Err(reason) => {
                            eprintln!(
                                "ROUNDTRIP-DEBUG input={:?}\nencoded={:?}\nerror={reason:?}",
                                String::from_utf8_lossy(yaml),
                                String::from_utf8_lossy(&encoded)
                            );
                            round_trip = None;
                            break;
                        }
                        Ok(second) => {
                            let r = round_trip.as_mut().expect("round_trip Some");
                            if second.len() != 1 {
                                round_trip = None;
                                break;
                            }
                            r.push(semantic::jqf_value(&second[0]));
                        }
                    },
                }
            }
            Outcome::Accept {
                checksums,
                round_trip_checksums: round_trip,
            }
        }
    }
}

/// Decodes the corpus's visible-notation escapes into their real bytes
/// (upstream `bin/YAMLTestSuite.pm` `unescape`).
fn unescape_corpus(text: &str) -> Vec<u8> {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '␣' => {
                out.push(' ');
                index += 1;
            }
            '—' | '»' => {
                // The upstream unescape is `s/—*»/\t/g`: ZERO or more
                // em-dashes followed by a guillemet is ONE tab, so a lone
                // `»` is also a tab. Mirror it exactly.
                while index < chars.len() && chars[index] == '—' {
                    index += 1;
                }
                if index < chars.len() && chars[index] == '»' {
                    index += 1;
                }
                out.push('\t');
            }
            '←' => {
                out.push('\r');
                index += 1;
            }
            '⇔' => {
                out.push('\u{FEFF}');
                index += 1;
            }
            '↵' => {
                index += 1;
            }
            '∎' => {
                // A trailing `∎\n` marks the end of the block; drop the
                // marker and its newline.
                index += 1;
                if index < chars.len() && chars[index] == '\n' {
                    index += 1;
                }
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    out.into_bytes()
}

/// One allowlisted corpus divergence: jqf's behavior is BY DESIGN here
/// (or matches the reference implementation where the corpus's spec reading
/// differs). Keyed by (file id, exact unescaped input).
///
/// The stale-entry rule (the jq-suite ALLOWLIST precedent): an entry whose
/// case stops diverging FAILS the gate, so a fix cannot leave its waiver
/// behind. A divergence NOT in this table fails the gate.
struct AllowlistEntry {
    id: &'static str,
    input: &'static [u8],
    category: &'static str,
}

const ALLOWLIST: &[AllowlistEntry] = &[
        AllowlistEntry { id: "26DV", input: b"\"top1\" : \n  \"key1\" : &alias1 scalar1\n'top2' : \n  'key2' : &alias2 scalar2\ntop3: &node3 \n  *alias1 : scalar3\ntop4: \n  *alias2 : scalar4\ntop5   :    \n  scalar5\ntop6: \n  &anchor6 'key6' : scalar6\n", category: "complex-key" },
        AllowlistEntry { id: "2JQS", input: b": a\n: b\n", category: "libyaml-disagreement" },
        AllowlistEntry { id: "4FJ6", input: b"---\n[\n  [ a, [ [[b,c]]: d, e]]: 23\n]\n", category: "complex-key" },
        AllowlistEntry { id: "6BFJ", input: b"---\n&mapping\n&key [ &item a, b, c ]: value\n", category: "complex-key" },
        AllowlistEntry { id: "6M2F", input: b"? &a a\n: &b b\n: *a\n", category: "libyaml-disagreement" },
        AllowlistEntry { id: "6PBE", input: b"---\n?\n- a\n- b\n:\n- c\n- d\n", category: "complex-key" },
        AllowlistEntry { id: "9MMW", input: b"- [ YAML : separate ]\n- [ \"JSON like\":adjacent ]\n- [ {JSON: like}:adjacent ]\n", category: "complex-key" },
        AllowlistEntry { id: "E76Z", input: b"&a a: &b b\n*b : *a\n", category: "complex-key" },
        AllowlistEntry { id: "FH7J", input: b"- !!str\n-\n  !!null : a\n  b: !!str\n- !!str : !!null\n", category: "complex-key" },
        AllowlistEntry { id: "KK5P", input: b"complex1:\n  ? - a\ncomplex2:\n  ? - a\n  : b\ncomplex3:\n  ? - a\n  : >\n    b\ncomplex4:\n  ? >\n    a\n  :\ncomplex5:\n  ? - a\n  : - b\n", category: "complex-key" },
        AllowlistEntry { id: "LX3P", input: b"[flow]: block\n", category: "complex-key" },
        AllowlistEntry { id: "L24T", input: b"foo: |\n  x\n   ", category: "spec-bytes" },
        AllowlistEntry { id: "M2N8", input: b"- ? : x\n", category: "libyaml-disagreement" },
        AllowlistEntry { id: "M2N8", input: b"? []: x\n", category: "complex-key" },
        AllowlistEntry { id: "M5DY", input: b"? - Detroit Tigers\n  - Chicago cubs\n:\n  - 2001-07-23\n\n? [ New York Yankees,\n    Atlanta Braves ]\n: [ 2001-07-02, 2001-08-12,\n    2001-08-14 ]\n", category: "complex-key" },
        AllowlistEntry { id: "NKF9", input: b"---\nkey: value\n: empty key\n---\n{\n key: value, : empty key\n}\n---\n# empty key and value\n:\n---\n# empty key and value\n{ : }\n", category: "libyaml-disagreement" },
        AllowlistEntry { id: "PW8X", input: b"- &a\n- a\n-\n  &a : a\n  b: &b\n-\n  &c : &a\n-\n  ? &d\n-\n  ? &e\n  : &a\n", category: "complex-key" },
        AllowlistEntry { id: "Q9WF", input: b"{ first: Sammy, last: Sosa }:\n# Statistics:\n  hr:  # Home runs\n     65\n  avg: # Average\n   0.278\n", category: "complex-key" },
        AllowlistEntry { id: "RZP5", input: b"a: \"double\n  quotes\" # lala\nb: plain\n value  # lala\nc  : #lala\n  d\n? # lala\n - seq1\n: # lala\n - #lala\n  seq2\ne: &node # lala\n - x: y\nblock: > # lala\n  abcde\n", category: "complex-key" },
        AllowlistEntry { id: "S98Z", input: b"empty block scalar: >\n \n  \n   \n # comment\n", category: "fail-case-disagreement" },
        AllowlistEntry { id: "SBG9", input: b"{a: [b, c], [d, e]: f}\n", category: "complex-key" },
        AllowlistEntry { id: "V9D5", input: b"- sun: yellow\n- ? earth: blue\n  : moon: white\n", category: "complex-key" },
        AllowlistEntry { id: "X38W", input: b"{ &a [a, &b b]: *b, *a : [c, *b, d]}\n", category: "complex-key" },
        AllowlistEntry { id: "XW4D", input: b"a: \"double\n  quotes\" # lala\nb: plain\n value  # lala\nc  : #lala\n  d\n? # lala\n - seq1\n: # lala\n - #lala\n  seq2\ne:\n &node # lala\n - x: y\nblock: > # lala\n  abcde\n", category: "complex-key" },
        AllowlistEntry { id: "ZYU8", input: b"%***\n---\n", category: "libyaml-disagreement" },
        AllowlistEntry { id: "ZYU8", input: b"%YAML 1.1 1.2\n---\n", category: "libyaml-disagreement" },
        AllowlistEntry { id: "8G76", input: b"  # Comment\n   \n\n\n", category: "empty-stream-null" },
        AllowlistEntry { id: "98YD", input: b"# Comment only.\n", category: "empty-stream-null" },
        AllowlistEntry { id: "AVM7", input: b"", category: "empty-stream-null" },
        AllowlistEntry { id: "HWV9", input: b"...\n", category: "empty-stream-null" },
        AllowlistEntry { id: "QT73", input: b"# comment\n...\n", category: "empty-stream-null" },
];

/// The index of the allowlist entry matching this divergence, if any.
fn allowlisted_index(id: &str, yaml: &[u8]) -> Option<usize> {
    ALLOWLIST.iter().position(|entry| entry.id == id && entry.input == yaml)
}

/// Records a divergence: allowlisted entries (with a reason) are
/// counted separately and do not fail the gate.
fn push_divergence(
    divergences: &mut Vec<String>,
    allowlisted_count: &mut u64,
    allowlist_fired: &mut [bool],
    text: &str,
    name: &str,
    yaml: &[u8],
) {
    let id = name.split(':').next().unwrap_or(name);
    match allowlisted_index(id, yaml) {
        Some(index) => {
            allowlist_fired[index] = true;
            *allowlisted_count += 1;
            divergences.push(format!(
                "ALLOWLISTED[{}] {text} (by design; stale-entry rule: retire the row if this stops diverging)",
                ALLOWLIST[index].category
            ));
        }
        None => divergences.push(text.to_owned()),
    }
}

/// One extracted corpus record: (name, fail flag, skip flag, unescaped
/// input, parsed json oracle — `None` when absent, one value for a
/// single-document oracle, several for a multi-document stream). A
/// `skip: true` record is the corpus author's own "do not test" marker
/// (spec-valid but discouraged productions), honored exactly as a real
/// yaml-test-suite runner would.
type Case = (String, bool, bool, Vec<u8>, Option<Vec<Option<serde_json::Value>>>);

#[allow(
    clippy::too_many_lines,
    reason = "one case loop: extraction, classification, allowlist, receipt"
)]
pub(crate) fn run() -> Result<(), String> {
    let corpus_dir = Path::new(ROOT).join(CORPUS);
    let mut cases: Vec<Case> = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&corpus_dir)
        .expect("corpus directory")
        .filter_map(std::result::Result::ok)
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("corpus file");
        // The corpus files are themselves YAML; parse the metadata with
        // serde_yaml (the metadata reader — the comparator-in-tool-crate
        // exception, never compared against jqf's decode).
        let meta: serde_yaml::Value = serde_yaml::from_str(&text).expect("corpus metadata");
        let serde_yaml::Value::Sequence(records) = meta else {
            continue;
        };
        for record in records {
            let fail = record.get("fail").and_then(serde_yaml::Value::as_bool).unwrap_or(false);
            let yaml = record
                .get("yaml")
                .and_then(serde_yaml::Value::as_str)
                .map(unescape_corpus);
            // The `json:` oracle is a scalar string (a block scalar holding
            // the JSON text), the empty string for the no-projection cases.
            // Convert to serde_json at parse time so the comparison loop is
            // typed. A multi-document stream's oracle is one JSON value per
            // line (`"fluorescent"\n"green"`); parse each line.
            let json = record.get("json").cloned().and_then(|value| match value {
                serde_yaml::Value::String(text) if text.is_empty() => Some(Vec::new()),
                serde_yaml::Value::String(text) => {
                    // A single-document oracle may be pretty-printed over
                    // several lines; parse the WHOLE text first. Only when it
                    // is not one JSON value do we treat it as a multi-
                    // document oracle: one JSON text per line
                    // (`"fluorescent"\n"green"`).
                    if let Ok(value) = serde_json::from_str(&text) {
                        Some(vec![Some(value)])
                    } else {
                        let mut values = Vec::new();
                        for line in text.lines().filter(|line| !line.trim().is_empty()) {
                            values.push(serde_json::from_str(line).ok());
                        }
                        if values.is_empty() { None } else { Some(values) }
                    }
                }
                _ => None,
            });
            let name = record
                .get("name")
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or("(unnamed)")
                .to_owned();
            if let Some(yaml) = yaml {
                let id = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("?")
                    .to_owned();
                let skip = record.get("skip").and_then(serde_yaml::Value::as_bool).unwrap_or(false);
                cases.push((format!("{id}: {name}"), fail, skip, yaml, json));
            }
        }
    }

    let mut divergences = Vec::new();
    let mut allowlist_tally = 0_u64;
    let mut allowlist_fired = vec![false; ALLOWLIST.len()];
    let mut accepted = 0_u64;
    let mut rejected = 0_u64;
    let mut round_trip_drift = 0_u64;

    for (name, fail, skip, yaml, json) in &cases {
        let fail = *fail;
        if *skip {
            // The corpus author's `skip: true` marker: do not test.
            continue;
        }
        let outcome = exercise_case(yaml);
        match &outcome {
            Outcome::Reject(reason) => {
                rejected += 1;
                let _ = reason;
                if !fail {
                    let text = format!(
                        "{name}: expected an accept (json oracle present), jqf rejected input={:?}",
                        String::from_utf8_lossy(yaml)
                    );
                    push_divergence(
                        &mut divergences,
                        &mut allowlist_tally,
                        &mut allowlist_fired,
                        &text,
                        name,
                        yaml,
                    );
                }
            }
            Outcome::Accept {
                checksums,
                round_trip_checksums,
            } => {
                accepted += 1;
                if fail {
                    let text = format!(
                        "{name}: expected a reject (fail: true), jqf accepted with checksums {:?} input={:?}",
                        checksums,
                        String::from_utf8_lossy(yaml)
                    );
                    push_divergence(
                        &mut divergences,
                        &mut allowlist_tally,
                        &mut allowlist_fired,
                        &text,
                        name,
                        yaml,
                    );
                    continue;
                }
                if let Some(oracle) = json {
                    // An oracle that is all `Some` is compared; an oracle with
                    // a `None` (a json value that failed to parse) is treated
                    // as absent.
                    let all_some = oracle.iter().all(Option::is_some);
                    if all_some {
                        let oracle_checksums: Vec<u64> = oracle
                            .iter()
                            .map(|value| semantic::serde_value(value.as_ref().expect("all_some")))
                            .collect();
                        if checksums.len() != oracle_checksums.len()
                            || checksums.iter().zip(oracle_checksums.iter()).any(|(a, b)| a != b)
                        {
                            let text = format!(
                                "{name}: checksum mismatch jqf={checksums:?} oracle={oracle_checksums:?} input={:?}",
                                String::from_utf8_lossy(yaml)
                            );
                            push_divergence(
                                &mut divergences,
                                &mut allowlist_tally,
                                &mut allowlist_fired,
                                &text,
                                name,
                                yaml,
                            );
                        }
                    }
                }
                if let Some(round_trip) = round_trip_checksums {
                    if round_trip.len() != checksums.len()
                        || round_trip.iter().zip(checksums.iter()).any(|(a, b)| a != b)
                    {
                        round_trip_drift += 1;
                        divergences.push(format!(
                            "{name}: round-trip checksum drift {checksums:?} -> {round_trip:?}"
                        ));
                    }
                } else {
                    divergences.push(format!("{name}: accepted but the canonical re-encode did not decode"));
                }
            }
        }
    }

    // The gate: an UN-ALLOWLISTED divergence fails. Allowlisted divergences
    // are by design (their reasons are printed beside them); a STALE
    // allowlist entry — one whose case exists but stopped diverging, or
    // whose case was deleted from the corpus — FAILS the gate, so a fix
    // cannot leave its waiver behind (the jq-suite stale-entry law, applied
    // to this lane directly; the old comment's claim that sdk-smoke checks
    // it was false).
    let mut allowlist_stale = Vec::new();
    for (index, entry) in ALLOWLIST.iter().enumerate() {
        if allowlist_fired[index] {
            continue;
        }
        // An entry whose case is `skip: true` in the corpus is never
        // exercised and so never fires — that is not staleness. Every other
        // unfired entry is stale: either its case exists but stopped
        // diverging, or its case was deleted from the corpus (the row must
        // be retired in the same commit as the deletion).
        let case_is_skipped = cases.iter().any(|(name, _fail, skip, yaml, _)| {
            *skip && name.split(':').next().unwrap_or(name) == entry.id && yaml.as_slice() == entry.input
        });
        if !case_is_skipped {
            let case_exists = cases.iter().any(|(name, _fail, skip, yaml, _)| {
                !*skip && name.split(':').next().unwrap_or(name) == entry.id && yaml.as_slice() == entry.input
            });
            allowlist_stale.push(format!(
                "ALLOWLIST[{}] ({}) never fired: the case {}— retire the row",
                entry.id,
                entry.category,
                if case_exists {
                    "stopped diverging"
                } else {
                    "is missing from the corpus"
                }
            ));
        }
    }
    let unwaived = divergences
        .iter()
        .filter(|divergence| !divergence.starts_with("ALLOWLISTED["))
        .count()
        + allowlist_stale.len();
    println!(
        "jqf-codec-smoke yaml-differential: cases={} accepted={accepted} rejected={rejected} round_trip_drift={round_trip_drift} divergences={} allowlisted={allowlist_tally} unwaived={unwaived}",
        cases.len(),
        divergences.len()
    );
    for stale_row in &allowlist_stale {
        println!("  {stale_row}");
    }
    println!(
        "  accepted={accepted} rejected={rejected} round_trip_drift={round_trip_drift} divergences={} allowlisted={allowlist_tally}",
        divergences.len()
    );
    for divergence in divergences.iter().take(200) {
        println!("  DIVERGENCE {divergence}");
    }
    if unwaived != 0 {
        return Err(format!("unwaived={unwaived}"));
    }
    Ok(())
}
