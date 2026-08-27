//! The shared differential frame: exit + dispatch for seven codecs, and the
//! accept/reject checksum `Verdict` used by json/toml/cbor. Each
//! `differential/<codec>/` module owns its corpus, DECLARED table, and
//! checksum.

pub mod cbor;
pub mod csv;
pub mod html;
pub mod json;
pub mod toml;
pub mod xml;
pub mod yaml;

use std::collections::BTreeMap;

/// One decode outcome, classified for cross-implementation comparison.
///
/// Error *kinds* are deliberately erased: a differential's agreement contract
/// is accept/accept-with-equal-checksum or reject/reject, never a comparison
/// of diagnostic taxonomies between two independent implementations.
#[derive(Debug)]
pub enum Verdict {
    /// The decoder accepted the input; the payload is its semantic checksum.
    Accept(u64),
    /// The decoder rejected the input; the payload is a short diagnosis.
    Reject(String),
}

/// Renders a poll error the way every old differential binary did: the kind,
/// plus the structured diagnostic's code when one is attached.
pub fn format_poll_error(error: &jqf_codec_core::CodecError) -> String {
    if let Some(diagnostic) = error.diagnostic() {
        format!("{:?} diagnostic={}", error.kind(), diagnostic.code())
    } else {
        format!("{:?}", error.kind())
    }
}

/// The accept/reject side of a verdict, for the declared-split rows.
pub fn verdict_side(verdict: &Verdict) -> &'static str {
    match verdict {
        Verdict::Accept(_) => "Accept",
        Verdict::Reject(_) => "Reject",
    }
}

/// The expected side of a declared row: `Some(true)` is Accept, `Some(false)`
/// is Reject, `None` is "any".
pub fn side(option: Option<bool>) -> &'static str {
    match option {
        Some(true) => "Accept",
        Some(false) => "Reject",
        None => "any",
    }
}

/// Renders a corpus input for divergence lines: the lossy text (truncated at
/// 200 bytes) plus the byte count.
pub fn describe_bytes(bytes: &[u8]) -> String {
    const MAX_SHOWN: usize = 200;
    let shown = &bytes[..bytes.len().min(MAX_SHOWN)];
    let text = String::from_utf8_lossy(shown);
    if bytes.len() > MAX_SHOWN {
        format!("{text:?}... ({} bytes total)", bytes.len())
    } else {
        format!("{text:?} ({} bytes)", bytes.len())
    }
}

/// Prints the category tally lines of a corpus report (`category=… count=…`,
/// sorted by category name), byte-identical to every old differential's own
/// loop.
pub fn print_categories(category_counts: &BTreeMap<&'static str, usize>) {
    for (category, count) in category_counts {
        println!("  category={category} count={count}");
    }
}

/// Runs a codec's differential corpus walk and owns the exit-code law: the
/// walk prints its own report (receipt lines, divergences) and returns
/// `Err(message)` on failure; this prints `{codec}-differential: FAILED:
/// {message}` and exits 1, byte-identical to the old per-codec binaries'
/// `main` on the passing path (the frame's one extra line appears only when
/// the gate is already red).
pub fn run_differential(codec: &str, run: impl FnOnce() -> Result<(), String>) {
    if let Err(error) = run() {
        eprintln!("{codec}-differential: FAILED: {error}");
        std::process::exit(1);
    }
}

/// Runs the differential corpus for `codec`. Extra arguments are forwarded
/// only to json (`--dump-accepts`); every other codec rejects leftovers.
pub fn dispatch(codec: &str, args: &[String]) {
    let no_extra = |run: fn() -> Result<(), String>| {
        if !args.is_empty() {
            eprintln!("jqf-codec-smoke: differential {codec} takes no extra arguments");
            std::process::exit(2);
        }
        run_differential(codec, run);
    };
    match codec {
        "cbor" => no_extra(cbor::run),
        "csv" => no_extra(csv::run),
        "html" => no_extra(html::run),
        "json" => run_differential("json", || json::run(args)),
        "toml" => no_extra(toml::run),
        "xml" => no_extra(xml::run),
        "yaml" => no_extra(yaml::run),
        other => {
            eprintln!(
                "jqf-codec-smoke: unknown differential codec {other:?}; registered: cbor, csv, html, json, toml, xml, yaml"
            );
            std::process::exit(2);
        }
    }
}
