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

//! The tree-construction conformance runner over the vendored html5lib-tests
//! `tree-construction/*.dat` suite.
//!
//! Each `#data` block's expected tree (the `#document` section, or the
//! `#script-off` section when the block is scripting-flagged) is compared
//! against the html5lib dump of the recovered tree, line by line. Blocks
//! with `#document-fragment` run the FRAGMENT parser under the block's
//! context element (the fragment-conformance wave of `.plans/011` and
//! `.plans/076` item 3): the fragment's bare html root's children are
//! dumped the same way. Blocks with only `#script-on` expectations require
//! scripting and are skipped for that reason (the codec pins
//! scripting-disabled).
//!
//! Receipt line:
//!
//! ```text
//! tree-conformance: total=N pass=P fail=F skipped=S
//! ```
//!
//! Any failure exits 1.

use jqf_codec_html::tree_core::{NodeKind, Tree};

/// Renders one text/attribute value with the html5lib dump escaping.
fn escape(text: &str, in_attribute: bool) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '\u{00A0}' => out.push_str("&nbsp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if in_attribute => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The html5lib tree dump.
fn dump(tree: &Tree) -> Vec<String> {
    let mut lines = Vec::new();
    let document = tree.nodes[tree.document.0].children.clone();
    for child in document {
        dump_node(tree, child.0, 0, &mut lines);
    }
    lines
}

fn dump_node(tree: &Tree, node: usize, depth: usize, lines: &mut Vec<String>) {
    let prefix = format!("| {}", "  ".repeat(depth));
    let element = &tree.nodes[node];
    match element.kind {
        NodeKind::Element => {
            // The html5lib testSerializer law: a foreign element renders as
            // `prefix name` (e.g. `svg svg`); the prefixes are the codec's
            // namespace table. The codec's Namespace enum carries the
            // constants; map them here for the dump.
            let namespace = match element.ns {
                jqf_codec_html::tree_core::Namespace::Html => "",
                jqf_codec_html::tree_core::Namespace::Svg => "svg ",
                jqf_codec_html::tree_core::Namespace::MathMl => "math ",
            };
            // The testSerializer law: element NAMES are dumped RAW (a
            // name can contain `<`, as in `<div<div>`), never escaped.
            let mut rendered = format!("{}<{}{}", prefix, namespace, element.name);
            let mut attributes: Vec<(String, &str)> = element
                .attrs
                .iter()
                .map(|attribute| {
                    // The etree law: a foreign element's namespaced
                    // attribute renders as `prefix name` (the etree tag
                    // form) — `xlink:href` becomes `xlink href`.
                    if element.ns != jqf_codec_html::tree_core::Namespace::Html {
                        if let Some((prefix, name)) = attribute.name.split_once(':') {
                            return (format!("{prefix} {name}"), attribute.value.as_str());
                        }
                    }
                    (attribute.name.clone(), attribute.value.as_str())
                })
                .collect();
            // The html5lib testSerializer emits the element line WITHOUT
            // attributes, then one `name="value"` line per attribute at the
            // next depth, sorted by name, values RAW (no escaping).
            attributes.sort_by(|left, right| left.0.cmp(&right.0));
            rendered.push('>');
            lines.push(rendered);
            for (name, value) in &attributes {
                lines.push(format!(
                    "{}{}=\"{}\"",
                    format!("| {}", "  ".repeat(depth + 1)),
                    name,
                    value
                ));
            }
            if element.name == "template" && element.ns == jqf_codec_html::tree_core::Namespace::Html {
                lines.push(format!("{}content", format!("| {}", "  ".repeat(depth + 1))));
                for child in &element.children {
                    dump_node(tree, child.0, depth + 2, lines);
                }
            } else {
                for child in &element.children {
                    dump_node(tree, child.0, depth + 1, lines);
                }
            }
        }
        NodeKind::Text => {
            // The html5lib dump renders text RAW, so an embedded newline
            // breaks the line: the continuation has no prefix.
            let rendered = format!("{prefix}\"{}\"", element.data);
            lines.extend(rendered.split('\n').map(str::to_owned).collect::<Vec<_>>());
        }
        NodeKind::Comment => {
            // The testSerializer renders comment data RAW (never escaped).
            lines.push(format!("{prefix}<!-- {} -->", element.data));
        }
        NodeKind::Doctype => {
            let doctype = element.doctype.as_ref().expect("doctype data");
            let name = doctype.name.as_deref().unwrap_or("");
            let public = doctype.public_identifier.as_deref().unwrap_or("");
            let system = doctype.system_identifier.as_deref().unwrap_or("");
            if public.is_empty() && system.is_empty() {
                lines.push(format!("{prefix}<!DOCTYPE {name}>"));
            } else {
                // The testSerializer law: doctype identifiers are dumped
                // RAW — a quote inside the system identifier stays a quote
                // (the corpus pins `taco"">`).
                lines.push(format!(
                    "{prefix}<!DOCTYPE {name} \"{}\" \"{}\">",
                    escape(public, false),
                    escape(system, false)
                ));
            }
        }
        NodeKind::Document => {}
    }
}

/// One parsed conformance block: the input data, the expected dump lines
/// (`None` when the block carries only scripting-flagged expectations that
/// are not ours), the `#document-fragment` flag, and the fragment CONTEXT
/// element name (the line right after `#document-fragment`).
type Block = (String, Option<Vec<String>>, bool, Option<String>);

/// Parses one .dat file into blocks.
fn parse_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut current: Option<Block> = None;
    let mut section = "";
    let mut saw_script_on = false;
    let mut saw_script_off = false;
    let mut saw_document = false;
    for raw in text.lines() {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.starts_with("#data") {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = Some((String::new(), None, false, None));
            section = "data";
            saw_script_on = false;
            saw_script_off = false;
            saw_document = false;
            continue;
        }
        if line.starts_with('#') {
            section = line;
            match line {
                "#script-on" => saw_script_on = true,
                "#script-off" => saw_script_off = true,
                "#document" => saw_document = true,
                _ => {}
            }
            continue;
        }
        let Some((data, expected, has_fragment, context)) = current.as_mut() else {
            continue;
        };
        match section {
            "data" => {
                // The data section is byte-exact: trailing whitespace is
                // part of the input (a test input can END in a space).
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(line);
            }
            "#document" => {
                saw_document = true;
                if !line.is_empty() {
                    let entries = expected.get_or_insert_with(Vec::new);
                    entries.push(line.to_string());
                }
            }
            "#script-off" => {
                saw_script_off = true;
                if !line.is_empty() {
                    let entries = expected.get_or_insert_with(Vec::new);
                    entries.push(line.to_string());
                }
            }
            "#script-on" => {
                saw_script_on = true;
                // Not our expectations (scripting is pinned disabled).
            }
            "#document-fragment" => {
                // The first non-`#` line in this section is the fragment
                // context element name.
                *has_fragment = true;
                if context.is_none() {
                    *context = Some(line.to_string());
                }
            }
            _ => {}
        }
        // When the block ends with only #script-on expectations (no
        // #document and no #script-off), the expected tree stays None.
        if saw_script_on && !saw_script_off && !saw_document && section == "#script-on" {
            *expected = None;
        }
    }
    if let Some(block) = current.take() {
        blocks.push(block);
    }
    blocks
}

/// The standing ALLOWLIST (differential-acceptance style): the four
/// remaining tree-construction divergences, each with its recorded shape and
/// its exact retirement condition. The list is VISIBLE — a case that starts
/// passing is reported (a stale entry fails the lane, so a fix cannot leave
/// its waiver behind), and a new case is never added silently.
const ALLOWED: &[(&str, usize, &str)] = &[
    (
        "tc-tests1.dat",
        30,
        "<a><table><td><a><table></table><a></tr><a></table><b>X</b>C<a>Y",
    ),
    (
        "tc-tests1.dat",
        103,
        "<a><table><td><a><table></table><a></tr><a></table><a>",
    ),
    (
        "tc-tricky01.dat",
        7,
        "<TABLE>\n<TR>\n<CENTER><CENTER><TD></TD></TR><TR>\n<FONT>\n<TABLE><tr></tr></TABLE>\n</P>\n<a></font><font></a>\nThis page contains an insanely badly-nested tag sequence.",
    ),
    (
        "tc-tricky01.dat",
        8,
        "<html>\n<body>\n<b><nobr><div>This text is in a div inside a nobr</nobr>More text that should not be in the nobr, i.e., the\nnobr should have closed the div inside it implicitly. </b><pre>A pre tag outside everything else.</pre>\n</body>\n</html>",
    ),
];

/// The allowlist's retirement conditions (one per entry, same order): the
/// misnested-formatting-in-table adoption-agency re-adoption, where the
/// cloned formatting element's subtree placement differs from the corpus's
/// (the 0.90 reference's `reparentChildren`/`appendChild` sequence lands the
/// clone's children differently). Retire each entry when its expected tree
/// matches.
const ALLOWED_REASONS: &[&str] = &[
    "the a-in-table adoption agency: the re-adopted clone's subtree differs",
    "the a-in-table adoption agency: the re-adopted clone's subtree differs",
    "the misnested formatting/table adoption agency: the fostered text's formatting differs",
    "the nobr/div misnesting: the adoption agency's clone placement differs",
];

fn allowed_index(file: &str, index: usize, data: &str) -> Option<usize> {
    ALLOWED
        .iter()
        .position(|(f, i, d)| *f == file && *i == index && d.trim() == data.trim())
}

fn main() {
    // The default corpus path resolves from this crate's manifest, so the
    // runner works from any working directory; an explicit argument still
    // wins.
    let corpus = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../jqf-codec/html/corpus/tree-construction")
        });
    let mut total = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();
    let mut skipped_reasons: std::collections::BTreeMap<&str, usize> = Default::default();
    for entry in std::fs::read_dir(&corpus).expect("corpus dir") {
        let path = entry.expect("entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("dat") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("dat file");
        let blocks = parse_blocks(&text);
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        for (index, (data, expected, has_fragment, context)) in blocks.into_iter().enumerate() {
            total += 1;
            let Some(expected) = expected else {
                skipped += 1;
                *skipped_reasons.entry("script-on-only").or_default() += 1;
                continue;
            };
            eprintln!("building block {file_name}[{index}]");
            // A `#document-fragment` block runs the fragment parser under
            // its context element (011's fragment-conformance wave): the
            // fragment's tree is the bare html root's children, dumped the
            // same way. A missing context is a corpus shape error, not a
            // skip.
            let tree = match (has_fragment, context) {
                (true, Some(context)) => jqf_codec_html::tree_core::TreeBuilder::build_fragment(&data, &context),
                (true, None) => {
                    failures.push(format!(
                        "{file_name}[{index}] declares #document-fragment but no context element"
                    ));
                    continue;
                }
                (false, _) => jqf_codec_html::tree_core::TreeBuilder::build(&data),
            };
            let actual = dump(&tree);
            if actual == expected {
                // A STALE allowlist entry — the case stopped failing — is
                // a lane failure, so a fix cannot leave its waiver behind.
                if let Some(allowed) = allowed_index(&file_name, index, &data) {
                    failures.push(format!(
                        "{file_name}[{index}] is ALLOWED but now PASSES — retire the entry ({})",
                        ALLOWED_REASONS[allowed]
                    ));
                }
            }
            if actual != expected {
                let allowed = allowed_index(&file_name, index, &data);
                if let Some(allowed) = allowed {
                    println!(
                        "tree-conformance: ALLOWED {file_name}[{index}] — {}",
                        ALLOWED_REASONS[allowed]
                    );
                    continue;
                }
                failures.push(format!(
                    "{file_name}[{index}] input={data:?}\n  expected:\n{}\n  actual:\n{}",
                    expected
                        .iter()
                        .map(|line| format!("    {line}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    actual
                        .iter()
                        .map(|line| format!("    {line}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
            }
        }
    }
    println!(
        "tree-conformance: total={total} pass={} fail={} skipped={skipped}",
        total - skipped - failures.len(),
        failures.len()
    );
    for (reason, count) in &skipped_reasons {
        println!("tree-conformance: skipped {count} for {reason}");
    }
    for failure in &failures {
        println!("tree-conformance: {failure}");
    }
    if !failures.is_empty() {
        std::process::exit(1);
    }
    println!("tree-conformance: all receipts pass");
}
