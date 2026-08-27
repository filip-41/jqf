#![allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::collapsible_if,
    clippy::format_in_format_args,
    clippy::format_push_string,
    clippy::manual_unwrap_or,
    clippy::redundant_closure_for_method_calls,
    clippy::default_trait_access,
    clippy::uninlined_format_args,
    clippy::only_used_in_recursion,
    clippy::doc_markdown,
    clippy::needless_return,
    clippy::map_unwrap_or,
    clippy::single_match_else,
    reason = "the receipt runner mirrors the corpus law in long sequential receipts"
)]

//! The WHATWG tokenizer conformance runner over the vendored html5lib-tests
//! tokenizer suite (`jqf-codec/html/corpus/tokenizer`, pinned there).
//!
//! The suite's expected stream includes `ParseError` tokens; jqf models
//! parse errors as diagnostics, not stream tokens, so the comparison skips
//! them and matches the VALUE tokens in order. Receipt line:
//!
//! ```text
//! tokenizer-conformance: total=N pass=P fail=F
//! ```
//!
//! Any failure exits 1.

use jqf_codec_html::tokenizer_core::{self, Token, TokenKind};

/// Maps an html5lib `initialStates` name to a tokenizer state.
fn initial_state(name: &str) -> Option<tokenizer_core::InitialState> {
    match name {
        "Data state" => Some(tokenizer_core::InitialState::Data),
        "RCDATA state" => Some(tokenizer_core::InitialState::Rcdata),
        "RAWTEXT state" => Some(tokenizer_core::InitialState::Rawtext),
        "Script data state" => Some(tokenizer_core::InitialState::ScriptData),
        "PLAINTEXT state" => Some(tokenizer_core::InitialState::Plaintext),
        "CDATA section state" => Some(tokenizer_core::InitialState::CdataSection),
        _ => None,
    }
}

/// One normalized comparison entry: `("character", data)` for text (merged
/// across adjacent runs) and `(kind, rendered_json)` for everything else.
type Entry = (String, Option<String>);

/// Renders one token into a comparison entry.
fn render(token: &Token) -> Option<Entry> {
    use serde_json::{Value, json};
    let (kind, value): (String, Value) = match &token.kind {
        TokenKind::Character { data } => {
            return Some(("character".to_owned(), Some(data.clone())));
        }
        TokenKind::StartTag {
            name,
            attributes,
            self_closing,
        } => {
            let mut map = serde_json::Map::new();
            for attribute in attributes {
                map.insert(attribute.name.clone(), Value::String(attribute.value.clone()));
            }
            if *self_closing {
                ("start".to_owned(), json!(["StartTag", name, map, true]))
            } else {
                ("start".to_owned(), json!(["StartTag", name, map]))
            }
        }
        TokenKind::EndTag { name } => ("end".to_owned(), json!(["EndTag", name])),
        TokenKind::Comment { data } => ("comment".to_owned(), json!(["Comment", data])),
        TokenKind::Doctype {
            name,
            public_identifier,
            system_identifier,
            force_quirks: _,
            correct,
        } => (
            "doctype".to_owned(),
            json!(["DOCTYPE", name, public_identifier, system_identifier, correct]),
        ),
        TokenKind::Eof => return None,
    };
    Some((kind, Some(value.to_string())))
}

/// Merges adjacent character entries: the WHATWG tokenizer's emission
/// granularity for text is per-consumed-char and the suite's own expected
/// streams merge runs non-deterministically, so both sides normalize to the
/// merged form (the tree builder's own law).
fn merge_characters(entries: Vec<Entry>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for (kind, data) in entries {
        if kind == "character" {
            if let Some((last_kind, Some(previous))) = out.last_mut() {
                if last_kind == "character" {
                    if let Some(next) = data {
                        previous.push_str(&next);
                        continue;
                    }
                }
            }
        }
        out.push((kind, data));
    }
    out
}

/// A lenient JSON-string unescape: the html5lib `doubleEscaped` inputs may
/// contain lone surrogate escapes, which Python's json accepts and the
/// tokenizer replaces with U+FFFD. Valid surrogate pairs combine.
fn unescape_json_string(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                let first = u32::from_str_radix(&hex, 16).expect("hex escape");
                let scalar = if (0xD800..=0xDBFF).contains(&first) {
                    // A high surrogate: try to combine with a following
                    // `\uXXXX` low surrogate.
                    let mut next_hex = String::new();
                    let mut saved: Vec<char> = Vec::new();
                    let mut combined = None;
                    let mut clone = chars.clone();
                    if clone.next() == Some('\\') && clone.next() == Some('u') {
                        for _ in 0..4 {
                            if let Some(digit) = clone.next() {
                                next_hex.push(digit);
                                saved.push(digit);
                            }
                        }
                        let second = u32::from_str_radix(&next_hex, 16).ok();
                        if let Some(second) = second {
                            if (0xDC00..=0xDFFF).contains(&second) {
                                let point = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                                combined = char::from_u32(point);
                                for _ in 0..6 {
                                    chars.next();
                                }
                            }
                        }
                    }
                    match combined {
                        Some(point) => point,
                        None => {
                            // Lone surrogate: the tokenizer's replacement.
                            '\u{FFFD}'
                        }
                    }
                } else {
                    char::from_u32(first).unwrap_or('\u{FFFD}')
                };
                out.push(scalar);
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// xmlViolation.test cases whose expected output still follows the old
/// noncharacter-replacement / FF-as-space / comment-rewrite oracles. Case
/// "FF between attributes" is current law (FF is ASCII whitespace) and runs.
const STALE_XML_VIOLATION: &[&str] = &["Non-XML character", "Non-XML space", "Double hyphen in comment"];

fn main() {
    // The default corpus path resolves from this crate's manifest, so the
    // runner works from any working directory; an explicit argument still
    // wins.
    let corpus = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../jqf-codec/html/corpus/tokenizer")
        });
    let mut total = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&corpus).expect("corpus dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("test") {
            continue;
        }
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let text = std::fs::read_to_string(&path).expect("test file");
        let document: serde_json::Value = serde_json::from_str(&text).expect("test json");
        let tests = document["tests"]
            .as_array()
            .or_else(|| document["xmlViolationTests"].as_array())
            .expect("tests array");
        for (index, test) in tests.iter().enumerate() {
            let description = test
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if file_name == "xmlViolation.test" && STALE_XML_VIOLATION.contains(&description) {
                skipped += 1;
                continue;
            }
            let input = test["input"].as_str().expect("input").to_owned();
            let output = test["output"].as_array().expect("output");
            let mut raw_input = input.clone();
            let double_escaped = test.get("doubleEscaped").and_then(serde_json::Value::as_bool) == Some(true);
            if double_escaped {
                raw_input = unescape_json_string(&raw_input);
            }
            let states: Vec<&str> = test
                .get("initialStates")
                .and_then(|states| states.as_array())
                .map(|states| states.iter().filter_map(|state| state.as_str()).collect())
                .unwrap_or_default();
            let state_names: Vec<&str> = if states.is_empty() { vec!["Data state"] } else { states };
            let last_start_tag = test
                .get("lastStartTag")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let mut expected_entries: Vec<Entry> = Vec::new();
            for token in output {
                let is_error = token.as_str() == Some("ParseError")
                    || token.as_array().and_then(|a| a.first()).and_then(|k| k.as_str()) == Some("ParseError");
                if is_error {
                    continue;
                }
                let kind = match token.as_array().and_then(|a| a.first()).and_then(|k| k.as_str()) {
                    Some("Character") => "character",
                    Some("StartTag") => "start",
                    Some("EndTag") => "end",
                    Some("Comment") => "comment",
                    Some("DOCTYPE") => "doctype",
                    _ => "other",
                };
                if kind == "character" {
                    let data = token
                        .as_array()
                        .and_then(|a| a.get(1))
                        .and_then(serde_json::Value::as_str)
                        .expect("character data")
                        .to_owned();
                    let data = if double_escaped {
                        unescape_json_string(&data)
                    } else {
                        data
                    };
                    expected_entries.push((kind.to_owned(), Some(data)));
                } else {
                    expected_entries.push((kind.to_owned(), Some(token.to_string())));
                }
            }
            let expected = merge_characters(expected_entries);
            for state_name in state_names {
                let Some(state) = initial_state(state_name) else {
                    failures.push(format!("{file_name}[{index}] unmapped initialState {state_name:?}"));
                    continue;
                };
                total += 1;
                let mut tokenizer =
                    tokenizer_core::Tokenizer::with_initial_state(raw_input.clone(), state, last_start_tag.as_deref());
                tokenizer.set_foreign_content(true);
                let mut actual_entries: Vec<Entry> = Vec::new();
                while let Some(token) = tokenizer.next_token() {
                    if let Some(entry) = render(&token) {
                        actual_entries.push(entry);
                    }
                }
                let actual = merge_characters(actual_entries);
                if actual != expected {
                    failures.push(format!(
                        "{file_name}[{index}] state={state_name:?} input={input:?}\n  expected: {expected:?}\n  actual:   {actual:?}"
                    ));
                }
            }
        }
    }
    println!(
        "tokenizer-conformance: total={total} pass={} fail={} skipped={skipped}",
        total.saturating_sub(failures.len()),
        failures.len()
    );
    for failure in failures.iter().take(20) {
        println!("tokenizer-conformance:   {failure}");
    }
    if total == 0 {
        eprintln!("tokenizer-conformance: no cases");
        std::process::exit(1);
    }
    if !failures.is_empty() {
        std::process::exit(1);
    }
    println!("tokenizer-conformance: all receipts pass");
}
