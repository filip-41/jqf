//! The codec-native selector seam.
//!
//! A selector profile is a CODEC-NATIVE language: `xml.xpath@1` is the XML codec's closed XPath 3.1 subset (§4.9 of
//! `codec-portfolio-design.md`) and `html.css@1` is the HTML codec's static Selectors Level 4 profile (§4.10).
//! Neither language enters jqf syntax and neither is jqf-data's business — but the machinery that declares, compiles,
//! and executes such a language is codec-agnostic and belongs to no codec, by the project's own rule that a codec owns
//! only its format. This module is that machinery, built once on the simpler parser (XML) and exercised twice (HTML is
//! a second consumer, not a second seam).
//!
//! **The XPath 3.1 subset is FROZEN** (its sole consumer is the selector builtins): the subset grows only on demand —
//! a new construct needs a ruling, never a silent widening.
//!
//! ## The law
//!
//! - **Declaration.** Each language is one [`SelectorLanguage`] variant binding its stable identity text to the exact
//!   format it serves and the fact roles its markup projection uses (kernel `name` / `attribute`). A selector compiled
//!   for one format never runs over another document's authority.
//! - **Compilation.** [`compile`] turns selector text into an owned plan. Compilation is total over the closed
//!   grammar: every construct outside the profile is a named compile error, never a silent subset. The plan carries
//!   the language, the format, and the normalized text; canonical selector identity is `(language, text)` — v1 binds
//!   no namespace environment channel, so `prefix:name` forms are undeclared-prefix compile errors and `Q{uri}name`
//!   is the one expanded-name spelling.
//! - **Execution.** [`select`] evaluates one compiled selector over one recovered document with an explicit scope
//!   node. It owns document order, duplicate suppression, errors, and resource budgets. The traversal domain is the
//!   scope node and its element descendants; matching has full read-only visibility of the same document (ancestor
//!   and sibling combinators may inspect nodes outside an element scope; they can never return them). Results are
//!   returned in document order, deduplicated, as [`jqf_data::NodeId`]s the caller re-locates in its own authority.
//!
//! The evaluator works over the format-neutral [`jqf_data::Document`] projection (the portable footprint): an element
//! is an array node carrying the language's `name`/`attrs`/`content` attached facts, text is a string leaf child, and
//! comments are attached facts rather than children. There is no codec-native tree and no universal DOM on this seam; a
//! future provider candidate is a codec-side optimization that this exact evaluator must postfilter.

#![allow(
    clippy::missing_errors_doc,
    reason = "fallible APIs use the closed structured SelectorError vocabulary"
)]
#![allow(
    clippy::too_many_lines,
    clippy::unnecessary_wraps,
    clippy::if_not_else,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    clippy::collapsible_if,
    clippy::doc_markdown,
    clippy::match_same_arms,
    clippy::single_match_else,
    clippy::unnecessary_cast,
    clippy::unnested_or_patterns,
    clippy::unnecessary_lazy_evaluations,
    clippy::unnecessary_mut_passed,
    clippy::needless_lifetimes,
    clippy::manual_let_else,
    clippy::explicit_iter_loop,
    clippy::collapsible_match,
    reason = "state-machine parsers mirror the reference grammars' shape; predicate \
              positions are inherently i64; the An+B law is signed; container sizes are \
              bounded by the u32::MAX source ceiling; the predicate comparison is XPath \
              1.0's OWN binary64 law (exact f64 equality — NaN must not equal NaN), and \
              its number conversion is XPath's number()"
)]

extern crate alloc;

mod bidi_ranges;
mod css;
mod dir;
mod error;
mod index;
mod lang;
mod xpath;

pub use error::SelectorError;
pub use lang::SelectorLanguage;

use alloc::vec::Vec;
use jqf_data::{Document, NodeHandle, NodeId};
use jqf_resource::ResourceContext;

/// The compile-time budget an owned selector plan may consume (selector bytes plus a flat parse-work allowance).
/// Selector text is small by contract; the bound exists so a hostile string cannot win an unbounded parse.
pub const MAX_SELECTOR_BYTES: usize = 4096;

/// The run-time budgets one [`select`] activation consumes.
///
/// Every counter is a hard ceiling: exceeding one fails the activation with [`SelectorError::Budget`] before the next
/// step of work, so a selector over a large document degrades into an error rather than an unbounded walk.
#[derive(Clone, Copy, Debug)]
pub struct SelectorBudget {
    /// Candidate evaluations (one candidate × one complex selector).
    pub max_candidate_tests: u64,
    /// Walk steps (one ancestor, sibling, descendant, or sibling-scan step).
    pub max_walk_steps: u64,
    /// Matched results returned.
    pub max_results: u64,
}

impl Default for SelectorBudget {
    fn default() -> Self {
        Self {
            max_candidate_tests: 1_000_000,
            max_walk_steps: 8_000_000,
            max_results: 65_536,
        }
    }
}

/// One compiled selector, ready to run against the documents of its format.
///
/// The plan is owned and immutable; compilation errors never reach this type.
#[derive(Debug)]
pub struct CompiledSelector {
    language: SelectorLanguage,
    plan: Plan,
}

#[derive(Debug)]
enum Plan {
    XPath(xpath::XPathPlan),
    Css(css::CssPlan),
}

/// Compiles selector text under one language.
///
/// # Errors
///
/// Returns a named [`SelectorError::Compile`] when the text is outside the language's closed grammar, or
/// [`SelectorError::Budget`] when the text exceeds [`MAX_SELECTOR_BYTES`].
pub fn compile(language: SelectorLanguage, text: &str) -> Result<CompiledSelector, SelectorError> {
    if text.len() > MAX_SELECTOR_BYTES {
        return Err(SelectorError::Budget { what: "selector text" });
    }
    let plan = match language {
        SelectorLanguage::XmlXPath1 => Plan::XPath(xpath::compile_xpath(text)?),
        SelectorLanguage::HtmlCss1 => Plan::Css(css::compile_css(text)?),
    };
    Ok(CompiledSelector { language, plan })
}

/// One selector evaluation's result: an element node-set (the path/selector law for both languages) or a scalar (a
/// top-level XPath function call).
#[derive(Debug)]
pub enum SelectorResult {
    /// Element results, in document order, deduplicated.
    Elements(Vec<NodeId>),
    /// One scalar result of a top-level `xml.xpath@1` function.
    Scalar(ScalarResult),
}

/// One scalar XPath result, in the selector's own value vocabulary (the engine maps it onto `jqf-data` values at the
/// seam).
#[derive(Debug)]
pub enum ScalarResult {
    /// A count or string-length: XPath's binary64 number law.
    Number(f64),
    /// A concat or name() result.
    Text(alloc::string::String),
}

/// Evaluates one compiled selector over one recovered document.
///
/// `scope` is the evaluation's scope node: the traversal domain is that node and its element descendants, `:scope`
/// matches that node, and relative paths start there. Absolute XPath paths still start at the virtual document node,
/// but their results are limited to the domain. Matching has full visibility of the document, exactly as the scope law
/// requires.
///
/// The document's format must equal the selector language's declared format; anything else is
/// [`SelectorError::FormatMismatch`]. A document whose schema does not carry the language's fact roles is
/// [`SelectorError::NotMarkup`].
///
/// # Errors
///
/// Returns the closed [`SelectorError`] vocabulary: format mismatch, missing markup authority, budget exhaustion, or an
/// internal contract violation over an otherwise valid document.
pub fn select(
    document: &Document<'_>,
    scope: NodeHandle,
    selector: &CompiledSelector,
    budget: SelectorBudget,
    resources: &mut ResourceContext<'_>,
) -> Result<SelectorResult, SelectorError> {
    if document.format().as_str() != selector.language.format() {
        return Err(SelectorError::FormatMismatch {
            language: selector.language.id(),
            format: alloc::string::String::from(document.format().as_str()),
        });
    }
    let scope_id = document.resolve_node_handle(scope).map_err(map_data)?;
    let index = index::MarkupIndex::build(document, selector.language, budget, resources)?;
    match &selector.plan {
        Plan::XPath(plan) => match xpath::evaluate(&index, scope_id, plan, budget)? {
            xpath::XPathOutcome::Nodes(nodes) => Ok(SelectorResult::Elements(nodes)),
            xpath::XPathOutcome::Scalar(scalar) => Ok(SelectorResult::Scalar(match scalar {
                xpath::AtomScalar::Number(number) => ScalarResult::Number(number),
                xpath::AtomScalar::Text(text) => ScalarResult::Text(text),
            })),
        },
        Plan::Css(plan) => Ok(SelectorResult::Elements(css::evaluate(&index, scope_id, plan, budget)?)),
    }
}

/// Maps a document data error onto the selector error vocabulary.
pub(crate) fn map_data(error: jqf_data::DataError) -> SelectorError {
    match error {
        jqf_data::DataError::Resource(error) => error.into(),
        jqf_data::DataError::Control(_) => SelectorError::Control,
        jqf_data::DataError::Allocation => SelectorError::Allocation,
        _ => SelectorError::Internal {
            contract: "selector walk over a valid document",
        },
    }
}

/// Convenience wrapper: compiles and evaluates in one call. Test-only: every caller in the tree is this crate's own
/// `#[cfg(test)]` module; production paths call [`compile`] + [`select`] separately.
///
/// # Errors
///
/// Combines [`compile`] and [`select`] errors.
#[cfg(test)]
pub fn select_text(
    document: &Document<'_>,
    scope: NodeHandle,
    language: SelectorLanguage,
    text: &str,
    budget: SelectorBudget,
    resources: &mut ResourceContext<'_>,
) -> Result<Vec<NodeId>, SelectorError> {
    let selector = compile(language, text)?;
    match select(document, scope, &selector, budget, resources)? {
        SelectorResult::Elements(nodes) => Ok(nodes),
        SelectorResult::Scalar(_) => Err(SelectorError::Internal {
            contract: "select_text caller received a scalar result",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use jqf_data::{AccountedDocumentBuilder, AccountedSemanticNode, BuilderCoverage, FactPayload};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

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

    /// Builds an XML-shaped markup document:
    /// `<catalog><item id="1"><name>ada</name></item><item id="2"/></catalog>`.
    fn xml_document() -> (jqf_data::Document<'static>, jqf_data::NodeHandle) {
        let mut resources = resources();
        let recipe = jqf_data::DocumentSchemaRecipe::try_new(
            "xml",
            Some("xml"),
            &["xml.element@1", "text", "comment", "pi"],
            &["xml.child@1"],
            &["name", "attrs", "content", "attribute", "xml.comment@1"],
            &["name", "attrs", "content", "attribute", "xml.comment@1"],
        )
        .expect("recipe");
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            recipe.format(),
            recipe.dialect(),
            BuilderCoverage::complete(),
        )
        .expect("builder");
        builder
            .bind_source(
                jqf_data::DocumentSourceBinding::from_resolved(jqf_source::ResolvedSource::new(
                    jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
                    "t.xml",
                    b"<catalog/>",
                    0,
                ))
                .expect("binding"),
            )
            .expect("source binding");
        let make_element = |builder: &mut AccountedDocumentBuilder<'static>,
                            name: &str,
                            attrs: &[(&str, &str)],
                            children: &[jqf_data::NodeId],
                            resources: &mut ResourceContext<'_>|
         -> jqf_data::NodeId {
            let id = builder
                .add_node(
                    "xml.element@1",
                    AccountedSemanticNode::Array {
                        item_role: "xml.child@1",
                    },
                    None,
                    resources,
                )
                .expect("node");
            builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "name",
                    "name",
                    1,
                    &FactPayload::Text(alloc::string::String::from(name)),
                    resources,
                )
                .expect("name");
            let map: Vec<(alloc::string::String, FactPayload)> = attrs
                .iter()
                .map(|(k, v)| {
                    (
                        alloc::string::String::from(*k),
                        FactPayload::Text(alloc::string::String::from(*v)),
                    )
                })
                .collect();
            builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "attrs",
                    "attrs",
                    1,
                    &FactPayload::Map(map),
                    resources,
                )
                .expect("attrs");
            builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "content",
                    "content",
                    1,
                    &FactPayload::Text(alloc::string::String::new()),
                    resources,
                )
                .expect("content");
            for child in children {
                builder
                    .add_occurrence(
                        jqf_data::LocalOwnerRef::Node(id),
                        "xml.child@1",
                        None,
                        *child,
                        resources,
                    )
                    .expect("occurrence");
            }
            id
        };
        let text =
            |builder: &mut AccountedDocumentBuilder<'static>, text: &str, resources: &mut ResourceContext<'_>| {
                builder
                    .add_node("text", AccountedSemanticNode::String(text), None, resources)
                    .expect("text")
            };
        let name_ada = text(&mut builder, "ada", &mut resources);
        let name = make_element(&mut builder, "name", &[], &[name_ada], &mut resources);
        let item1 = make_element(&mut builder, "item", &[("id", "1")], &[name], &mut resources);
        let item2 = make_element(&mut builder, "item", &[("id", "2")], &[], &mut resources);
        let catalog = make_element(&mut builder, "catalog", &[], &[item1, item2], &mut resources);
        let mut finalizer = builder.begin_finish(catalog, &mut resources).expect("finalizer");
        let document = match unsafe {
            finalizer.poll_with_source(
                jqf_source::ResolvedSource::new(
                    jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
                    "t.xml",
                    b"<catalog/>",
                    0,
                ),
                &mut resources,
            )
        }
        .expect("finalize")
        {
            jqf_data::DocumentFinalizationPoll::Ready(document) => document,
            jqf_data::DocumentFinalizationPoll::Pending => {
                panic!("finalization pending for a tiny document")
            }
        };
        let handle = document.root_handle();
        (document, handle)
    }

    /// Builds an HTML-shaped markup document (the `html.css@1` substrate):
    /// `<html><body><div id="a" class="x y"><p>hi</p></div><div class="y">`
    /// `</div></body></html>` with the document mode and pragma language facts.
    fn html_document() -> (jqf_data::Document<'static>, jqf_data::NodeHandle) {
        html_document_shaped(true, false)
    }

    /// The same document with the mode authority optionally withheld.
    fn html_document_with_mode(with_mode: bool) -> (jqf_data::Document<'static>, jqf_data::NodeHandle) {
        html_document_shaped(with_mode, false)
    }

    /// The probe fixture, deepened: `html[lang=en] > body > div#a.x.y
    /// > div.inner > p "hi"`, plus `p.q[title="café"]` and `div.y` as
    /// body's later children. The `p` is the only `p` among body's children and the nearest `div` ancestor of it is
    /// `div.inner`, whose parent is `div#a` — the shapes the selector tests below match against.
    fn deep_html_document() -> (jqf_data::Document<'static>, jqf_data::NodeHandle) {
        html_document_shaped(true, true)
    }

    fn html_document_shaped(with_mode: bool, deep: bool) -> (jqf_data::Document<'static>, jqf_data::NodeHandle) {
        let mut resources = resources();
        let recipe = jqf_data::DocumentSchemaRecipe::try_new(
            "html",
            Some("html"),
            &["html.element@1", "text", "comment"],
            &["html.child@1"],
            &[
                "name",
                "attrs",
                "content",
                "attribute",
                "html.comment@1",
                "html.mode@1",
                "html.pragma-language@1",
            ],
            &[
                "name",
                "attrs",
                "content",
                "attribute",
                "html.comment@1",
                "html.mode@1",
                "html.pragma-language@1",
            ],
        )
        .expect("recipe");
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            recipe.format(),
            recipe.dialect(),
            BuilderCoverage::complete(),
        )
        .expect("builder");
        builder
            .bind_source(
                jqf_data::DocumentSourceBinding::from_resolved(jqf_source::ResolvedSource::new(
                    jqf_source::SourceRef::new(jqf_source::SourceId::new(2), jqf_source::SourceKind::Input),
                    "t.html",
                    b"<html/>",
                    0,
                ))
                .expect("binding"),
            )
            .expect("source binding");
        let make_element = |builder: &mut AccountedDocumentBuilder<'static>,
                            name: &str,
                            attrs: &[(&str, &str)],
                            children: &[jqf_data::NodeId],
                            resources: &mut ResourceContext<'_>|
         -> jqf_data::NodeId {
            let id = builder
                .add_node(
                    "html.element@1",
                    AccountedSemanticNode::Array {
                        item_role: "html.child@1",
                    },
                    None,
                    resources,
                )
                .expect("node");
            builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "name",
                    "name",
                    1,
                    &FactPayload::Text(alloc::string::String::from(name)),
                    resources,
                )
                .expect("name");
            let map: Vec<(alloc::string::String, FactPayload)> = attrs
                .iter()
                .map(|(k, v)| {
                    (
                        alloc::string::String::from(*k),
                        FactPayload::Text(alloc::string::String::from(*v)),
                    )
                })
                .collect();
            builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "attrs",
                    "attrs",
                    1,
                    &FactPayload::Map(map),
                    resources,
                )
                .expect("attrs");
            builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "content",
                    "content",
                    1,
                    &FactPayload::Text(alloc::string::String::new()),
                    resources,
                )
                .expect("content");
            for child in children {
                builder
                    .add_occurrence(
                        jqf_data::LocalOwnerRef::Node(id),
                        "html.child@1",
                        None,
                        *child,
                        resources,
                    )
                    .expect("occurrence");
            }
            id
        };
        let text =
            |builder: &mut AccountedDocumentBuilder<'static>, text: &str, resources: &mut ResourceContext<'_>| {
                builder
                    .add_node("text", AccountedSemanticNode::String(text), None, resources)
                    .expect("text")
            };
        let p_hi = text(&mut builder, "hi", &mut resources);
        let p = make_element(&mut builder, "p", &[], &[p_hi], &mut resources);
        let body_children = if deep {
            let div_inner = make_element(&mut builder, "div", &[("class", "inner")], &[p], &mut resources);
            let div_a = make_element(
                &mut builder,
                "div",
                &[("id", "a"), ("class", "x y")],
                &[div_inner],
                &mut resources,
            );
            let p_q = make_element(
                &mut builder,
                "p",
                &[("class", "q"), ("title", "café")],
                &[],
                &mut resources,
            );
            let div_y = make_element(&mut builder, "div", &[("class", "y")], &[], &mut resources);
            vec![div_a, p_q, div_y]
        } else {
            let div_a = make_element(
                &mut builder,
                "div",
                &[("id", "a"), ("class", "x y")],
                &[p],
                &mut resources,
            );
            let div_b = make_element(&mut builder, "div", &[("class", "y")], &[], &mut resources);
            vec![div_a, div_b]
        };
        let body = make_element(&mut builder, "body", &[], &body_children, &mut resources);
        let html = make_element(&mut builder, "html", &[("lang", "en")], &[body], &mut resources);
        // The document-mode authority html.css@1 requires, plus the pragma default language.
        if with_mode {
            builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(html),
                    "html.mode@1",
                    "html.mode@1",
                    1,
                    &FactPayload::Text(alloc::string::String::from("no-quirks")),
                    &mut resources,
                )
                .expect("mode");
        }
        builder
            .add_fact(
                jqf_data::LocalOwnerRef::Node(html),
                "html.pragma-language@1",
                "html.pragma-language@1",
                1,
                &FactPayload::Text(alloc::string::String::from("en")),
                &mut resources,
            )
            .expect("pragma");
        let mut finalizer = builder.begin_finish(html, &mut resources).expect("finalizer");
        let document = match unsafe {
            finalizer.poll_with_source(
                jqf_source::ResolvedSource::new(
                    jqf_source::SourceRef::new(jqf_source::SourceId::new(2), jqf_source::SourceKind::Input),
                    "t.html",
                    b"<html/>",
                    0,
                ),
                &mut resources,
            )
        }
        .expect("finalize")
        {
            jqf_data::DocumentFinalizationPoll::Ready(document) => document,
            jqf_data::DocumentFinalizationPoll::Pending => panic!("tiny document finalized"),
        };
        let handle = document.root_handle();
        (document, handle)
    }

    /// A minimal html.css@1 build context: element and text helpers over the builder, so a test can assemble its own
    /// tree under a chosen mode.
    struct HtmlTree<'a> {
        builder: &'a mut AccountedDocumentBuilder<'static>,
        resources: &'a mut ResourceContext<'static>,
    }

    impl HtmlTree<'_> {
        fn element(&mut self, name: &str, attrs: &[(&str, &str)], children: &[jqf_data::NodeId]) -> jqf_data::NodeId {
            let id = self
                .builder
                .add_node(
                    "html.element@1",
                    AccountedSemanticNode::Array {
                        item_role: "html.child@1",
                    },
                    None,
                    self.resources,
                )
                .expect("node");
            self.builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "name",
                    "name",
                    1,
                    &FactPayload::Text(alloc::string::String::from(name)),
                    self.resources,
                )
                .expect("name");
            let map: Vec<(alloc::string::String, FactPayload)> = attrs
                .iter()
                .map(|(k, v)| {
                    (
                        alloc::string::String::from(*k),
                        FactPayload::Text(alloc::string::String::from(*v)),
                    )
                })
                .collect();
            self.builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "attrs",
                    "attrs",
                    1,
                    &FactPayload::Map(map),
                    self.resources,
                )
                .expect("attrs");
            self.builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "content",
                    "content",
                    1,
                    &FactPayload::Text(alloc::string::String::new()),
                    self.resources,
                )
                .expect("content");
            for child in children {
                self.builder
                    .add_occurrence(
                        jqf_data::LocalOwnerRef::Node(id),
                        "html.child@1",
                        None,
                        *child,
                        self.resources,
                    )
                    .expect("occurrence");
            }
            id
        }

        fn text(&mut self, text: &str) -> jqf_data::NodeId {
            self.builder
                .add_node("text", AccountedSemanticNode::String(text), None, self.resources)
                .expect("text")
        }
    }

    /// Builds an HTML-shaped document with the given mode authority and a test-assembled tree under it.
    fn html_document_built(
        mode: &str,
        build: impl FnOnce(&mut HtmlTree<'_>) -> jqf_data::NodeId,
    ) -> (jqf_data::Document<'static>, jqf_data::NodeHandle) {
        let mut resources = resources();
        let recipe = jqf_data::DocumentSchemaRecipe::try_new(
            "html",
            Some("html"),
            &["html.element@1", "text", "comment"],
            &["html.child@1"],
            &[
                "name",
                "attrs",
                "content",
                "attribute",
                "html.comment@1",
                "html.mode@1",
                "html.pragma-language@1",
            ],
            &[
                "name",
                "attrs",
                "content",
                "attribute",
                "html.comment@1",
                "html.mode@1",
                "html.pragma-language@1",
            ],
        )
        .expect("recipe");
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            recipe.format(),
            recipe.dialect(),
            BuilderCoverage::complete(),
        )
        .expect("builder");
        builder
            .bind_source(
                jqf_data::DocumentSourceBinding::from_resolved(jqf_source::ResolvedSource::new(
                    jqf_source::SourceRef::new(jqf_source::SourceId::new(2), jqf_source::SourceKind::Input),
                    "t.html",
                    b"<html/>",
                    0,
                ))
                .expect("binding"),
            )
            .expect("source binding");
        let root = {
            let mut tree = HtmlTree {
                builder: &mut builder,
                resources: &mut resources,
            };
            build(&mut tree)
        };
        builder
            .add_fact(
                jqf_data::LocalOwnerRef::Node(root),
                "html.mode@1",
                "html.mode@1",
                1,
                &FactPayload::Text(alloc::string::String::from(mode)),
                &mut resources,
            )
            .expect("mode");
        let mut finalizer = builder.begin_finish(root, &mut resources).expect("finalizer");
        let document = match unsafe {
            finalizer.poll_with_source(
                jqf_source::ResolvedSource::new(
                    jqf_source::SourceRef::new(jqf_source::SourceId::new(2), jqf_source::SourceKind::Input),
                    "t.html",
                    b"<html/>",
                    0,
                ),
                &mut resources,
            )
        }
        .expect("finalize")
        {
            jqf_data::DocumentFinalizationPoll::Ready(document) => document,
            jqf_data::DocumentFinalizationPoll::Pending => panic!("tiny document finalized"),
        };
        let handle = document.root_handle();
        (document, handle)
    }

    /// A minimal xml markup build context, mirroring [`HtmlTree`].
    struct XmlTree<'a> {
        builder: &'a mut AccountedDocumentBuilder<'static>,
        resources: &'a mut ResourceContext<'static>,
    }

    impl XmlTree<'_> {
        fn element(&mut self, name: &str, attrs: &[(&str, &str)], children: &[jqf_data::NodeId]) -> jqf_data::NodeId {
            let id = self
                .builder
                .add_node(
                    "xml.element@1",
                    AccountedSemanticNode::Array {
                        item_role: "xml.child@1",
                    },
                    None,
                    self.resources,
                )
                .expect("node");
            self.builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "name",
                    "name",
                    1,
                    &FactPayload::Text(alloc::string::String::from(name)),
                    self.resources,
                )
                .expect("name");
            let map: Vec<(alloc::string::String, FactPayload)> = attrs
                .iter()
                .map(|(k, v)| {
                    (
                        alloc::string::String::from(*k),
                        FactPayload::Text(alloc::string::String::from(*v)),
                    )
                })
                .collect();
            self.builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "attrs",
                    "attrs",
                    1,
                    &FactPayload::Map(map),
                    self.resources,
                )
                .expect("attrs");
            self.builder
                .add_fact(
                    jqf_data::LocalOwnerRef::Node(id),
                    "content",
                    "content",
                    1,
                    &FactPayload::Text(alloc::string::String::new()),
                    self.resources,
                )
                .expect("content");
            for child in children {
                self.builder
                    .add_occurrence(
                        jqf_data::LocalOwnerRef::Node(id),
                        "xml.child@1",
                        None,
                        *child,
                        self.resources,
                    )
                    .expect("occurrence");
            }
            id
        }

        fn text(&mut self, text: &str) -> jqf_data::NodeId {
            self.builder
                .add_node("text", AccountedSemanticNode::String(text), None, self.resources)
                .expect("text")
        }
    }

    /// Builds `catalog > [item#1 > name > "ada", item#2]` and returns the document, the root handle, and a handle for
    /// the deep `item#1`.
    fn xml_document_deep_scope() -> (jqf_data::Document<'static>, jqf_data::NodeHandle, jqf_data::NodeHandle) {
        let mut resources = resources();
        let recipe = jqf_data::DocumentSchemaRecipe::try_new(
            "xml",
            Some("xml"),
            &["xml.element@1", "text", "comment", "pi"],
            &["xml.child@1"],
            &["name", "attrs", "content", "attribute", "xml.comment@1"],
            &["name", "attrs", "content", "attribute", "xml.comment@1"],
        )
        .expect("recipe");
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            recipe.format(),
            recipe.dialect(),
            BuilderCoverage::complete(),
        )
        .expect("builder");
        builder
            .bind_source(
                jqf_data::DocumentSourceBinding::from_resolved(jqf_source::ResolvedSource::new(
                    jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
                    "t.xml",
                    b"<catalog/>",
                    0,
                ))
                .expect("binding"),
            )
            .expect("source binding");
        let (catalog, item1) = {
            let mut tree = XmlTree {
                builder: &mut builder,
                resources: &mut resources,
            };
            let name_ada = tree.text("ada");
            let name = tree.element("name", &[], &[name_ada]);
            let item1 = tree.element("item", &[("id", "1")], &[name]);
            let item2 = tree.element("item", &[("id", "2")], &[]);
            let catalog = tree.element("catalog", &[], &[item1, item2]);
            (catalog, item1)
        };
        let mut finalizer = builder.begin_finish(catalog, &mut resources).expect("finalizer");
        let document = match unsafe {
            finalizer.poll_with_source(
                jqf_source::ResolvedSource::new(
                    jqf_source::SourceRef::new(jqf_source::SourceId::new(1), jqf_source::SourceKind::Input),
                    "t.xml",
                    b"<catalog/>",
                    0,
                ),
                &mut resources,
            )
        }
        .expect("finalize")
        {
            jqf_data::DocumentFinalizationPoll::Ready(document) => document,
            jqf_data::DocumentFinalizationPoll::Pending => panic!("tiny document finalized"),
        };
        let root = document.root_handle();
        let deep = document.node_handle(item1).expect("item1 handle");
        (document, root, deep)
    }

    #[test]
    fn xpath_selects_absolute_and_relative() {
        let (document, root) = xml_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::XmlXPath1,
            "//item",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 2);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::XmlXPath1,
            "/catalog/item",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 2);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::XmlXPath1,
            "/catalog/item[1]",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::XmlXPath1,
            "//item[@id='2']",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::XmlXPath1,
            "//item[position() = last()]",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn compile_rejects_outside_the_closed_grammar() {
        assert!(matches!(
            compile(SelectorLanguage::XmlXPath1, "//text()"),
            Err(SelectorError::Compile { .. })
        ));
        assert!(matches!(
            compile(SelectorLanguage::XmlXPath1, "//@id"),
            Err(SelectorError::Compile { .. })
        ));
        // Comparison and function predicates are in-profile, and top-level pure functions answer scalar results; the
        // still-closed surface keeps refusing:
        // an unknown function, an unsupported axis, and a scalar unioned with a path.
        assert!(matches!(
            compile(SelectorLanguage::XmlXPath1, "//item[unknown() = 2]"),
            Err(SelectorError::Compile { .. })
        ));
        assert!(matches!(
            compile(SelectorLanguage::XmlXPath1, "//following-sibling::item"),
            Err(SelectorError::Compile { .. })
        ));
        assert!(matches!(
            compile(SelectorLanguage::XmlXPath1, "count(//item) | //name"),
            Err(SelectorError::Compile { .. })
        ));
    }

    /// Predicate comparisons, the pure functions, and position/last as general numeric atoms all parse and select. The
    /// fixture is `<catalog><item id="1"><name>ada</name></item> <item id="2"/></catalog>` (item 2 has no name child);
    /// each row asserts the selected-node COUNT, which is distinctive per predicate.
    #[test]
    fn predicate_comparisons_and_functions_select() {
        let (document, root) = xml_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        let mut count = |text: &str| -> usize {
            select_text(
                &document,
                root,
                SelectorLanguage::XmlXPath1,
                text,
                budget,
                &mut resources,
            )
            .expect("select")
            .len()
        };
        assert_eq!(count("//item[@id > 0]"), 2);
        assert_eq!(count("//item[@id >= 2]"), 1);
        assert_eq!(count("//item[@id < 2]"), 1);
        assert_eq!(count("//item[@id != 1]"), 1);
        assert_eq!(count("//item[position() <= 1]"), 1);
        assert_eq!(count("//item[position() != last()]"), 1);
        assert_eq!(count("//item[position() >= last()]"), 1);
        assert_eq!(count("//item[count(name) = 1]"), 1);
        assert_eq!(count("//item[string-length(@id) = 1]"), 2);
        assert_eq!(count("//item[concat(@id, \"\") = \"1\"]"), 1);
        assert_eq!(count("//item[name() = \"item\"]"), 2);
        // The comparison atom law: a number literal makes BOTH sides numeric (an unparseable attribute converts to NaN
        // and fails every comparison), while a quoted literal stays a string.
        assert_eq!(count("//item[@id > \"abc\"]"), 0);
        assert_eq!(count("//item[@id > \"1\"]"), 1);
    }

    /// Top-level function widening: a whole-expression function call answers the scalar XPath 1.0 assigns, with the
    /// document node as the context node. The fixture is the same two-item catalog as [`widened_predicates_select`].
    #[test]
    fn top_level_functions_answer_scalars() {
        let (document, root) = xml_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        let mut scalar = |text: &str| -> String {
            let selector = compile(SelectorLanguage::XmlXPath1, text)
                .unwrap_or_else(|error| panic!("compile {text:?}: {error:?}"));
            match select(&document, root, &selector, budget, &mut resources)
                .unwrap_or_else(|error| panic!("select {text:?}: {error:?}"))
            {
                SelectorResult::Elements(_) => panic!("{text:?} must answer a scalar"),
                SelectorResult::Scalar(ScalarResult::Number(number)) => {
                    if number.fract() == 0.0 {
                        alloc::format!("{number:.0}")
                    } else {
                        alloc::format!("{number}")
                    }
                }
                SelectorResult::Scalar(ScalarResult::Text(text)) => text,
            }
        };
        // count is the node-set's cardinality (2 items, 1 with id > 1).
        assert_eq!(scalar("count(//item)"), "2");
        assert_eq!(scalar("count(//item[@id > '1'])"), "1");
        assert_eq!(scalar("count(//item/name)"), "1");
        // concat / string-length are the predicate grammar's own atoms.
        assert_eq!(scalar("concat('x', '-', 'y')"), "x-y");
        assert_eq!(scalar("string-length(concat('ab', 'c'))"), "3");
        // The document node has no QName: XPath's empty answer.
        assert_eq!(scalar("name()"), "");
    }

    #[test]
    fn css_selects_over_the_html_shaped_document() {
        let (document, root) = html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        // The seam is NOT XPath-shaped: html.css@1 executes its own grammar over the same index machinery.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 2);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "#a",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div.y > p",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:first-child",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:nth-child(2)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "html:lang(en)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        // The rest of the static profile: attribute operators, combinators, structural pseudo-classes, :not/:is/:has,
        // :dir, :scope, :empty.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div[class~='x']",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body > div + div",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:not(#a)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:is(.x, .y)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 2);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:has(> p)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:has(p)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            ":scope",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:empty",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:dir(ltr)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:dir(rtl)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
        // :lang ranges: en matches en, en-US, en-GB via prefix; en-* does not match a bare en.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "html:lang(en-*)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
        // Compile rejections outside the static profile.
        for bad in ["div::before", "div:hover", "div:has(> p >)", "div:nth-child(2 of)"] {
            assert!(
                matches!(
                    compile(SelectorLanguage::HtmlCss1, bad),
                    Err(SelectorError::Compile { .. })
                ),
                "{bad} must fail compilation"
            );
        }
        // A forgiving list drops its invalid members: :is() with a valid member and a pseudo-element member still
        // matches.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:is(.y, ::before)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 2);
    }

    /// The of-type family counts SAME-NAMED siblings (the name filter inside `element_position`), so the four
    /// pseudo-classes are not aliases of the `-child` forms.
    #[test]
    fn of_type_counts_same_named_siblings() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        // p.q is body's only `p`, so every of-type form selects it; the `-child` forms count ALL siblings, so they do
        // not.
        for selector in [
            "p.q:first-of-type",
            "p.q:last-of-type",
            "p.q:nth-of-type(1)",
            "p.q:nth-last-of-type(1)",
            "p.q:only-of-type",
        ] {
            let nodes = select_text(
                &document,
                root,
                SelectorLanguage::HtmlCss1,
                selector,
                budget,
                &mut resources,
            )
            .expect("select");
            assert_eq!(nodes.len(), 1, "{selector} must select p.q");
        }
        for selector in ["p.q:first-child", "p.q:nth-child(1)"] {
            let nodes = select_text(
                &document,
                root,
                SelectorLanguage::HtmlCss1,
                selector,
                budget,
                &mut resources,
            )
            .expect("select");
            assert_eq!(nodes.len(), 0, "{selector} must not select p.q");
        }
        // div.y is the SECOND div among body's children; the second child overall is p.q, so the two families must
        // disagree.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:nth-of-type(2)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
    }

    /// Complex-selector matching BACKTRACKS — a stricter combinator to the left of a descendant one re-considers
    /// farther candidates instead of committing to the nearest ancestor.
    #[test]
    fn complex_matching_backtracks() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        // The nearest `div` ancestor of the `p` is div.inner, whose parent is div#a — not body. Only the farther
        // candidate div#a satisfies the `body >` part, so this requires the backtrack.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body > div p",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1, "body > div p must select the p");
        // A longer chain with the same shape: the `body >` still binds the farther div#a, never the nearer div.inner.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "html > body div p",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1, "html > body div p must select the p");
        // Controls: a pure descendant chain (already correct), and a chain that is genuinely false on this tree.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body div p",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body > div > p",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
    }

    /// Whitespace between compounds is the descendant combinator: a `#`/`.`/`[`/`:` seen past whitespace starts the
    /// NEXT compound, never attaches to the previous one (`div .y` is two compounds joined by Descendant).
    #[test]
    fn inter_compound_whitespace_is_the_descendant_combinator() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        // Both `.y` elements are body's DESCENDANTS and body carries no class, so a merged single compound would select
        // nothing.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body .y",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 2, "body .y must select both .y descendants");
        // A boundary inside a longer chain: div.x is div#a, and .inner matches only across the whitespace combinator.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div.x .inner",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        // Without the whitespace the simples bind as ONE compound.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div.x.y",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
    }

    /// A second parentless element breaks the single-root contract the rank pass relies on (two roots would share rank
    /// 0): index build refuses with the internal-contract error instead of mis-ranking.
    #[test]
    fn a_second_root_element_is_an_internal_contract_violation() {
        let (document, root) = html_document_built("no-quirks", |t| {
            let _orphan = t.element("div", &[("class", "orphan")], &[]);
            let body = t.element("body", &[], &[]);
            t.element("html", &[], &[body])
        });
        let mut resources = resources();
        let budget = SelectorBudget::default();
        let result = select_text(&document, root, SelectorLanguage::HtmlCss1, "*", budget, &mut resources);
        assert!(matches!(
            result,
            Err(SelectorError::Internal {
                contract: "single-root element tree",
            })
        ));
    }

    /// A zero-length text leaf is not content: `:empty` ignores it (the Selectors Level 4 emptiness law), while text
    /// with data still counts.
    #[test]
    fn empty_ignores_zero_length_text_leaves() {
        let (document, root) = html_document_built("no-quirks", |t| {
            let vacant_text = t.text("");
            let vacant = t.element("span", &[], &[vacant_text]);
            let full_text = t.text("hi");
            let full = t.element("span", &[], &[full_text]);
            let body = t.element("body", &[], &[vacant, full]);
            t.element("html", &[], &[body])
        });
        let mut resources = resources();
        let budget = SelectorBudget::default();
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "html span:empty",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1, "the span holding an empty text leaf must be :empty");
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "html span:not(:empty)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1, "the span holding text must not be :empty");
    }

    /// `:only-child` reads the parent's cached ELEMENT-child count: the sole element child answers true, a child among
    /// siblings false.
    #[test]
    fn only_child_reads_the_cached_element_child_count() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        // div.inner is div#a's only element child.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "#a > div:only-child",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        // body has three element children (div#a, p.q, div.y): none is an only child.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body > div:only-child",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
    }

    /// `:has()` applies the RELATIVE selector's leading combinator to the anchor — relating the LEFTMOST compound,
    /// not the subject — so the implicit-descendant form is not the only one that works.
    #[test]
    fn has_uses_the_relative_combinator() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        for selector in ["body:has(> div p)", "body:has(div > p)", "body:has(div p)"] {
            let nodes = select_text(
                &document,
                root,
                SelectorLanguage::HtmlCss1,
                selector,
                budget,
                &mut resources,
            )
            .expect("select");
            assert_eq!(nodes.len(), 1, "{selector} must select body");
        }
        // The Child relation discriminates: div#a's direct child is div.inner, not the p.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div#a:has(> p)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div.inner:has(> p)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body:has(> div)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
    }

    /// Quoted non-ASCII attribute values decode as text, not byte-as-char, and CSS escapes decode to code points.
    #[test]
    fn quoted_attributes_decode_utf8() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        for selector in ["p[title=\"café\"]", "p[title=café]", "p[title=\"caf\\e9\"]"] {
            let nodes = select_text(
                &document,
                root,
                SelectorLanguage::HtmlCss1,
                selector,
                budget,
                &mut resources,
            )
            .expect("select");
            assert_eq!(nodes.len(), 1, "{selector} must select p.q");
        }
    }

    /// `[a|=v]` honours the attribute case law — the `equals` closure with the `i`/`s` flags and the HTML
    /// case-insensitive list.
    #[test]
    fn dash_match_honours_case_law() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        // `lang` is HTML case-insensitive, so the dash-match folds case.
        for selector in ["html[lang|=en]", "html[lang|=EN]", "html[lang|=En]"] {
            let nodes = select_text(
                &document,
                root,
                SelectorLanguage::HtmlCss1,
                selector,
                budget,
                &mut resources,
            )
            .expect("select");
            assert_eq!(nodes.len(), 1, "{selector} must select html");
        }
        // The explicit `i` flag forces the same law; the explicit `s` flag pins the sensitive spelling for a
        // case-insensitive attribute.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "html[lang|=EN i]",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "html[lang|=EN s]",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
        // `title` is NOT case-insensitive: the dash-match stays exact.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p[title|=CAFÉ]",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
    }

    /// The forgiving `:is()` recovery survives a stray `]` (the depth decrement has a floor), so the invalid member is
    /// dropped and the valid one kept.
    #[test]
    fn forgiving_list_recovers_after_bracket() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        for selector in [":is(a], p.q)", ":is(&&&, p.q)"] {
            let nodes = select_text(
                &document,
                root,
                SelectorLanguage::HtmlCss1,
                selector,
                budget,
                &mut resources,
            )
            .expect("select");
            assert_eq!(nodes.len(), 1, "{selector} must select p.q");
        }
    }

    /// An+B admits a bare negative integer (`-3` is valid and matches nothing), so `:nth-child(-3)` compiles.
    #[test]
    fn nth_accepts_a_negative_integer() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:nth-child(-3)",
            budget,
            &mut resources,
        )
        .expect("-3 is a valid An+B integer that matches nothing");
        assert_eq!(nodes.len(), 0);
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:nth-child(2n)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
    }

    /// A parentless element (the document element) has no sibling set, so the structural pseudo-classes answer false
    /// instead of raising an internal error.
    #[test]
    fn parentless_elements_do_not_match_structural() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        for selector in ["html:nth-child(1)", "html:first-of-type"] {
            let nodes = select_text(
                &document,
                root,
                SelectorLanguage::HtmlCss1,
                selector,
                budget,
                &mut resources,
            )
            .expect("select");
            assert_eq!(nodes.len(), 0, "{selector} must not match the root");
        }
    }

    /// A budget exhausted inside the `of`-clause sibling scan propagates as `SelectorError::Budget` instead of silently
    /// computing a wrong position.
    #[test]
    fn budget_exhaustion_propagates_from_element_position() {
        let (document, root) = deep_html_document();
        let mut resources = resources();
        // With a generous budget the of-clause answers normally: div.y is the second `.y` sibling of body (after
        // div#a).
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body div:nth-child(2 of .y)",
            SelectorBudget::default(),
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        // A candidate budget that expires inside the sibling scan must raise Budget rather than silently answer.
        // (Charge trace for the selector below: seven candidates for evaluate, then div#a's scan (2), then div.y's scan
        // — max=10 fails on div.y's second sibling, inside element_position.)
        let budget = SelectorBudget {
            max_candidate_tests: 10,
            ..SelectorBudget::default()
        };
        let error = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "body div:nth-child(2 of .x)",
            budget,
            &mut resources,
        )
        .expect_err("exhausted budget must raise");
        assert!(matches!(error, SelectorError::Budget { .. }), "got {error:?}");
    }

    #[test]
    fn css_requires_the_mode_authority() {
        let (document, root) = html_document_with_mode(false);
        let mut resources = resources();
        let error = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div",
            SelectorBudget::default(),
            &mut resources,
        )
        .expect_err("css requires mode authority");
        assert!(matches!(error, SelectorError::MissingModeAuthority), "got {error:?}");
    }

    #[test]
    fn format_mismatch_is_a_named_error() {
        let (document, root) = xml_document();
        let mut resources = resources();
        let error = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "item",
            SelectorBudget::default(),
            &mut resources,
        )
        .expect_err("css over an xml document");
        assert!(matches!(error, SelectorError::FormatMismatch { .. }));
    }

    /// Selectors 4 §14 — with an `of` clause the subject itself must match the listed compounds; its position counts
    /// only within the S-filtered sibling set.
    #[test]
    fn css_nth_child_of_requires_the_subject_to_match_of() {
        let (document, root) = html_document_built("no-quirks", |t| {
            let div_a = t.element("div", &[("class", "a")], &[]);
            let p = t.element("p", &[], &[]);
            let outer = t.element("div", &[], &[div_a, p]);
            let body = t.element("body", &[], &[outer]);
            t.element("html", &[], &[body])
        });
        let mut resources = resources();
        let budget = SelectorBudget::default();
        // p sits at position 1 among the `of .a`-matching siblings but does NOT itself match .a — the pre-fix code
        // answered it anyway.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:nth-child(1 of .a)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0, "subject must itself match the of clause");
        // div.a DOES match .a and is position 1 within the filtered set.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:nth-child(1 of .a)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        // The outer div is a div at the same position but outside .a.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div:nth-child(2 of .a)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
    }

    /// The walk root's own `dir` attribute is the `auto` being resolved — it must not exclude the element's own text
    /// (WHATWG element-directionality excludes only DESCENDANT dir attributes).
    #[test]
    fn css_dir_auto_uses_the_roots_own_text() {
        let (document, root) = html_document_built("no-quirks", |t| {
            let p_heb = t.text("\u{05D0}\u{05D1}\u{05D2}");
            let p = t.element("p", &[("dir", "auto")], &[p_heb]);
            let body = t.element("body", &[], &[p]);
            t.element("html", &[], &[body])
        });
        let mut resources = resources();
        let budget = SelectorBudget::default();
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:dir(rtl)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1, "Hebrew is the first strong char -> Rtl");
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:dir(ltr)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);

        // A descendant element with its own dir stays excluded: the only strong text sits inside span[dir=ltr], so the
        // auto walk finds no strong char and resolves to ltr.
        let (document, root) = html_document_built("no-quirks", |t| {
            let p_heb = t.text("\u{05D0}\u{05D1}\u{05D2}");
            let span = t.element("span", &[("dir", "ltr")], &[p_heb]);
            let p = t.element("p", &[("dir", "auto")], &[span]);
            let body = t.element("body", &[], &[p]);
            t.element("html", &[], &[body])
        });
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:dir(rtl)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0, "span's own dir excludes its subtree");
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "p:dir(ltr)",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
    }

    /// Absolute and leading-`//` XPath results are limited to the scope node and its element descendants (lib.rs's
    /// scope law), even though the paths seed the document node and every element.
    #[test]
    fn xpath_results_are_limited_to_the_scope_domain() {
        let (document, root, deep) = xml_document_deep_scope();
        let mut resources = resources();
        let budget = SelectorBudget::default();
        // Control: the whole-document domain returns both items.
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::XmlXPath1,
            "/catalog/item",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 2);
        // From item#1 the same absolute path returns only in-domain nodes.
        let nodes = select_text(
            &document,
            deep,
            SelectorLanguage::XmlXPath1,
            "/catalog/item",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1, "item#2 is outside the scope domain");
        let nodes = select_text(
            &document,
            deep,
            SelectorLanguage::XmlXPath1,
            "//item",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
        // A descendant of the scope is still served.
        let nodes = select_text(
            &document,
            deep,
            SelectorLanguage::XmlXPath1,
            "/catalog/item/name",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1);
    }

    /// HTML's host law makes id/class matching ASCII case-insensitive in quirks mode and case-sensitive in the
    /// standards modes.
    #[test]
    fn css_id_and_class_are_case_insensitive_in_quirks_mode() {
        let build = |mode: &str| {
            html_document_built(mode, |t| {
                let div = t.element("div", &[("id", "A"), ("class", "X")], &[]);
                let body = t.element("body", &[], &[div]);
                t.element("html", &[], &[body])
            })
        };
        let (document, root) = build("quirks");
        let mut resources = resources();
        let budget = SelectorBudget::default();
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "#a",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1, "quirks: #a matches id=A");
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div.x",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 1, "quirks: div.x matches class=X");

        let (document, root) = build("no-quirks");
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "#a",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0, "standards mode stays case-sensitive");
        let nodes = select_text(
            &document,
            root,
            SelectorLanguage::HtmlCss1,
            "div.x",
            budget,
            &mut resources,
        )
        .expect("select");
        assert_eq!(nodes.len(), 0);
    }

    /// Kernel leaf sorts: a name-fact node is Element; kernel `text` is `is_text_leaf`; a comment node is not.
    #[test]
    fn index_classifies_kernel_leaf_sorts() {
        let mut resources = resources();
        let recipe = jqf_data::DocumentSchemaRecipe::try_new(
            "xml",
            Some("xml"),
            &["xml.element@1", "text", "comment"],
            &["xml.child@1"],
            &["name"],
            &["name"],
        )
        .expect("recipe");
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            recipe.format(),
            recipe.dialect(),
            BuilderCoverage::complete(),
        )
        .expect("builder");
        let text = builder
            .add_node("text", AccountedSemanticNode::String("hi"), None, &mut resources)
            .expect("text");
        let comment = builder
            .add_node("comment", AccountedSemanticNode::String("c"), None, &mut resources)
            .expect("comment");
        let root = builder
            .add_node(
                "xml.element@1",
                AccountedSemanticNode::Array {
                    item_role: "xml.child@1",
                },
                None,
                &mut resources,
            )
            .expect("root");
        builder
            .add_fact(
                jqf_data::LocalOwnerRef::Node(root),
                "name",
                "name",
                1,
                &FactPayload::Text(String::from("r")),
                &mut resources,
            )
            .expect("name");
        for child in [text, comment] {
            builder
                .add_occurrence(
                    jqf_data::LocalOwnerRef::Node(root),
                    "xml.child@1",
                    None,
                    child,
                    &mut resources,
                )
                .expect("child");
        }
        let document = builder.finish(root, &mut resources).expect("finish");
        let index = index::MarkupIndex::build(
            &document,
            SelectorLanguage::XmlXPath1,
            SelectorBudget::default(),
            &mut resources,
        )
        .expect("index");
        assert!(index.is_element(root));
        assert_eq!(index.leaf[root.get() as usize], index::LeafSort::Element);
        assert!(index.is_text_leaf(text));
        assert!(!index.is_text_leaf(comment));
        assert_eq!(index.leaf[comment.get() as usize], index::LeafSort::Comment);
    }

    /// HTML-only mode/pragma roles live on the language, matching the HTML crate literals (no builtins→html
    /// dependency).
    #[test]
    fn html_css_language_holds_codec_mode_and_pragma_roles() {
        assert_eq!(SelectorLanguage::HtmlCss1.mode_role(), Some("html.mode@1"));
        assert_eq!(
            SelectorLanguage::HtmlCss1.pragma_language_role(),
            Some("html.pragma-language@1")
        );
        assert_eq!(SelectorLanguage::XmlXPath1.mode_role(), None);
        assert_eq!(SelectorLanguage::XmlXPath1.pragma_language_role(), None);
    }
}
