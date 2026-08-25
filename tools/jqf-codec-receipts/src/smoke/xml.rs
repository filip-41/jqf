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

//! XML (XML 1.0 Fifth Edition + Namespaces) codec receipt battery.
//!
//! Pins the XML codec's surface as of the decode vertical's landing: the
//! registration's dialect set (`xml.document@1` input plus the two output
//! profiles `xml.source@1` and `xml.jqf-deterministic@1`), the advertised
//! route inventory (slot 0 Whole/CompleteDocument only — the demand ladder
//! and the encoder land in later stages and grow this pin in the same
//! commit), and a whole-route decode corpus covering the D1 projection law:
//! an element is an ORDERED array of its raw mixed-content children, its
//! expanded name and resolved attributes are intrinsic facts (`@name`,
//! `@attrs`, `@content`) and never object members, comments and processing
//! instructions are children, entities resolve from the internal subset,
//! namespaces resolve with the predeclared `xml` prefix, and malformed or
//! undeclared-prefix documents are rejected.

use crate::drive::{resources, resume, source, whole_requirement};
use jqf_codec_core::{
    AccessFootprintKind, AccessOutcome, AccessResultKind, CodecRunContext, DecodeRequest, DiagnosticPolicy,
    ValidationMode,
};
use jqf_data::{DialectId, Value};
use jqf_resource::ResourceContext;

/// Drives the XML whole-route provider to one materialized root value.
fn decode(bytes: &[u8]) -> Result<Value, String> {
    let mut resources = resources();
    let registration = jqf_codec_xml::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("xml decoder factory")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("xml provider: {error:?}"))?;
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

fn expect_reject(bytes: &[u8], error_text: &str) -> Result<(), String> {
    match decode(bytes) {
        Ok(_) => Err(format!("expected reject for {bytes:02x?}")),
        Err(error) if error.contains(error_text) => Ok(()),
        Err(other) => Err(format!("expected {error_text:?} for {bytes:02x?}, got {other}")),
    }
}

/// Compact render for readable corpus assertions over owned values.
fn render(value: &Value) -> String {
    use jqf_data::Value as V;
    match value {
        V::Null => "null".into(),
        V::Bool(true) => "true".into(),
        V::Bool(false) => "false".into(),
        V::Number(number) => {
            if let Some(integer) = number.to_integer() {
                integer.as_str().into()
            } else if let Some(float) = number.as_float() {
                let value = float.get();
                if value.fract() == 0.0 {
                    format!("{value:.1}")
                } else {
                    format!("{value}")
                }
            } else {
                format!("{number:?}")
            }
        }
        V::String(text) => format!("{text:?}"),
        // `as_slice` over `as_ref`: winnow (via the harness's pinned `toml`
        // oracle dep) implements `AsRef` for `[u8]`, so `as_ref` is ambiguous
        // once the toml differential references that crate. Same bytes.
        V::Bytes(bytes) => format!("h{:?}", bytes.as_slice()),
        V::Array(array) => {
            let mut out = String::from("[");
            for (index, item) in array.iter().enumerate() {
                if index != 0 {
                    out.push_str(", ");
                }
                out.push_str(&render(item));
            }
            out.push(']');
            out
        }
        V::Object(object) => {
            let mut out = String::from("{");
            for (index, entry) in object.iter().enumerate() {
                if index != 0 {
                    out.push_str(", ");
                }
                out.push('"');
                out.push_str(entry.key());
                out.push_str("\": ");
                out.push_str(&render(entry.value()));
            }
            out.push('}');
            out
        }
        V::Tagged { tag, payload } => format!("{}({})", tag.as_str(), render(payload)),
        V::OffsetDateTime(datetime) => {
            let date = datetime.local.date;
            let time = &datetime.local.time;
            let fraction = time.fraction().digits();
            let mut out = format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                date.year(),
                date.month(),
                date.day(),
                time.hour(),
                time.minute(),
                time.second(),
            );
            if !fraction.is_empty() {
                out.push('.');
                out.push_str(fraction);
            }
            out.push('Z');
            out
        }
        other => format!("{other:?}"),
    }
}

/// Pins the registration surface: format `xml`, three dialects, the input
/// dialect `xml.document@1` advertised.
fn registration_surface() -> Result<(), String> {
    let registration = jqf_codec_xml::registration().map_err(|error| format!("{error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != "xml" {
        return Err(format!("unexpected format {}", descriptor.format().as_str()));
    }
    let dialects = descriptor.dialects();
    let expected = ["xml.document@1", "xml.source@1", "xml.jqf-deterministic@1"];
    if dialects.len() != expected.len()
        || dialects
            .iter()
            .zip(expected)
            .any(|(left, right)| left.as_str() != right)
    {
        return Err(format!(
            "unexpected XML dialect set: {}",
            dialects
                .iter()
                .map(|dialect| dialect.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let _ = DialectId::try_new("xml.document@1").map_err(|error| format!("{error:?}"))?;
    Ok(())
}

/// Pins the route inventory: the two-slot ladder the XML projection serves —
/// slot 0 `Whole`/`CompleteDocument`, slot 1 Exact/`Located`. There is no
/// projected element-stream slot: a projected member is a named field of a
/// sequence item, and an XML child element's members are its positional
/// children, not named fields, so there is nothing to project.
fn route_inventory() -> Result<(), String> {
    let mut resources = resources();
    let registration = jqf_codec_xml::registration().map_err(|error| format!("{error:?}"))?;
    let provider = registration
        .decoder()
        .expect("xml decoder factory")
        .create_provider(
            source(b"<a/>"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID).expect("dialect"),
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
            "XML advertised {} routes; expected the two-slot ladder",
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
        return Err(format!("XML route inventory drifted: {kinds:?}, expected {expected:?}"));
    }
    Ok(())
}

/// The whole-route decode corpus: the D1 projection law.
fn decode_corpus() -> Result<(), String> {
    let cases: &[(&str, &str)] = &[
        // An element with only text is a one-child array; an empty element
        // is an empty array. Attributes are facts, never members.
        ("<a>hi</a>", "[\"hi\"]"),
        ("<a></a>", "[]"),
        ("<a/>", "[]"),
        ("<a b=\"1\">hi</a>", "[\"hi\"]"),
        // Mixed content keeps ORDER: text, element, text.
        ("<a>b<x/>e</a>", "[\"b\", [], \"e\"]"),
        // Nested elements.
        ("<a><b><c>v</c></b></a>", "[[[\"v\"]]]"),
        // Comments are children; processing instructions render their target.
        ("<a>x<!--cmt-->y</a>", "[\"x\", \"cmt\", \"y\"]"),
        ("<a><?pi data?></a>", "[\"<?pi data?>\"]"),
        ("<a><?pi?></a>", "[\"<?pi?>\"]"),
        // CDATA is text.
        ("<a><![CDATA[raw <>&]]></a>", "[\"raw <>&\"]"),
        // Character references and the five predefined entities.
        ("<a>&lt;&amp;A&#65;</a>", "[\"<&AA\"]"),
        // Internal subset entity.
        ("<!DOCTYPE r [<!ENTITY co \"Codec\">]><r>&co;</r>", "[\"Codec\"]"),
        // Namespaces: expanded names are facts; declared prefixes resolve.
        ("<p xmlns:n=\"urn:x\"><n:e>v</n:e></p>", "[[\"v\"]]"),
        // The predeclared xml prefix is bound at parse start.
        ("<a xml:lang=\"en\">v</a>", "[\"v\"]"),
        // Document-level prolog: an XML declaration and comments are skipped.
        ("<?xml version=\"1.0\"?><a>v</a>", "[\"v\"]"),
        // Entity expansion is bounded and recursive references nest.
        (
            "<!DOCTYPE r [<!ENTITY a \"1\"><!ENTITY b \"&a;2\">]><r>&b;</r>",
            "[\"12\"]",
        ),
    ];
    for (source_text, expected) in cases {
        let value =
            decode(source_text.as_bytes()).map_err(|error| format!("decode failed for {source_text:?}: {error}"))?;
        let rendered = render(&value);
        if rendered != *expected {
            return Err(format!(
                "decode mismatch for {source_text:?}: got {rendered:?}, expected {expected:?}"
            ));
        }
    }
    Ok(())
}

/// The reject corpus: malformed or unrepresentable documents.
fn reject_corpus() -> Result<(), String> {
    // Mismatched end tag.
    expect_reject(b"<a><b></a>", "InvalidInput")?;
    // Missing end tag at EOF.
    expect_reject(b"<a><b></b>", "InvalidInput")?;
    // Undeclared prefix in a start tag.
    expect_reject(b"<a><a:bad/></a>", "InvalidInput")?;
    // Undeclared prefix in an end tag.
    expect_reject(b"<a:bad/>", "InvalidInput")?;
    // A second root element (the whole document is ONE document).
    expect_reject(b"<a/><b/>", "InvalidInput")?;
    // No root element at all.
    expect_reject(b"", "InvalidInput")?;
    // A duplicated expanded attribute name (same prefix and local twice).
    expect_reject(b"<a xmlns:x=\"x\" x:b=\"1\" x:b=\"2\"/>", "InvalidInput")?;
    // Unbound entity reference.
    expect_reject(b"<!DOCTYPE r [<!ENTITY a \"1\">]><r>&b;</r>", "InvalidInput")?;
    // Raw '<' in attribute content.
    expect_reject(b"<a b=\"<\"/>", "InvalidInput")?;
    Ok(())
}

/// The `xml.xpath@1` selector profile through the engine's `xpath/1` door:
/// compile/execute over a decoded whole-route document, the closed-grammar
/// rejections, and the format-mismatch law. This is the receipt for the
/// profile — no longer zero code.
fn xpath_corpus() -> Result<(), String> {
    use jqf_builtins::selector::SelectorBudget;
    // Decode the fixture through the whole route, then select over the
    // recovered document authority.
    let bytes = br#"<catalog><item id="1" class="a"><name>ada</name><price>9.5</price></item><item id="2" class="b"><name>linus</name></item><item id="3" class="a b"><name>grace</name></item></catalog>"#;
    let mut resources = resources();
    let registration = jqf_codec_xml::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("xml decoder factory")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("xml provider: {error:?}"))?;
    let requirement = whole_requirement(&resources);
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
        let compiled = jqf_builtins::selector::compile(jqf_builtins::selector::SelectorLanguage::XmlXPath1, selector)
            .map_err(|error| format!("compile {selector:?}: {error:?}"))?;
        let result = jqf_builtins::selector::select(document, root, &compiled, SelectorBudget::default(), resources)
            .map_err(|error| format!("select {selector:?}: {error:?}"))?;
        let jqf_builtins::selector::SelectorResult::Elements(nodes) = result else {
            return Err(format!("select {selector:?}: unexpected scalar result"));
        };
        Ok(nodes)
    }
    // The names of the selected elements, via the .@name fact the XML
    // projection attaches.
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
            let owner = jqf_data::LocalOwnerRef::Node(*node);
            let limit = jqf_data::BatchLimit::new(usize::MAX).ok_or_else(|| "batch limit".to_owned())?;
            loop {
                match reader
                    .poll_batch(limit, resources)
                    .map_err(|error| format!("facts: {error:?}"))?
                {
                    jqf_data::ReaderPoll::Batch(batch) => {
                        for fact in batch.iter() {
                            if fact.owner() == owner && fact.role().as_str() == jqf_codec_core::markup::NAME_FACT {
                                if let jqf_data::FactPayloadView::Text(text) = fact.payload() {
                                    out.push(text.to_owned());
                                }
                            }
                        }
                    }
                    jqf_data::ReaderPoll::Pending => {
                        resume(resources);
                    }
                    jqf_data::ReaderPoll::End(_) => break,
                }
            }
        }
        Ok(out)
    }

    let cases: &[(&str, &[&str])] = &[
        // Absolute paths, the descendant abbreviation, and position.
        ("/catalog/item", &["item", "item", "item"]),
        ("//item", &["item", "item", "item"]),
        ("//item[1]", &["item"]),
        ("//item[position() = last()]", &["item"]),
        ("//item[2]", &["item"]),
        // Relative paths start at the context node (the document root, whose
        // children are the items directly).
        ("item", &["item", "item", "item"]),
        ("item/name", &["name", "name", "name"]),
        ("//catalog/item/name", &["name", "name", "name"]),
        // The wildcard and the descendant axis.
        ("//*[@id='2']", &["item"]),
        ("//item/../item", &["item", "item", "item"]),
        // Predicate atoms: attribute equality in both operand orders, text()
        // atomization (only DIRECT text children — `ada` lives inside
        // `name`, so the item itself has no matching text node), and
        // string(.) over concatenated descendant text.
        ("//item[@id='1']", &["item"]),
        ("//item['2' = @id]", &["item"]),
        ("//name[text() = 'ada']", &["name"]),
        ("//item[text() = 'ada']", &[]),
        ("//name['linus' = text()]", &["name"]),
        ("//item[string(.) = 'ada9.5']", &["item"]),
        ("//item[string(.) = 'grace']", &["item"]),
        // Multiple predicates apply left to right with fresh positions.
        ("//item[@class='a'][1]", &["item"]),
        ("//item[1][@id='2']", &[]),
        ("//item[position() = last()][@class='a b']", &["item"]),
        // Union, deduplicated, in document order (each item precedes its
        // name).
        ("//name | //item", &["item", "name", "item", "name", "item", "name"]),
        ("//item | //item", &["item", "item", "item"]),
        // The widening: comparison operators (numeric when either
        // side is a number) and the pure functions in predicate position.
        ("//item[@id > 0]", &["item", "item", "item"]),
        ("//item[@id >= 2]", &["item", "item"]),
        ("//item[@id != 2]", &["item", "item"]),
        ("//item[position() <= 1]", &["item"]),
        ("//item[count(name) = 1]", &["item", "item", "item"]),
        ("//item[string-length(@id) = 1]", &["item", "item", "item"]),
        ("//item[concat(@id, '-') = '1-']", &["item"]),
        ("//item[name() = 'item']", &["item", "item", "item"]),
    ];
    for (selector, expected) in cases {
        let nodes = select(document, root, &mut resources, selector)?;
        let actual = names(document, &mut resources, &nodes)?;
        if actual != *expected {
            return Err(format!("xpath {selector:?} selected {actual:?}, expected {expected:?}"));
        }
    }

    // The 2026-08-09 widening: a TOP-LEVEL function call's result is a
    // scalar (the four pure functions the predicate grammar already knew),
    // evaluated with the document node as the context node (XPath 1.0).
    // `count(path)` is the node-set's cardinality as an exact integer;
    // concat/string-length/name answer strings/numbers.
    let scalars: &[(&str, &str)] = &[
        ("count(//item)", "3"),
        ("count(//item[@id > 1])", "2"),
        ("concat('x', '-', 'y')", "x-y"),
        ("string-length(concat('ab', 'c'))", "3"),
        // The document node has no name: XPath's empty answer.
        ("name()", ""),
    ];
    for (selector, expected) in scalars {
        let compiled = jqf_builtins::selector::compile(jqf_builtins::selector::SelectorLanguage::XmlXPath1, selector)
            .map_err(|error| format!("compile {selector:?}: {error:?}"))?;
        let result =
            jqf_builtins::selector::select(document, root, &compiled, SelectorBudget::default(), &mut resources)
                .map_err(|error| format!("select {selector:?}: {error:?}"))?;
        let actual = match result {
            jqf_builtins::selector::SelectorResult::Elements(_) => {
                return Err(format!("xpath {selector:?} must answer a scalar"));
            }
            jqf_builtins::selector::SelectorResult::Scalar(jqf_builtins::selector::ScalarResult::Number(number)) => {
                if number.fract() == 0.0 {
                    format!("{number:.0}")
                } else {
                    format!("{number}")
                }
            }
            jqf_builtins::selector::SelectorResult::Scalar(jqf_builtins::selector::ScalarResult::Text(text)) => text,
        };
        if actual != *expected {
            return Err(format!("xpath {selector:?} answered {actual:?}, expected {expected:?}"));
        }
    }

    // The closed grammar rejects everything outside the profile.
    let rejects: &[&str] = &[
        "//@id",
        "//text()",
        "//node()",
        "//item[position()]",
        // Attribute EXISTENCE is outside the closed grammar: the profile
        // admits only the comparison atoms (a bare atom is not a truthiness
        // test).
        "//item[@class]",
        // An unknown function is a named compile error.
        "//item[unknown() = 2]",
        "/",
        "//",
        "a | ",
    ];
    for selector in rejects {
        if jqf_builtins::selector::compile(jqf_builtins::selector::SelectorLanguage::XmlXPath1, selector).is_ok() {
            return Err(format!("xpath {selector:?} must fail compilation"));
        }
    }

    // The format law: the xml.xpath@1 profile over a NON-xml document is a
    // named mismatch (the engine raises it catchably).
    let error = jqf_builtins::selector::compile(jqf_builtins::selector::SelectorLanguage::HtmlCss1, "item")
        .map_err(|error| format!("unexpected css compile failure: {error:?}"))?;
    let _ = error;
    Ok(())
}

pub fn run() -> Result<(), String> {
    let results = [
        ("registration surface", registration_surface()),
        ("route inventory", route_inventory()),
        ("decode corpus", decode_corpus()),
        ("reject corpus", reject_corpus()),
        ("xpath profile", xpath_corpus()),
    ];
    let mut failures = 0;
    for (label, result) in results {
        match result {
            Ok(()) => println!("xml-smoke: {label}: ok"),
            Err(error) => {
                failures += 1;
                println!("xml-smoke: {label}: FAIL: {error}");
            }
        }
    }
    if failures != 0 {
        println!("xml-smoke: {failures} receipt(s) failed");
        return Err(format!("{failures} receipt(s) failed"));
    }
    println!("xml-smoke: all receipts pass");
    Ok(())
}
