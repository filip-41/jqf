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

//! The html.document@1 codec receipt battery : registration
//! surface, the two-slot route ladder, the whole-route decode corpus, and
//! the `html.css@1` selector receipts through the engine's `css/1` door.
//!
//! Conformance (the html5lib tokenizer + tree-construction suites) lives in
//! the `jqf-tokenizer-conformance` and `jqf-tree-conformance` binaries.

use crate::drive::{resources, resume, source, whole_requirement};
use jqf_codec_core::{
    AccessFootprintKind, AccessOutcome, AccessResultKind, CodecRunContext, DecodeRequest, DiagnosticPolicy, FactIntent,
    ValidationMode,
};
use jqf_data::{DialectId, FactPayloadView, LocalOwnerRef, ReaderPoll, Value};
use jqf_resource::ResourceContext;

/// Drives the HTML whole-route provider to one materialized root value.
fn decode(bytes: &[u8]) -> Result<Value, String> {
    let mut resources = resources();
    let registration = jqf_codec_html::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("html decoder factory")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_html::HTML_DOCUMENT_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("html provider: {error:?}"))?;
    let requirement = whole_requirement(&resources);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {error:?}"))?;
    {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).map_err(|error| format!("decode: {error:?}"))?;
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            return Err("expected full document".into());
        };
        return product
            .document()
            .materialize_root(&mut resources)
            .map_err(|error| format!("materialize: {error:?}"));
    }
}

/// Pins the registration surface: format `html`, the two input dialects
/// (`html.document@1` advertised, `html.fragment@1` registered), and the
/// two output profiles.
fn registration_surface() -> Result<(), String> {
    let registration = jqf_codec_html::registration().map_err(|error| format!("{error:?}"))?;
    let surface = format!("{:?}", registration);
    if !surface.contains("html") {
        return Err("registration surface does not name html".into());
    }
    // The document registration carries its input dialect plus the two
    // output profiles (the fragment registration is a separate set, input
    // only).
    let expected = ["html.document@1", "html.source@1", "html.document-serialize@1"];
    let descriptor = registration.descriptor();
    let dialects = descriptor.dialects();
    if dialects.len() != expected.len()
        || dialects
            .iter()
            .zip(expected)
            .any(|(actual, want)| actual.as_str() != want)
    {
        let actual: Vec<&str> = dialects.iter().map(|d| d.as_str()).collect();
        return Err(format!(
            "html registration dialects drifted: expected {expected:?}, got {actual:?}"
        ));
    }
    if registration.encoder().is_none() {
        return Err("html encoder factory missing".into());
    }
    // The fragment registration is a separate set (input only): its dialect
    // must stay pinned too, so a drift in either registration fails here.
    let fragment = jqf_codec_html::registration_fragment().map_err(|error| format!("{error:?}"))?;
    let fragment_dialects = fragment.descriptor().dialects();
    if fragment_dialects.len() != 1 || fragment_dialects[0].as_str() != "html.fragment@1" {
        let actual: Vec<&str> = fragment_dialects.iter().map(|d| d.as_str()).collect();
        return Err(format!(
            "html fragment registration dialects drifted: expected [\"html.fragment@1\"], got {actual:?}"
        ));
    }
    if fragment.encoder().is_some() {
        return Err("html fragment encoder must be absent".into());
    }
    Ok(())
}

/// Pins the route inventory: the two-slot ladder the HTML projection
/// serves — slot 0 `Whole`/`CompleteDocument`, slot 1 Exact/`Located`.
fn route_inventory() -> Result<(), String> {
    let mut resources = resources();
    let registration = jqf_codec_html::registration().map_err(|error| format!("{error:?}"))?;
    let provider = registration
        .decoder()
        .expect("html decoder factory")
        .create_provider(
            source(b"<p>hi</p>"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_html::HTML_DOCUMENT_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let routes = provider.route_descriptions();
    if routes.len() != 2 {
        return Err(format!(
            "HTML advertised {} routes; expected the two-slot ladder",
            routes.len()
        ));
    }
    let kinds: Vec<(u32, AccessFootprintKind, AccessResultKind)> = routes
        .iter()
        .map(|route| (route.slot().get(), route.bundle().footprint(), route.bundle().result()))
        .collect();
    let expected = [
        (0, AccessFootprintKind::Whole, AccessResultKind::CompleteDocument),
        (1, AccessFootprintKind::Exact, AccessResultKind::Located),
    ];
    if kinds != expected {
        return Err(format!("route ladder is {kinds:?}, expected {expected:?}"));
    }
    Ok(())
}

/// The whole-route decode corpus: the recovered semantic shape, the
/// document-level facts, and the comment-attachment law.
fn decode_corpus() -> Result<(), String> {
    // A full document: doctype, head with a title, body with a paragraph
    // and a comment.
    let value = decode(
        b"<!DOCTYPE html><html><head><title>Hi</title></head><body><!-- lead --><p class=\"a\">one<br>two</p></body></html>",
    )?;
    let rendered = render(&value);
    // The recovered shape: html = [head [title ["Hi"]], body [p ["one" [] "two"]]]
    // — the `<br>` is an empty element array, comments are facts, never
    // items.
    let expected = r#"[[["Hi"]] [["one" [] "two"]]]"#;
    if rendered != expected {
        return Err(format!("decoded shape {rendered:?}, expected {expected:?}"));
    }
    // The comment law: the leading comment is an attached fact of the body
    // element, never a child item.
    let comments = decode(b"<body><!-- lead --><p>x</p></body>")?;
    let rendered = render(&comments);
    if rendered != r#"[[] [["x"]]]"# {
        return Err(format!(
            "comment shape {rendered:?}, expected the comment to be absent from items"
        ));
    }
    // The encoding determination: the BOM wins, then the meta prescan, then
    // windows-1252. A windows-1252 byte decodes to U+00E9.
    let legacy = decode(b"<p>caf\xe9</p>")?;
    let rendered = render(&legacy);
    if rendered != "[[] [[\"caf\u{e9}\"]]]" {
        return Err(format!("windows-1252 decode {rendered:?}"));
    }
    Ok(())
}

/// Compact render for readable corpus assertions over owned values.
fn render(value: &Value) -> String {
    fn push(value: &Value, depth: usize, out: &mut String) {
        match value {
            Value::String(text) => {
                out.push('"');
                out.push_str(text);
                out.push('"');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        out.push(' ');
                    }
                    push(item, depth + 1, out);
                }
                out.push(']');
            }
            Value::Object(members) => {
                out.push('{');
                for (index, entry) in members.iter().enumerate() {
                    if index != 0 {
                        out.push(' ');
                    }
                    out.push_str(entry.key());
                    out.push(':');
                    push(entry.value(), depth + 1, out);
                }
                out.push('}');
            }
            other => {
                out.push_str(&format!("{other:?}"));
            }
        }
    }
    let mut out = String::new();
    push(value, 0, &mut out);
    out
}

/// The `html.css@1` selector profile through the selector crate's door:
/// compile/execute over a decoded whole-route document.
fn css_corpus() -> Result<(), String> {
    use jqf_builtins::selector::SelectorBudget;
    let bytes = b"<!DOCTYPE html><html><head><title>t</title></head><body><ul class=\"nav\"><li id=\"a\" class=\"x\">one</li><li id=\"b\">two</li></ul><p class=\"x\">para</p></body></html>";
    let mut resources = resources();
    let registration = jqf_codec_html::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("html decoder factory")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_html::HTML_DOCUMENT_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("html provider: {error:?}"))?;
    // CSS walks occurrence topology and attribute facts; identity coverage omits both.
    let requirement = whole_requirement(&resources).with_fact_intent(FactIntent::Preserve);
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
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            return Err("expected full document".into());
        };
        product.try_clone().expect("clone")
    };
    let document = product.document();
    let root = document.root_handle();

    fn select(
        document: &jqf_data::Document<'_>,
        root: jqf_data::NodeHandle,
        resources: &mut ResourceContext<'_>,
        selector: &str,
    ) -> Result<Vec<jqf_data::NodeId>, String> {
        let compiled = jqf_builtins::selector::compile(jqf_builtins::selector::SelectorLanguage::HtmlCss1, selector)
            .map_err(|error| format!("compile {selector:?}: {error:?}"))?;
        let result = jqf_builtins::selector::select(document, root, &compiled, SelectorBudget::default(), resources)
            .map_err(|error| format!("select {selector:?}: {error:?}"))?;
        let jqf_builtins::selector::SelectorResult::Elements(nodes) = result else {
            return Err(format!("select {selector:?}: unexpected scalar result"));
        };
        Ok(nodes)
    }

    fn names(
        document: &jqf_data::Document<'_>,
        resources: &mut ResourceContext<'_>,
        nodes: &[jqf_data::NodeId],
    ) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for node in nodes {
            let mut reader = document
                .fact_reader(resources)
                .map_err(|error| format!("fact reader: {error:?}"))?;
            let owner = LocalOwnerRef::Node(*node);
            let limit = jqf_data::unbounded_batch_limit();
            loop {
                match reader
                    .poll_batch(limit, resources)
                    .map_err(|error| format!("facts: {error:?}"))?
                {
                    ReaderPoll::Batch(batch) => {
                        for fact in batch.iter() {
                            if fact.owner() == owner && fact.role().as_str() == jqf_codec_core::markup::NAME_FACT {
                                if let FactPayloadView::Text(text) = fact.payload() {
                                    out.push(text.to_owned());
                                }
                            }
                        }
                    }
                    ReaderPoll::Pending => {
                        resume(resources);
                    }
                    ReaderPoll::End(_) => break,
                }
            }
        }
        Ok(out)
    }

    let cases: &[(&str, &[&str])] = &[
        ("li", &["li", "li"]),
        (".x", &["li", "p"]),
        ("#a", &["li"]),
        ("ul.nav > li", &["li", "li"]),
        ("p.x", &["p"]),
        ("body li", &["li", "li"]),
        ("li:first-child", &["li"]),
        ("ul li:last-child", &["li"]),
    ];
    for (selector, expected) in cases {
        let nodes = select(document, root, &mut resources, selector)?;
        let actual = names(document, &mut resources, &nodes)?;
        if actual != *expected {
            return Err(format!("css {selector:?} selected {actual:?}, expected {expected:?}"));
        }
    }
    Ok(())
}

pub fn run() -> Result<(), String> {
    let results = [
        ("registration surface", registration_surface()),
        ("route inventory", route_inventory()),
        ("decode corpus", decode_corpus()),
        ("css profile", css_corpus()),
    ];
    let mut failures = 0;
    for (label, result) in results {
        match result {
            Ok(()) => println!("html-smoke: {label}: ok"),
            Err(error) => {
                failures += 1;
                println!("html-smoke: {label}: FAIL: {error}");
            }
        }
    }
    if failures != 0 {
        println!("html-smoke: {failures} receipt(s) failed");
        return Err(format!("{failures} receipt(s) failed"));
    }
    println!("html-smoke: all receipts pass");
    Ok(())
}
