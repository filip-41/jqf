//! The WHATWG tree construction algorithm (§13.2.6 of the HTML Standard): insertion modes, implied end tags, foster
//! parenting, the adoption agency, the active formatting elements list, template contents, scripting-disabled parsing,
//! and HTML/SVG/MathML foreign content.
//!
//! The builder owns a node arena and the tokenizer; it drives the tokenizer (switching its text-content states) and
//! answers its adjusted-current-node queries.
//!
//! The scripting flag is DISABLED (`html.document@1` pins scripting-disabled parsing), so the `noscript` element is
//! parsed with the scripting-disabled rules and no script execution ever happens.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ops::Range;

use crate::tokenize::{Attribute, Token, TokenKind, Tokenizer};

/// One element namespace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Namespace {
    /// The HTML namespace.
    Html,
    /// The SVG namespace.
    Svg,
    /// The MathML namespace.
    MathMl,
}

/// One arena node.
pub struct Node {
    /// The node kind.
    pub kind: NodeKind,
    /// The ordered children.
    pub children: Vec<NodeId>,
    /// The parent node.
    pub parent: Option<NodeId>,
    /// The element name (adjusted), or empty for non-elements.
    pub name: String,
    /// The element namespace.
    pub ns: Namespace,
    /// The recovered attributes.
    pub attrs: Vec<Attribute>,
    /// The text or comment data (for Text/Comment nodes).
    pub data: String,
    /// The doctype fields (for the Doctype node).
    pub doctype: Option<DoctypeData>,
}

/// The recovered doctype facts.
#[derive(Clone, Debug)]
pub struct DoctypeData {
    /// The doctype name.
    pub name: Option<String>,
    /// The public identifier.
    pub public_identifier: Option<String>,
    /// The system identifier.
    pub system_identifier: Option<String>,
    /// The token's force-quirks flag.
    pub force_quirks: bool,
}

/// One arena node kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeKind {
    /// The document node.
    Document,
    /// An element.
    Element,
    /// A text node.
    Text,
    /// A comment node.
    Comment,
    /// The doctype node.
    Doctype,
}

/// The document-local node identity (dense arena index).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeId(pub usize);

/// The recovered document mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuirksMode {
    /// Standards mode.
    NoQuirks,
    /// Limited quirks (the transitional doctypes).
    LimitedQuirks,
    /// Quirks mode.
    Quirks,
}

/// The active formatting elements list entries.
enum FormattingEntry {
    Element(NodeId),
    Marker,
}

/// The insertion modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertionMode {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    AfterHead,
    InBody,
    Text,
    InTable,
    InTableText,
    InCaption,
    InColumnGroup,
    InTableBody,
    InRow,
    InCell,
    InSelect,
    InSelectInTable,
    InTemplate,
    AfterBody,
    InFrameset,
    AfterFrameset,
    AfterAfterBody,
    AfterAfterFrameset,
    InForeignContent,
}

/// The complete recovered tree.
pub struct Tree {
    /// The node arena.
    pub nodes: Vec<Node>,
    /// The document node.
    pub document: NodeId,
    /// The recovered document mode.
    pub quirks: QuirksMode,
    /// The pragma-set default language, when one was set.
    pub pragma_language: Option<String>,
}

impl Tree {
    /// The tree's document element. A full document's `document` node is a Document whose child is the html element; a
    /// FRAGMENT's `document` node IS the bare html root element (`build_fragment`'s design — the `#document-fragment`
    /// dump law), so every projection must accept both shapes.
    pub(crate) fn document_element(&self) -> Option<NodeId> {
        let document = &self.nodes[self.document.0];
        if document.kind == NodeKind::Element {
            Some(self.document)
        } else {
            document
                .children
                .iter()
                .find(|child| self.nodes[child.0].kind == NodeKind::Element)
                .copied()
        }
    }
}

/// The element names whose end tags can be implied (the "implied end tags").
const IMPLIED_END_TAGS: &[&str] = &["dd", "dt", "li", "optgroup", "option", "p", "rb", "rp", "rt", "rtc"];

/// The heading elements.
fn is_heading(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

/// The scoping category (the "has an element in scope" barriers — the scopingElements list, NOT the special list). The
/// scoping test including the SVG foreignObject (the WHATWG table's scoping list has the (svg, foreignObject) entry).
fn is_scoping_in(element: &Node) -> bool {
    // The spec's scope algorithm: the SVG integration points (foreignObject, desc, title) and the MathML text
    // integration points (mi/mo/mn/ms/mtext) plus annotation-xml are scope boundaries — the corpus pins it: a `<p>`
    // inside `<math><mi>` must NOT close the p below the math, while a `</p>` under a plain `<math>` still closes it
    // ([82] vs [31-35]).
    if element.ns == Namespace::Svg {
        return matches!(element.name.as_str(), "foreignObject" | "desc" | "title");
    }
    if element.ns == Namespace::MathMl {
        return matches!(
            element.name.as_str(),
            "mi" | "mo" | "mn" | "ms" | "mtext" | "annotation-xml"
        );
    }
    element.ns == Namespace::Html && is_scoping(&element.name)
}

fn is_scoping(name: &str) -> bool {
    matches!(
        name,
        "applet" | "caption" | "html" | "marquee" | "object" | "table" | "td" | "th" | "template"
    )
}

/// The special category (the furthest-block and implied-closing rules). The "special" category (the WHATWG table): the
/// HTML list plus the SVG foreignObject/desc/title elements.
fn is_special_in(element: &Node) -> bool {
    if element.ns == Namespace::Svg {
        return matches!(element.name.as_str(), "foreignObject" | "desc" | "title");
    }
    element.ns == Namespace::Html && is_special(&element.name)
}

fn is_special(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "applet"
            | "area"
            | "article"
            | "aside"
            | "base"
            | "basefont"
            | "bgsound"
            | "blockquote"
            | "body"
            | "br"
            | "button"
            | "caption"
            | "center"
            | "col"
            | "colgroup"
            | "dd"
            | "details"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "embed"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "frame"
            | "frameset"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hgroup"
            | "hr"
            | "html"
            | "iframe"
            | "img"
            | "input"
            | "keygen"
            | "li"
            | "link"
            | "listing"
            | "main"
            | "marquee"
            | "menu"
            | "meta"
            | "nav"
            | "noembed"
            | "noframes"
            | "noscript"
            | "object"
            | "ol"
            | "p"
            | "param"
            | "plaintext"
            | "pre"
            | "script"
            | "search"
            | "section"
            | "select"
            | "source"
            | "style"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "template"
            | "textarea"
            | "tfoot"
            | "th"
            | "thead"
            | "title"
            | "tr"
            | "track"
            | "ul"
            | "wbr"
            | "xmp"
    )
}

/// The SVG tag-name adjustments (the WHATWG table).
fn adjust_svg_name(name: &str) -> String {
    match name {
        "altglyph" => "altGlyph".to_string(),
        "altglyphdef" => "altGlyphDef".to_string(),
        "altglyphitem" => "altGlyphItem".to_string(),
        "animatecolor" => "animateColor".to_string(),
        "animatemotion" => "animateMotion".to_string(),
        "animatetransform" => "animateTransform".to_string(),
        "clippath" => "clipPath".to_string(),
        "feblend" => "feBlend".to_string(),
        "fecolormatrix" => "feColorMatrix".to_string(),
        "fecomponenttransfer" => "feComponentTransfer".to_string(),
        "fecomposite" => "feComposite".to_string(),
        "feconvolvematrix" => "feConvolveMatrix".to_string(),
        "fediffuselighting" => "feDiffuseLighting".to_string(),
        "fedisplacementmap" => "feDisplacementMap".to_string(),
        "fedistantlight" => "feDistantLight".to_string(),
        "fedropshadow" => "feDropShadow".to_string(),
        "feflood" => "feFlood".to_string(),
        "fefunca" => "feFuncA".to_string(),
        "fefuncb" => "feFuncB".to_string(),
        "fefuncg" => "feFuncG".to_string(),
        "fefuncr" => "feFuncR".to_string(),
        "fegaussianblur" => "feGaussianBlur".to_string(),
        "feimage" => "feImage".to_string(),
        "femerge" => "feMerge".to_string(),
        "femergenode" => "feMergeNode".to_string(),
        "femorphology" => "feMorphology".to_string(),
        "feoffset" => "feOffset".to_string(),
        "fepointlight" => "fePointLight".to_string(),
        "fespecularlighting" => "feSpecularLighting".to_string(),
        "fespotlight" => "feSpotLight".to_string(),
        "fetile" => "feTile".to_string(),
        "feturbulence" => "feTurbulence".to_string(),
        "foreignobject" => "foreignObject".to_string(),
        "glyphref" => "glyphRef".to_string(),
        "lineargradient" => "linearGradient".to_string(),
        "radialgradient" => "radialGradient".to_string(),
        "textpath" => "textPath".to_string(),
        _ => name.to_string(),
    }
}

/// The MathML attribute-name adjustments (the WHATWG table). The HTML integration points (the WHATWG table): the SVG
/// foreignObject, desc, and title elements.
fn is_html_integration_point(element: &Node) -> bool {
    // The stored name is the ADJUSTED SVG spelling (foreignObject).
    if element.ns == Namespace::Svg {
        return matches!(element.name.as_str(), "foreignObject" | "desc" | "title");
    }
    // The MathML annotation-xml with an xhtml/html encoding is an HTML integration point (the corpus pins it:
    // `<math><annotation-xml encoding="application/xhtml+xml"><div>` keeps the div inside).
    if element.ns == Namespace::MathMl && element.name == "annotation-xml" {
        return element.attrs.iter().any(|attribute| {
            attribute.name == "encoding"
                && matches!(
                    attribute.value.to_ascii_lowercase().as_str(),
                    "text/html" | "application/xhtml+xml"
                )
        });
    }
    false
}

/// The MathML text integration points (the WHATWG table).
fn is_mathml_text_integration_point(element: &Node) -> bool {
    element.ns == Namespace::MathMl && matches!(element.name.as_str(), "mi" | "mo" | "mn" | "ms" | "mtext")
}

fn adjust_mathml_attr(name: &str) -> String {
    match name {
        "definitionurl" => "definitionURL".to_string(),
        _ => name.to_string(),
    }
}

/// The SVG attribute-name adjustments (the WHATWG table).
fn adjust_svg_attr(name: &str) -> String {
    match name {
        "attributename" => "attributeName".to_string(),
        "attributetype" => "attributeType".to_string(),
        "basefrequency" => "baseFrequency".to_string(),
        "baseprofile" => "baseProfile".to_string(),
        "calcmode" => "calcMode".to_string(),
        "clippathunits" => "clipPathUnits".to_string(),
        "contentscripttype" => "contentScriptType".to_string(),
        "contentstyletype" => "contentStyleType".to_string(),
        "diffuseconstant" => "diffuseConstant".to_string(),
        "edgemode" => "edgeMode".to_string(),
        "externalresourcesrequired" => "externalResourcesRequired".to_string(),
        "filterres" => "filterRes".to_string(),
        "filterunits" => "filterUnits".to_string(),
        "glyphref" => "glyphRef".to_string(),
        "gradienttransform" => "gradientTransform".to_string(),
        "gradientunits" => "gradientUnits".to_string(),
        "kernelmatrix" => "kernelMatrix".to_string(),
        "kernelunitlength" => "kernelUnitLength".to_string(),
        "keypoints" => "keyPoints".to_string(),
        "keysplines" => "keySplines".to_string(),
        "keytimes" => "keyTimes".to_string(),
        "lengthadjust" => "lengthAdjust".to_string(),
        "limitingconeangle" => "limitingConeAngle".to_string(),
        "markerheight" => "markerHeight".to_string(),
        "markerunits" => "markerUnits".to_string(),
        "markerwidth" => "markerWidth".to_string(),
        "maskcontentunits" => "maskContentUnits".to_string(),
        "maskunits" => "maskUnits".to_string(),
        "numoctaves" => "numOctaves".to_string(),
        "pathlength" => "pathLength".to_string(),
        "patterncontentunits" => "patternContentUnits".to_string(),
        "patterntransform" => "patternTransform".to_string(),
        "patternunits" => "patternUnits".to_string(),
        "pointsatx" => "pointsAtX".to_string(),
        "pointsaty" => "pointsAtY".to_string(),
        "pointsatz" => "pointsAtZ".to_string(),
        "preservealpha" => "preserveAlpha".to_string(),
        "preserveaspectratio" => "preserveAspectRatio".to_string(),
        "primitiveunits" => "primitiveUnits".to_string(),
        "refx" => "refX".to_string(),
        "refy" => "refY".to_string(),
        "repeatcount" => "repeatCount".to_string(),
        "repeatdur" => "repeatDur".to_string(),
        "requiredextensions" => "requiredExtensions".to_string(),
        "requiredfeatures" => "requiredFeatures".to_string(),
        "specularconstant" => "specularConstant".to_string(),
        "specularexponent" => "specularExponent".to_string(),
        "spreadmethod" => "spreadMethod".to_string(),
        "startoffset" => "startOffset".to_string(),
        "stddeviation" => "stdDeviation".to_string(),
        "stitchtiles" => "stitchTiles".to_string(),
        "surfacescale" => "surfaceScale".to_string(),
        "systemlanguage" => "systemLanguage".to_string(),
        "tablevalues" => "tableValues".to_string(),
        "targetx" => "targetX".to_string(),
        "targety" => "targetY".to_string(),
        "textlength" => "textLength".to_string(),
        "viewbox" => "viewBox".to_string(),
        "viewtarget" => "viewTarget".to_string(),
        "xchannelselector" => "xChannelSelector".to_string(),
        "ychannelselector" => "yChannelSelector".to_string(),
        "zoomandpan" => "zoomAndPan".to_string(),
        _ => name.to_string(),
    }
}

/// Buckets in the open-element name filter. A power of two, so the bucket index is a mask rather than a division.
const OPEN_NAME_BUCKETS: usize = 64;

/// The filter bucket for an element name.
///
/// Any hash is sound here: a collision only costs a walk that the unfiltered code would have run anyway. The
/// requirement is the other direction — two equal names must land in the same bucket — which any pure function of the
/// bytes satisfies.
fn open_name_bucket(name: &str) -> usize {
    let bytes = name.as_bytes();
    let first = usize::from(bytes.first().copied().unwrap_or(0));
    let last = usize::from(bytes.last().copied().unwrap_or(0));
    (first.wrapping_mul(31) ^ last.wrapping_mul(7) ^ bytes.len()) & (OPEN_NAME_BUCKETS - 1)
}

/// The tree builder proper.
pub struct TreeBuilder {
    /// The node arena.
    pub nodes: Vec<Node>,
    /// The document node.
    pub document: NodeId,
    open_elements: Vec<NodeId>,
    /// How many HTML-namespace elements of each name-hash are on `open_elements` right now — the scope algorithms'
    /// absence test.
    ///
    /// Every "has an element in scope" variant answers TRUE only on an HTML-namespace name match, so a name whose
    /// bucket is empty cannot be found and the walk can be skipped. Without this, a `<div>` start tag (which must close
    /// a `p` in button scope) walks the WHOLE stack every time, because neither `div` nor anything else in a nested-div
    /// chain is a scope boundary — that is O(depth) per tag and O(depth²) per document. Measured before the filter: 40
    /// chains of 2 000 nested divs took 0.19 s and the same shape at depth 8 000 took 2.21 s (11.8× for 4× the input);
    /// the `span` twin, which has no p-closing clause, was linear at the same sizes.
    ///
    /// INVARIANT: `open_elements` is mutated ONLY through `push_open`, `pop_open`, `remove_open`, `insert_open` and
    /// `retain_open`, which are what keep this true. A stale count can only ever cause a walk that would have run
    /// anyway, never a wrong answer — but only if it over-counts, so nothing may pop the stack behind their backs.
    open_name_filter: [u32; OPEN_NAME_BUCKETS],
    active_formatting: Vec<FormattingEntry>,
    insertion_mode: InsertionMode,
    original_insertion_mode: InsertionMode,
    template_modes: Vec<InsertionMode>,
    head_pointer: Option<NodeId>,
    /// True while a foreign delegation is running its target-mode handler (the per-token selection must not re-enter
    /// the foreign rules).
    delegating: bool,
    /// The pre/listing/textarea drop-newline flag: the first whitespace token after such a start tag drops one leading
    /// line feed.
    drop_next_newline: bool,
    /// The mode that was current before the OUTERMOST anything-else delegation (the mode a template or foreign element
    /// should treat as its origin — the phase is never changed by a delegation).
    delegation_origin: Option<InsertionMode>,
    /// The insertion mode that was current when foreign content was entered (the phase law: the foreign phase is a
    /// per-token overlay, and a breakout reprocesses through the mode that was current BEFORE the foreign element).
    foreign_origin_mode: Option<InsertionMode>,
    form_pointer: Option<NodeId>,
    frameset_ok: bool,
    foster_parenting: bool,
    quirks: QuirksMode,
    pragma_language: Option<String>,
    /// The pending table character tokens ("in table text" mode).
    pending_table_tokens: Vec<(String, Range<usize>)>,
    /// The fragment context element name, when this parse is the WHATWG HTML fragment parsing algorithm (12.4) rather
    /// than a full document parse. The fragment root is a bare `html` element; the fragment's content is its children.
    fragment_context: Option<String>,
    /// The cooperative whole-document session tokenizer, when a poll drive is active.
    cooperative_tokenizer: Option<Tokenizer>,
    /// The cooperative drive saw tokenizer EOF and awaits `process_eof`.
    cooperative_eof: bool,
}

impl TreeBuilder {
    /// Builds the recovered tree for one decoded input. The decoded text is MOVED in (the tokenizer owns it); a
    /// borrowed caller converts.
    pub fn build(input: impl Into<String>) -> Tree {
        let mut builder = TreeBuilder::new();
        builder.run(input.into());
        builder.finish_tree()
    }

    /// Builds the recovered tree for one HTML fragment under the WHATWG fragment parsing algorithm (12.4): the fragment
    /// context element names the starting insertion mode (reset-the-insertion-mode-appropriately's fragment case) and
    /// the tokenizer's initial state (RCDATA/RAWTEXT/PLAINTEXT for textarea/title, style-family, and plaintext
    /// contexts), and a bare `html` root element holds the content.
    ///
    /// The returned tree's `document` is that ROOT element — the fragment is the root's children, exactly what the
    /// tree-construction `#document-fragment` expectations dump.
    pub fn build_fragment(input: impl Into<String>, context: &str) -> Tree {
        let mut builder = TreeBuilder::new();
        let mut tokenizer = Tokenizer::new(input.into());
        let root = builder.setup_fragment(context, &mut tokenizer);
        builder.run_with(tokenizer);
        Tree {
            nodes: builder.nodes,
            document: root,
            quirks: builder.quirks,
            pragma_language: builder.pragma_language,
        }
    }

    /// The fragment case of reset-the-insertion-mode-appropriately: the open-elements stack holds only the bare html
    /// root, so the walk sets last to true immediately and substitutes the CONTEXT element. The context's name is
    /// matched against the same table; "last is true" makes td/th and select fall through to "in body". Prepares the
    /// WHATWG fragment parsing algorithm (12.4) on a fresh builder and returns the bare `html` root element.
    fn setup_fragment(&mut self, context: &str, tokenizer: &mut Tokenizer) -> NodeId {
        self.fragment_context = Some(context.to_string());
        let root = self.new_element("html".to_string(), Namespace::Html, Vec::new());
        self.append_child(self.document, root);
        self.push_open(root);
        self.reset_insertion_mode_fragment();
        tokenizer.enter_text_mode(context);
        root
    }

    fn reset_insertion_mode_fragment(&mut self) {
        let context = self.fragment_context.as_deref().unwrap_or("div");
        self.insertion_mode = match context {
            "select" => InsertionMode::InBody,
            "td" | "th" => InsertionMode::InBody,
            "tr" => InsertionMode::InRow,
            "tbody" | "thead" | "tfoot" => InsertionMode::InTableBody,
            "caption" => InsertionMode::InCaption,
            "colgroup" => InsertionMode::InColumnGroup,
            "table" => InsertionMode::InTable,
            "template" => InsertionMode::InTemplate,
            "body" => InsertionMode::InBody,
            "frameset" => InsertionMode::InFrameset,
            "html" => {
                if self.head_pointer.is_none() {
                    InsertionMode::BeforeHead
                } else {
                    InsertionMode::AfterHead
                }
            }
            _ => InsertionMode::InBody,
        };
    }

    fn new() -> Self {
        let mut nodes = Vec::new();
        nodes.push(Node {
            kind: NodeKind::Document,
            children: Vec::new(),
            parent: None,
            name: String::new(),
            ns: Namespace::Html,
            attrs: Vec::new(),
            data: String::new(),
            doctype: None,
        });
        Self {
            document: NodeId(0),
            nodes,
            open_elements: Vec::new(),
            open_name_filter: [0; OPEN_NAME_BUCKETS],
            active_formatting: Vec::new(),
            insertion_mode: InsertionMode::Initial,
            original_insertion_mode: InsertionMode::Initial,
            template_modes: Vec::new(),
            head_pointer: None,
            foreign_origin_mode: None,
            delegation_origin: None,
            drop_next_newline: false,
            delegating: false,
            form_pointer: None,
            frameset_ok: true,
            foster_parenting: false,
            quirks: QuirksMode::Quirks,
            pragma_language: None,
            pending_table_tokens: Vec::new(),
            fragment_context: None,
            cooperative_tokenizer: None,
            cooperative_eof: false,
        }
    }

    fn refresh_tokenizer_foreign_content(&self, tokenizer: &mut Tokenizer) {
        tokenizer.set_foreign_content(
            self.adjusted_current_node()
                .is_some_and(|node| self.nodes[node.0].ns != Namespace::Html),
        );
    }

    fn maybe_enter_html_text_mode(&mut self, tokenizer: &mut Tokenizer, name: &str) {
        let html_namespace = self
            .adjusted_current_node()
            .is_some_and(|node| self.nodes[node.0].ns == Namespace::Html);
        if html_namespace
            && matches!(
                name,
                "title" | "textarea" | "style" | "xmp" | "iframe" | "noembed" | "noframes" | "script" | "plaintext"
            )
        {
            tokenizer.enter_text_mode(name);
        }
    }

    fn run(&mut self, input: String) {
        let tokenizer = Tokenizer::new(input);
        self.run_with(tokenizer);
    }

    fn run_with(&mut self, mut tokenizer: Tokenizer) {
        loop {
            self.refresh_tokenizer_foreign_content(&mut tokenizer);
            let Some(token) = tokenizer.next_token() else {
                break;
            };
            if let TokenKind::StartTag { name, .. } = &token.kind {
                self.maybe_enter_html_text_mode(&mut tokenizer, name);
            }
            self.process_token(&token);
        }
        self.process_eof();
    }

    /// Collects the finished tree out of the builder (the fields are replaced with fresh empties so a cooperative drive
    /// can finish a `&mut` builder).
    fn finish_tree(&mut self) -> Tree {
        Tree {
            nodes: core::mem::take(&mut self.nodes),
            document: core::mem::replace(&mut self.document, NodeId(0)),
            quirks: self.quirks,
            pragma_language: self.pragma_language.take(),
        }
    }
}

/// One cooperative-parse observation.
pub(crate) enum TreeBuildPoll {
    /// The cooperative entry's work credits are spent; re-poll after the caller replenishes.
    Pending,
    /// The parse completed and the tree is ready.
    Ready(Tree),
}

impl TreeBuilder {
    /// Starts a cooperative whole-document tokenize + tree-construction drive. The decoded text is owned by the
    /// tokenizer until [`Self::poll_cooperative`] returns [`TreeBuildPoll::Ready`].
    pub(crate) fn begin_cooperative(text: String, fragment: bool) -> Self {
        let mut builder = TreeBuilder::new();
        let mut tokenizer = Tokenizer::new(text);
        if fragment {
            builder.setup_fragment(crate::FRAGMENT_DEFAULT_CONTEXT, &mut tokenizer);
        }
        builder.cooperative_tokenizer = Some(tokenizer);
        builder
    }

    /// Drives one cooperative step: an admission check, one tokenizer step, and the produced tokens through the tree
    /// builder.
    pub(crate) fn poll_cooperative(
        &mut self,
        resources: &mut jqf_resource::ResourceContext<'_>,
    ) -> Result<TreeBuildPoll, jqf_codec_core::CodecError> {
        loop {
            if resources.admit_work_transition()? == jqf_resource::WorkAdmission::Pending {
                return Ok(TreeBuildPoll::Pending);
            }
            if self.cooperative_eof {
                self.process_eof();
                self.cooperative_tokenizer = None;
                return Ok(TreeBuildPoll::Ready(self.finish_tree()));
            }
            let mut tokenizer = self.cooperative_tokenizer.take().ok_or_else(cooperative_contract)?;
            self.refresh_tokenizer_foreign_content(&mut tokenizer);
            let mut out = Vec::new();
            tokenizer.step(&mut out);
            for token in out {
                if let TokenKind::StartTag { name, .. } = &token.kind {
                    self.maybe_enter_html_text_mode(&mut tokenizer, name);
                }
                self.process_token(&token);
            }
            if tokenizer.at_eof() {
                self.cooperative_eof = true;
            }
            self.cooperative_tokenizer = Some(tokenizer);
        }
    }
}

fn cooperative_contract() -> jqf_codec_core::CodecError {
    jqf_codec_core::data_contract("HTML cooperative tree build missing tokenizer")
}

impl TreeBuilder {
    /// Processes one token per the insertion mode.
    fn process_token(&mut self, token: &Token) {
        match &token.kind {
            TokenKind::Character { data } => self.process_characters(data, token.span.clone()),
            TokenKind::StartTag {
                name,
                attributes,
                self_closing,
            } => self.process_start_tag(name, attributes, *self_closing, token.span.clone()),
            TokenKind::EndTag { name } => self.process_end_tag(name, token.span.clone()),
            TokenKind::Comment { data } => self.process_comment(data, token.span.clone()),
            TokenKind::Doctype {
                name,
                public_identifier,
                system_identifier,
                force_quirks,
                correct: _,
            } => self.process_doctype(
                name.clone(),
                public_identifier.clone(),
                system_identifier.clone(),
                *force_quirks,
                token.span.clone(),
            ),
            TokenKind::Eof => {}
        }
    }

    fn process_eof(&mut self) {
        self.process_eof_by_mode();
    }

    // ------------------------------------------------------------------ Node helpers
    // ------------------------------------------------------------------

    fn new_element(&mut self, name: String, ns: Namespace, attrs: Vec<Attribute>) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node {
            kind: NodeKind::Element,
            children: Vec::new(),
            parent: None,
            name,
            ns,
            attrs,
            data: String::new(),
            doctype: None,
        });
        id
    }

    fn new_text(&mut self, data: String) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node {
            kind: NodeKind::Text,
            children: Vec::new(),
            parent: None,
            name: String::new(),
            ns: Namespace::Html,
            attrs: Vec::new(),
            data,
            doctype: None,
        });
        id
    }

    fn new_comment(&mut self, data: String) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node {
            kind: NodeKind::Comment,
            children: Vec::new(),
            parent: None,
            name: String::new(),
            ns: Namespace::Html,
            attrs: Vec::new(),
            data,
            doctype: None,
        });
        id
    }

    fn new_doctype(&mut self, doctype: DoctypeData) -> NodeId {
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node {
            kind: NodeKind::Doctype,
            children: Vec::new(),
            parent: None,
            name: String::new(),
            ns: Namespace::Html,
            attrs: Vec::new(),
            data: String::new(),
            doctype: Some(doctype),
        });
        id
    }

    fn append_child(&mut self, parent: NodeId, child: NodeId) {
        self.nodes[parent.0].children.push(child);
        self.nodes[child.0].parent = Some(parent);
    }

    /// Removes one node from its parent's children (the tree law).
    fn remove_from_parent(&mut self, node: NodeId) {
        if let Some(parent) = self.nodes[node.0].parent {
            self.nodes[parent.0].children.retain(|entry| *entry != node);
            self.nodes[node.0].parent = None;
        }
    }

    fn insert_at(&mut self, parent: NodeId, index: usize, child: NodeId) {
        self.nodes[parent.0].children.insert(index, child);
        self.nodes[child.0].parent = Some(parent);
    }

    /// The mainLoop selection test: the adjusted current node is a foreign element that is NOT an integration point
    /// (integration points route through the current insertion mode's rules).
    fn foreign_current(&self) -> bool {
        if self.delegating || self.insertion_mode == InsertionMode::InForeignContent {
            return false;
        }
        self.adjusted_current_node().is_some_and(|node| {
            let element = &self.nodes[node.0];
            element.ns != Namespace::Html
                && !is_html_integration_point(element)
                && !is_mathml_text_integration_point(element)
        })
    }

    /// The current node (the top of the stack of open elements).
    fn current_node(&self) -> Option<NodeId> {
        self.open_elements.last().copied()
    }

    /// The name filter's bucket for a node, or `None` for a foreign element (the scope algorithms never match one, so
    /// they are not counted).
    fn open_filter_bucket(&self, node: NodeId) -> Option<usize> {
        let element = &self.nodes[node.0];
        (element.ns == Namespace::Html).then(|| open_name_bucket(&element.name))
    }

    /// Pushes onto the stack of open elements.
    fn push_open(&mut self, node: NodeId) {
        if let Some(bucket) = self.open_filter_bucket(node) {
            self.open_name_filter[bucket] += 1;
        }
        self.open_elements.push(node);
    }

    /// Pops the stack of open elements.
    fn pop_open(&mut self) -> Option<NodeId> {
        let node = self.open_elements.pop()?;
        self.forget_open(node);
        Some(node)
    }

    /// Removes the element at `index` from the stack of open elements.
    fn remove_open(&mut self, index: usize) {
        let node = self.open_elements.remove(index);
        self.forget_open(node);
    }

    /// Inserts `node` into the stack of open elements at `index`.
    fn insert_open(&mut self, index: usize, node: NodeId) {
        if let Some(bucket) = self.open_filter_bucket(node) {
            self.open_name_filter[bucket] += 1;
        }
        self.open_elements.insert(index, node);
    }

    /// Drops every open element the predicate rejects. The callers remove one element each, so the quadratic shape of
    /// the removal loop is not one.
    fn retain_open(&mut self, keep: impl Fn(NodeId) -> bool) {
        let mut index = 0;
        while index < self.open_elements.len() {
            if keep(self.open_elements[index]) {
                index += 1;
            } else {
                self.remove_open(index);
            }
        }
    }

    /// Drops `node`'s contribution to the name filter.
    fn forget_open(&mut self, node: NodeId) {
        if let Some(bucket) = self.open_filter_bucket(node) {
            self.open_name_filter[bucket] -= 1;
        }
    }

    /// Whether any HTML-namespace element of this name can be on the stack. False is authoritative; true means "walk
    /// and see".
    fn open_name_possible(&self, name: &str) -> bool {
        self.open_name_filter[open_name_bucket(name)] > 0
    }

    /// The adjusted current node (the template contents' top when in a template).
    fn adjusted_current_node(&self) -> Option<NodeId> {
        if self.open_elements.is_empty() {
            return Some(self.document);
        }
        if self.nodes[self.open_elements[0].0].name == "template" {
            if let Some(template) = self.template_contents_top() {
                return Some(template);
            }
        }
        self.current_node()
    }

    /// The top of the template contents stack (the template element's children holder — in this model, the template
    /// element itself is the contents container, matching the dump).
    fn template_contents_top(&self) -> Option<NodeId> {
        self.open_elements
            .iter()
            .rev()
            .find(|node| self.nodes[node.0].name == "template")
            .copied()
    }

    /// The element in scope test (the "has an element in scope" algorithm).
    fn has_in_scope(&self, name: &str) -> bool {
        if !self.open_name_possible(name) {
            return false;
        }
        for node in self.open_elements.iter().rev() {
            let element = &self.nodes[node.0];
            if element.name == name && element.ns == Namespace::Html {
                return true;
            }
            if is_scoping_in(element) {
                return false;
            }
        }
        false
    }

    /// The list-item scope test (adds `ol`/`ul` as boundaries).
    fn has_in_list_item_scope(&self, name: &str) -> bool {
        if !self.open_name_possible(name) {
            return false;
        }
        for node in self.open_elements.iter().rev() {
            let element = &self.nodes[node.0];
            if element.name == name && element.ns == Namespace::Html {
                return true;
            }
            if is_scoping_in(element) || (element.ns == Namespace::Html && matches!(element.name.as_str(), "ol" | "ul"))
            {
                return false;
            }
        }
        false
    }

    /// The button scope test (adds `button` as a boundary).
    fn has_in_button_scope(&self, name: &str) -> bool {
        if !self.open_name_possible(name) {
            return false;
        }
        for node in self.open_elements.iter().rev() {
            let element = &self.nodes[node.0];
            if element.name == name && element.ns == Namespace::Html {
                return true;
            }
            // the law: only the scoping elements and button stop the walk — a plain FOREIGN element does not (the
            // corpus pins it: `</p>` inside `<math>` still closes the p below).
            if is_scoping_in(element) || (element.ns == Namespace::Html && element.name == "button") {
                return false;
            }
        }
        false
    }

    /// The table scope test.
    fn has_in_table_scope(&self, name: &str) -> bool {
        if !self.open_name_possible(name) {
            return false;
        }
        for node in self.open_elements.iter().rev() {
            let element = &self.nodes[node.0];
            if element.name == name && element.ns == Namespace::Html {
                return true;
            }
            if element.ns == Namespace::Html && matches!(element.name.as_str(), "html" | "table" | "template") {
                return false;
            }
        }
        false
    }

    /// The select scope test.
    fn has_in_select_scope(&self, name: &str) -> bool {
        if !self.open_name_possible(name) {
            return false;
        }
        for node in self.open_elements.iter().rev() {
            let element = &self.nodes[node.0];
            if element.name == name && element.ns == Namespace::Html {
                return true;
            }
            if element.ns == Namespace::Html && !matches!(element.name.as_str(), "optgroup" | "option") {
                return false;
            }
        }
        false
    }

    /// Generates implied end tags (the "generate implied end tags" algo).
    fn generate_implied_end_tags(&mut self, except: Option<&str>) {
        while let Some(node) = self.current_node() {
            let name = self.nodes[node.0].name.clone();
            if except.is_some_and(|except| except == name) {
                break;
            }
            if IMPLIED_END_TAGS.contains(&name.as_str()) {
                self.pop_open_element();
            } else {
                break;
            }
        }
    }

    /// Pops one open element.
    fn pop_open_element(&mut self) {
        self.pop_open();
    }

    /// Pops elements until (and including) the named element.
    fn pop_until(&mut self, name: &str) {
        while let Some(node) = self.pop_open() {
            if self.nodes[node.0].name == name {
                break;
            }
        }
    }

    /// Pops elements until (and including) the named element, clearing the active formatting elements to the last
    /// marker (the "clear the list back to a marker" algorithm).
    fn clear_active_formatting_to_marker(&mut self) {
        while let Some(entry) = self.active_formatting.pop() {
            if matches!(entry, FormattingEntry::Marker) {
                break;
            }
        }
    }

    // ------------------------------------------------------------------ The token processors
    // ------------------------------------------------------------------

    fn process_characters(&mut self, data: &str, span: Range<usize>) {
        if data.is_empty() {
            return;
        }
        // Same selection law: foreign characters under a non-integration foreign current node run the foreign rules.
        if self.foreign_current() {
            self.process_characters_in_foreign(data, span);
            return;
        }
        let mode = self.insertion_mode;
        match mode {
            InsertionMode::Initial
            | InsertionMode::BeforeHtml
            | InsertionMode::BeforeHead
            | InsertionMode::InHead
            | InsertionMode::AfterHead
            | InsertionMode::InCaption
            | InsertionMode::InColumnGroup
            | InsertionMode::InTableBody
            | InsertionMode::InRow
            | InsertionMode::InCell
            | InsertionMode::InSelect
            | InsertionMode::AfterBody
            | InsertionMode::InFrameset
            | InsertionMode::AfterFrameset
            | InsertionMode::AfterAfterBody
            | InsertionMode::AfterAfterFrameset => {
                self.process_characters_by_mode(data, span);
            }
            InsertionMode::InBody => {
                self.process_characters_in_body(data, span);
            }
            InsertionMode::InForeignContent => {
                self.process_characters_in_foreign(data, span);
            }
            InsertionMode::Text => {
                self.process_characters_in_text(data, span);
            }
            InsertionMode::InTable | InsertionMode::InTableText => {
                self.process_characters_in_table(data, span);
            }
            InsertionMode::InSelectInTable => {
                self.process_characters_in_select(data, span);
            }
            InsertionMode::InTemplate => {
                self.process_characters_in_template(data, span);
            }
        }
    }

    fn process_characters_by_mode(&mut self, data: &str, span: Range<usize>) {
        // Whitespace is ignored in most of these modes; anything else reprocesses in the body. The AfterBody family is
        // NOT in that set: its whitespace is inserted in the body like any other character.
        let whitespace = data.chars().all(|ch| matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' '));
        let after_body = matches!(
            self.insertion_mode,
            InsertionMode::AfterBody | InsertionMode::AfterAfterBody
        );
        // AfterHead whitespace INSERTS into the current node (the base-phase law) — the space between `</head>` and
        // `<body>` lands as the head's tail. Whitespace is ignored ONLY in the pre-head modes (Initial, BeforeHtml,
        // BeforeHead); every other mode has its own whitespace law — the head-ish modes insert it into the current node
        // (the base-phase law: the space between `</head>` and `<body>` lands as the head's tail), and the table modes
        // route it through the pending-table-text machinery (the corpus pins the space in `<table><tr> x` joining the
        // fostered "x").
        if whitespace
            && !after_body
            && matches!(
                self.insertion_mode,
                InsertionMode::Initial | InsertionMode::BeforeHtml | InsertionMode::BeforeHead
            )
        {
            return;
        }
        match self.insertion_mode {
            InsertionMode::Initial => {
                self.insertion_mode = InsertionMode::BeforeHtml;
                self.process_characters(data, span);
            }
            InsertionMode::BeforeHtml => {
                // Act as if an html start tag was seen, then reprocess.
                let html = self.new_element("html".to_string(), Namespace::Html, Vec::new());
                self.append_child(self.document, html);
                self.push_open(html);
                self.insertion_mode = InsertionMode::BeforeHead;
                self.process_characters(data, span);
            }
            InsertionMode::BeforeHead => {
                let head = self.new_element("head".to_string(), Namespace::Html, Vec::new());
                self.append_child(self.current_node().unwrap_or(self.document), head);
                self.push_open(head);
                self.head_pointer = Some(head);
                self.insertion_mode = InsertionMode::InHead;
                self.process_characters(data, span);
            }
            InsertionMode::InHead => {
                if whitespace {
                    // InHead whitespace INSERTS into the head (the base-phase law).
                    self.insert_text(data, span);
                    return;
                }
                // the law: a character TOKEN is one unit. A run that is not all-whitespace is "anything else" — the
                // WHOLE run (leading whitespace included) pops the head and reprocesses in the body. A split would put
                // the leading space in the head and the rest in the body, which is not what the reference does.
                self.pop_until("head");
                self.insertion_mode = InsertionMode::AfterHead;
                self.process_characters(data, span);
            }
            InsertionMode::AfterHead => {
                if whitespace {
                    self.insert_text(data, span);
                } else {
                    self.insert_body(data, span);
                }
            }
            InsertionMode::InColumnGroup => {
                if whitespace {
                    // Column-group whitespace inserts into the colgroup.
                    self.insert_text(data, span);
                    return;
                }
                // the law: anything else is ignored unless the current node is a COLGROUP — the corpus pins it:
                // `<template><col>Hello` drops the text (the current node is the col, never a colgroup).
                if !self
                    .current_node()
                    .is_some_and(|node| self.nodes[node.0].name == "colgroup")
                {
                    return;
                }
                self.pop_open_element();
                self.insertion_mode = InsertionMode::InTable;
                self.process_characters(data, span);
            }
            InsertionMode::InCaption | InsertionMode::InTableBody | InsertionMode::InRow | InsertionMode::InCell => {
                self.process_characters_in_table(data, span);
            }
            InsertionMode::InSelect => {
                self.process_characters_in_select(data, span);
            }
            InsertionMode::AfterBody | InsertionMode::AfterAfterBody => {
                self.insertion_mode = InsertionMode::InBody;
                self.process_characters(data, span);
            }
            InsertionMode::InFrameset => {
                // the tokenizer emits per-character tokens, so the frameset law applies PER CHARACTER: whitespace
                // inserts into the frameset, anything else is a parse error and ignored. A merged run splits on
                // whitespace.
                let mut run = String::new();
                let mut is_space = true;
                for ch in data.chars() {
                    let space = matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' ');
                    if space != is_space && !run.is_empty() {
                        if is_space {
                            self.insert_text(&run, span.clone());
                        }
                        run.clear();
                    }
                    is_space = space;
                    run.push(ch);
                }
                if !run.is_empty() && is_space {
                    self.insert_text(&run, span);
                }
            }
            InsertionMode::AfterFrameset | InsertionMode::AfterAfterFrameset => {
                // The after-frameset law is PER-CHARACTER (the tokenizer splits the runs): whitespace inserts into the
                // current node; anything else is a parse error and is DROPPED (the 2010-era law the corpus pins).
                let mut run = String::new();
                let mut is_space = true;
                for ch in data.chars() {
                    let space = matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' ');
                    if space != is_space && !run.is_empty() {
                        if is_space {
                            self.insert_text(&run, span.clone());
                        }
                        run.clear();
                    }
                    is_space = space;
                    run.push(ch);
                }
                if !run.is_empty() && is_space {
                    self.insert_text(&run, span);
                }
            }
            _ => unreachable!("by-mode character dispatch"),
        }
    }

    /// The AfterHead character "anything else": acts as if a body start tag was seen (creating the body element), then
    /// reprocesses.
    fn insert_body(&mut self, data: &str, span: Range<usize>) {
        let body = self.new_element("body".to_string(), Namespace::Html, Vec::new());
        self.append_child(self.current_node().unwrap_or(self.document), body);
        self.push_open(body);
        // the AfterHead anythingElse: the implied body keeps the frameset-ok flag TRUE (a following frameset replaces
        // it).
        self.frameset_ok = true;
        self.insertion_mode = InsertionMode::InBody;
        self.process_characters(data, span);
    }

    /// The foreign-content character law (the InForeignContentPhase): the null becomes U+FFFD and does NOT clear the
    /// frameset-ok flag; under an integration point the HTML law runs instead (null dropped).
    fn process_characters_in_foreign(&mut self, data: &str, span: Range<usize>) {
        // The corpus's two-way NUL law: under an HTML/MathML integration point the HTML law runs on the RAW data
        // (foreignObject's text loses its nulls — the in-body path drops the exact null token), while in RAW foreign
        // content a null becomes U+FFFD (the plain-text-unsafe suite pins the replacement character). The tokenizer
        // emits nulls as their own token, so the split is exact.
        let is_exact_null = data == "\0";
        let replaced = data.replace('\0', "\u{FFFD}");
        let integration = self.adjusted_current_node().is_some_and(|node| {
            let element = &self.nodes[node.0];
            is_html_integration_point(element) || is_mathml_text_integration_point(element)
        });
        if integration {
            self.process_characters_in_body(data, span);
        } else if is_exact_null {
            // The corpus's law: a null's replacement insertion does NOT clear the frameset-ok flag
            // (`<svg>\0</svg><frameset>` keeps the frameset — the body is replaced).
            self.insert_text(&replaced, span);
        } else if data.chars().all(|ch| matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' ')) {
            // Whitespace in raw foreign content inserts WITHOUT clearing the frameset-ok flag (the corpus pins `<svg>
            // </svg> <frameset>` replacing the body).
            self.insert_text(data, span);
        } else {
            // the foreign character law: the text clears the frameset-ok flag (a frameset after svg text must be
            // ignored — the corpus pins it) and inserts raw.
            self.frameset_ok = false;
            self.insert_text(data, span);
        }
    }

    fn process_characters_in_body(&mut self, data: &str, span: Range<usize>) {
        #[cfg(jqf_trace)]
        std::eprintln!(
            "BODY_CHARS {:?} afe={:?}",
            data,
            self.active_formatting
                .iter()
                .map(|e| match e {
                    FormattingEntry::Element(n) => self.nodes[n.0].name.clone(),
                    FormattingEntry::Marker => "Marker".to_string(),
                })
                .collect::<Vec<_>>()
        );
        // the own law: a token that is EXACTLY the null character is dropped (the tokenizer emits it on its own; the
        // tree builder never lets it reach the tree).
        if data == "\0" {
            return;
        }
        // The pre/listing/textarea drop-newline law: the first whitespace token after such a start tag drops one
        // leading line feed.
        if self.drop_next_newline {
            self.drop_next_newline = false;
            if let Some(stripped) = data.strip_prefix('\n') {
                if !stripped.is_empty() {
                    self.insert_text(stripped, span);
                }
                return;
            }
        }
        // Non-whitespace clears the frameset-ok flag (the flag is what lets a frameset REPLACE an empty body and what
        // stops it when the body has real content).
        if self.frameset_ok && data.chars().any(|ch| !matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' ')) {
            self.frameset_ok = false;
        }
        // The "reconstruct the active formatting elements" step.
        self.reconstruct_formatting();
        self.insert_text(data, span);
    }

    fn insert_text(&mut self, data: &str, _span: Range<usize>) {
        let (parent, index) = self.appropriate_insertion_place();
        // Merge with an adjacent text node: the LAST child for an append, the PRECEDING sibling for an indexed (foster)
        // insert — the spec's "append the character to the last text node" law, which merges fostered runs ("AC", never
        // "A" + "C").
        let adjacent = match index {
            Some(index) => {
                if index > 0 {
                    self.nodes[parent.0].children.get(index - 1).copied()
                } else {
                    None
                }
            }
            None => self.nodes[parent.0].children.last().copied(),
        };
        if let Some(last) = adjacent {
            if self.nodes[last.0].kind == NodeKind::Text {
                self.nodes[last.0].data.push_str(data);
                return;
            }
        }
        let text = self.new_text(data.to_string());
        match index {
            Some(index) => self.insert_at(parent, index, text),
            None => self.append_child(parent, text),
        }
    }

    fn process_characters_in_text(&mut self, data: &str, span: Range<usize>) {
        // The pre/listing/textarea drop-newline law also applies to the text-content modes: the first whitespace token
        // after the start tag drops one leading line feed (the corpus pins it: `<textarea>\n</textarea>` is empty).
        if self.drop_next_newline {
            self.drop_next_newline = false;
            if let Some(stripped) = data.strip_prefix('\n') {
                if !stripped.is_empty() {
                    self.insert_text(&stripped, span);
                }
                return;
            }
        }
        self.insert_text(data, span);
    }

    fn process_characters_in_table(&mut self, data: &str, span: Range<usize>) {
        #[cfg(jqf_trace)]
        std::eprintln!(
            "TABLE_CHARS {:?} afe={:?} mode={:?}",
            data,
            self.active_formatting
                .iter()
                .map(|e| match e {
                    FormattingEntry::Element(n) => self.nodes[n.0].name.clone(),
                    FormattingEntry::Marker => "Marker".to_string(),
                })
                .collect::<Vec<_>>(),
            self.insertion_mode
        );
        // The null drop law: the spec's in-table mode ignores an exact null BEFORE the whitespace split, and the
        // in-table-text phase drops it too (a null must never reach the pending table tokens).
        if data == "\0" {
            return;
        }
        let whitespace = data.chars().all(|ch| matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' '));
        if self.insertion_mode == InsertionMode::InTableText {
            // Already in the pending mode: every character joins the pending run (the own accumulation — a "B" after a
            // pending " " stays with it, and the flush decides).
            self.pending_table_tokens.push((data.to_string(), span));
            return;
        }
        if whitespace {
            // the law: WHITESPACE goes pending (the in-table-text mode); the flush inserts it at the current node of
            // FLUSH time — the corpus pins it: the space after `</tr>` lands in the tbody, never in the tr.
            self.pending_table_tokens.clear();
            self.original_insertion_mode = self.insertion_mode;
            self.insertion_mode = InsertionMode::InTableText;
            self.pending_table_tokens.push((data.to_string(), span));
            return;
        }
        // Non-whitespace while NOT pending: the table magic — foster the run into the body immediately (the
        // insertFromTable law).
        let previous = self.foster_parenting;
        self.foster_parenting = true;
        self.process_characters_in_body(data, span);
        self.foster_parenting = previous;
    }

    /// The "in table text" mode: any character flushes the pending tokens and fosters them.
    fn flush_pending_table_text(&mut self) {
        let pending = core::mem::take(&mut self.pending_table_tokens);
        // The "in table text" flush: non-whitespace characters foster (they land BEFORE the table, the own law); pure
        // whitespace inserts into the current node (a space inside a table stays inside it).
        let any_non_space = pending
            .iter()
            .any(|(data, _)| data.chars().any(|ch| !matches!(ch, '\t' | '\n' | '\x0C' | '\r' | ' ')));
        let previous = self.foster_parenting;
        self.foster_parenting = any_non_space;
        if any_non_space {
            // The non-whitespace flush runs the in-body character law, which RECONSTRUCTS the active formatting
            // elements first (the "3" after `</td>` lands inside a reconstructed `<a>`).
            self.reconstruct_formatting();
        }
        for (data, _span) in pending {
            let (parent, index) = self.appropriate_insertion_place();
            // The merge law: the PRECEDING sibling for an indexed (foster) insert — "x" after `</tr>` merges into the
            // "aba" text ("abax", never "aba" + "x").
            let adjacent = match index {
                Some(index) if index > 0 => self.nodes[parent.0].children.get(index - 1).copied(),
                _ => self.nodes[parent.0].children.last().copied(),
            };
            if let Some(last) = adjacent {
                if self.nodes[last.0].kind == NodeKind::Text {
                    self.nodes[last.0].data.push_str(&data);
                    continue;
                }
            }
            let text = self.new_text(data);
            match index {
                Some(index) => self.insert_at(parent, index, text),
                None => self.append_child(parent, text),
            }
        }
        self.foster_parenting = previous;
        self.insertion_mode = self.original_insertion_mode;
    }

    fn process_characters_in_select(&mut self, data: &str, span: Range<usize>) {
        // the law: the exact-null token is dropped in select.
        if data == "\0" {
            return;
        }
        self.insert_text(data, span);
    }

    fn process_characters_in_template(&mut self, data: &str, span: Range<usize>) {
        self.process_characters_in_body(data, span);
    }

    // ------------------------------------------------------------------ Start tags
    // ------------------------------------------------------------------

    fn process_start_tag(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        // The mainLoop phase selection: while the adjusted current node is a NON-integration foreign element, the token
        // runs the foreign rules even when the insertion mode is an HTML mode (the mode was left behind by a breakout;
        // the pops restored the foreign element to the top).
        if self.foreign_current() {
            self.start_in_foreign_content(name, attributes, self_closing, span);
            return;
        }
        let mode = self.insertion_mode;
        match mode {
            InsertionMode::Initial => self.start_initial(name, attributes, self_closing, span),
            InsertionMode::BeforeHtml => self.start_before_html(name, attributes, self_closing, span),
            InsertionMode::BeforeHead => self.start_before_head(name, attributes, self_closing, span),
            InsertionMode::InHead => self.start_in_head(name, attributes, self_closing, span),
            InsertionMode::AfterHead => self.start_after_head(name, attributes, self_closing, span),
            InsertionMode::InBody => self.start_in_body(name, attributes, self_closing, span),
            InsertionMode::Text => {
                // A start tag in the text mode is a parse error; ignore.
            }
            InsertionMode::InTable => self.start_in_table(name, attributes, self_closing, span),
            InsertionMode::InTableText => {
                // The flush restores the ORIGINAL mode; the token then re-dispatches through it (the phase restoration
                // — a `</tr>` pending after the flush must close the tr, not fall into the in-table handler).
                self.flush_pending_table_text();
                self.process_start_tag(name, attributes, self_closing, span);
            }
            InsertionMode::InCaption => self.start_in_caption(name, attributes, self_closing, span),
            InsertionMode::InColumnGroup => self.start_in_column_group(name, attributes, self_closing, span),
            InsertionMode::InTableBody => self.start_in_table_body(name, attributes, self_closing, span),
            InsertionMode::InRow => self.start_in_row(name, attributes, self_closing, span),
            InsertionMode::InCell => self.start_in_cell(name, attributes, self_closing, span),
            InsertionMode::InSelect => self.start_in_select(name, attributes, self_closing, span),
            InsertionMode::InSelectInTable => self.start_in_select_in_table(name, attributes, self_closing, span),
            InsertionMode::InTemplate => self.start_in_template(name, attributes, self_closing, span),
            InsertionMode::AfterBody => self.start_after_body(name, attributes, self_closing, span),
            InsertionMode::InFrameset => self.start_in_frameset(name, attributes, self_closing, span),
            InsertionMode::AfterFrameset => {
                if name == "noframes" {
                    // The after-frameset noframes: process in head (the element lands in the current node — the html).
                    self.start_in_head(name, attributes, self_closing, span);
                    return;
                }
                if name == "html" {
                    self.start_in_body(name, attributes, self_closing, span);
                    return;
                }
                // Anything else: parse error, IGNORE (the corpus law — the modern "process in body" puts a `<div>` into
                // the tree that the .dat drops).
            }
            InsertionMode::AfterAfterBody => self.start_after_after_body(name, attributes, self_closing, span),
            InsertionMode::AfterAfterFrameset => {
                if name == "html" {
                    self.start_in_body(name, attributes, self_closing, span);
                    return;
                }
                if name == "noframes" {
                    self.start_in_head(name, attributes, self_closing, span);
                    return;
                }
                // the law: anything else is a parse error and IGNORED — the corpus pins it: `<p>` after the frameset
                // closes must not appear in the tree.
            }
            InsertionMode::InForeignContent => self.start_in_foreign_content(name, attributes, self_closing, span),
        }
    }

    /// Inserts an element at the appropriate place and pushes it.
    fn insert_element(&mut self, name: String, ns: Namespace, attributes: &[Attribute], _span: Range<usize>) -> NodeId {
        let (parent, index) = self.appropriate_insertion_place();
        let element = self.new_element(name, ns, attributes.to_vec());
        match index {
            Some(index) => self.insert_at(parent, index, element),
            None => self.append_child(parent, element),
        }
        self.push_open(element);
        element
    }

    /// The "appropriate place for inserting a node" (with foster parenting).
    fn appropriate_insertion_place(&self) -> (NodeId, Option<usize>) {
        // the own law: the foster-parenting flag applies ONLY when the current node is a table-mode element (table,
        // tbody, tfoot, thead, tr). A cell or caption context disables it, so a pending table text flush lands INSIDE
        // the cell, and a `<plaintext>` fostered into a table receives its own text. This is the `openElements[-1].name
        // not in tableInsertModeElements` check of the `insertText`/`insertElementTable`.
        if self.foster_parenting {
            let name = self.current_node().map(|node| self.nodes[node.0].name.as_str());
            if !matches!(name, Some("table" | "tbody" | "tfoot" | "thead" | "tr")) {
                let target = self.current_node().unwrap_or(self.document);
                return (target, None);
            }
        }
        if !self.foster_parenting {
            let target = self.current_node().unwrap_or(self.document);
            return (target, None);
        }
        // Foster parenting: the LAST TABLE wins, then the LAST TEMPLATE. The corpus pins the order: `Foo` after
        // `<template><table>` lands BEFORE the table (inside the template), while a select with no table on the stack
        // lands inside the template.
        let mut last_template = None;
        let mut last_table = None;
        for node in &self.open_elements {
            let name = self.nodes[node.0].name.as_str();
            if name == "template" {
                last_template = Some(*node);
            } else if name == "table" {
                last_table = Some(*node);
            }
        }
        if let Some(table) = last_table {
            let parent = self.nodes[table.0].parent;
            if let Some(parent) = parent {
                let index = self.nodes[parent.0].children.iter().position(|child| *child == table);
                return (parent, index);
            }
            let previous = self.nodes[table.0]
                .children
                .last()
                .copied()
                .filter(|child| self.nodes[child.0].kind == NodeKind::Element);
            if let Some(previous) = previous {
                return (previous, Some(self.nodes[previous.0].children.len()));
            }
            return (self.document, None);
        }
        if let Some(template) = last_template {
            return (template, None);
        }
        let target = self.current_node().unwrap_or(self.document);
        (target, None)
    }

    /// The start-tag handlers.
    fn start_initial(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        match name {
            "html" => {
                // Append the html element; process in before-html.
                self.insertion_mode = InsertionMode::BeforeHtml;
                self.start_before_html(name, attributes, self_closing, span);
            }
            _ => {
                self.insertion_mode = InsertionMode::BeforeHtml;
                self.process_start_tag(name, attributes, self_closing, span);
            }
        }
    }

    fn start_before_html(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if name == "html" {
            let html = self.new_element("html".to_string(), Namespace::Html, attributes.to_vec());
            self.append_child(self.document, html);
            self.push_open(html);
            self.insertion_mode = InsertionMode::BeforeHead;
            return;
        }
        // Anything else: create the html element, process again.
        let html = self.new_element("html".to_string(), Namespace::Html, Vec::new());
        self.append_child(self.document, html);
        self.push_open(html);
        self.insertion_mode = InsertionMode::BeforeHead;
        self.process_start_tag(name, attributes, self_closing, span);
    }

    fn start_before_head(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if name == "html" {
            self.start_in_body(name, attributes, self_closing, span);
            return;
        }
        let head = self.new_element("head".to_string(), Namespace::Html, Vec::new());
        self.append_child(self.current_node().unwrap_or(self.document), head);
        self.push_open(head);
        self.head_pointer = Some(head);
        self.insertion_mode = InsertionMode::InHead;
        self.process_start_tag(name, attributes, self_closing, span);
    }

    fn start_in_head(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        match name {
            "html" => self.start_in_body(name, attributes, self_closing, span),
            "base" | "basefont" | "bgsound" | "link" | "meta" => {
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
                self.pop_open();
            }
            "title" => self.start_raw_text(name, attributes, span),
            "noscript" => {
                // Scripting is disabled: noscript content is elements, not RAWTEXT. In-head children that InHead would
                // reject still follow InHead's anything-else (implied body); in-body children stay inside the noscript
                // element.
                self.insert_element("noscript".to_string(), Namespace::Html, attributes, span);
            }
            "noframes" | "style" => self.start_raw_text(name, attributes, span),
            "script" => {
                self.insert_element("script".to_string(), Namespace::Html, attributes, span);
                self.original_insertion_mode = self.insertion_mode;
                self.insertion_mode = InsertionMode::Text;
            }
            "template" => self.start_template(name, attributes, span),
            "head" => {
                // Parse error; ignore.
            }
            _ => {
                self.pop_until("head");
                self.insertion_mode = InsertionMode::AfterHead;
                self.process_start_tag(name, attributes, self_closing, span);
            }
        }
        // The meta pragma-set default language tracking (in-head only).
        if name == "meta" {
            self.track_pragma_language(attributes);
        }
    }

    fn start_raw_text(&mut self, name: &str, attributes: &[Attribute], span: Range<usize>) {
        self.insert_element(name.to_string(), Namespace::Html, attributes, span);
        // The Text mode's end tag returns to the mode that was current when the raw-text element started (the
        // originalPhase law).
        self.original_insertion_mode = self.insertion_mode;
        self.insertion_mode = InsertionMode::Text;
    }

    /// The template start tag: pushes a template insertion mode.
    fn start_template(&mut self, _name: &str, attributes: &[Attribute], span: Range<usize>) {
        self.insert_element("template".to_string(), Namespace::Html, attributes, span);
        self.active_formatting.push(FormattingEntry::Marker);
        self.frameset_ok = false;
        self.insertion_mode = InsertionMode::InTemplate;
        // The pushed mode is the mode current BEFORE the template (the delegation origin when the template arrived
        // through an anything-else chain — the phase is never changed by one).
        self.template_modes
            .push(self.delegation_origin.unwrap_or(self.insertion_mode));
    }

    /// The pragma-set default language: the first in-head `<meta http-equiv="Content-Language" content="lang,...">`
    /// wins.
    fn track_pragma_language(&mut self, attributes: &[Attribute]) {
        if self.pragma_language.is_some() {
            return;
        }
        let http_equiv = attributes
            .iter()
            .find(|attr| attr.name == "http-equiv")
            .map(|attr| attr.value.to_ascii_lowercase());
        if http_equiv.as_deref() != Some("content-language") {
            return;
        }
        if let Some(content) = attributes.iter().find(|attr| attr.name == "content") {
            let language = content.value.split(',').next().map(str::trim).unwrap_or("");
            if !language.is_empty() {
                self.pragma_language = Some(language.to_string());
            }
        }
    }

    fn start_after_head(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        match name {
            "html" => self.start_in_body(name, attributes, self_closing, span),
            "body" => {
                let body = self.new_element("body".to_string(), Namespace::Html, attributes.to_vec());
                self.append_child(self.current_node().unwrap_or(self.document), body);
                self.push_open(body);
                self.frameset_ok = false;
                self.insertion_mode = InsertionMode::InBody;
            }
            "frameset" => {
                self.start_frameset(attributes, span);
            }
            "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style" | "template"
            | "title" => {
                // Parse errors; process in head (with the head pushed).
                let head = self.head_pointer.expect("head pointer");
                self.push_open(head);
                self.start_in_head(name, attributes, self_closing, span);
                self.retain_open(|node| node != head);
            }
            "head" => {
                // Parse error; ignore.
            }
            _ => {
                self.insert_body_start(name, attributes, self_closing, span);
            }
        }
    }

    /// The implied-body insertion (the AfterHead anythingElse): the body is created with the frameset-ok flag kept TRUE
    /// — an implied body never sets it false, which is what lets a following frameset replace the body.
    fn insert_body_only(&mut self) {
        let body = self.new_element("body".to_string(), Namespace::Html, Vec::new());
        self.append_child(self.current_node().unwrap_or(self.document), body);
        self.push_open(body);
        self.frameset_ok = true;
        self.insertion_mode = InsertionMode::InBody;
    }

    /// The AfterHead character/start-tag "anything else": insert the implied body, then reprocess the token in the
    /// body.
    fn insert_body_start(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        self.insert_body_only();
        self.process_start_tag(name, attributes, self_closing, span);
    }

    fn start_frameset(&mut self, attributes: &[Attribute], _span: Range<usize>) {
        let frameset = self.new_element("frameset".to_string(), Namespace::Html, attributes.to_vec());
        self.append_child(self.current_node().unwrap_or(self.document), frameset);
        self.push_open(frameset);
        self.insertion_mode = InsertionMode::InFrameset;
    }

    // ------------------------------------------------------------------ The "in body" start tags
    // ------------------------------------------------------------------

    fn start_in_body(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        match name {
            "html" => {
                // Merge attributes into the existing html element — unless a template is on the stack, which makes the
                // token ignored (the spec's own condition: `<html b=c>` inside a template content must not merge).
                if self
                    .open_elements
                    .iter()
                    .any(|node| self.nodes[node.0].name == "template")
                {
                    return;
                }
                if let Some(html) = self.open_elements.first().copied() {
                    if self.nodes[html.0].ns == Namespace::Html {
                        for attribute in attributes {
                            if !self.nodes[html.0]
                                .attrs
                                .iter()
                                .any(|existing| existing.name == attribute.name)
                            {
                                self.nodes[html.0].attrs.push(attribute.clone());
                            }
                        }
                    }
                }
            }
            "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style" | "template"
            | "title" => {
                self.start_in_head(name, attributes, self_closing, span);
            }
            "body" => {
                if self.open_elements.len() < 2
                    || self.nodes[self.open_elements[1].0].name != "body"
                    || self.nodes[self.open_elements[1].0].ns != Namespace::Html
                    // A template on the stack ignores the token too: the spec's fragment-case condition, pinned by
                    // `<body c=d>` inside template content — the attribute must NOT merge into the real body.
                    || self
                        .open_elements
                        .iter()
                        .any(|node| self.nodes[node.0].name == "template")
                {
                    // Parse error; ignore.
                    return;
                }
                self.frameset_ok = false;
                let body = self.open_elements[1];
                for attribute in attributes {
                    if !self.nodes[body.0]
                        .attrs
                        .iter()
                        .any(|existing| existing.name == attribute.name)
                    {
                        self.nodes[body.0].attrs.push(attribute.clone());
                    }
                }
            }
            "frameset" => {
                // The in-body frameset (the law): parse error unless the second element is a body and the frameset-ok
                // flag still holds; then the body is REMOVED from the tree and the frameset takes its place.
                let second_is_body = self
                    .open_elements
                    .get(1)
                    .is_some_and(|node| self.nodes[node.0].name == "body" && self.nodes[node.0].ns == Namespace::Html);
                if !second_is_body || !self.frameset_ok {
                    return;
                }
                self.remove_from_parent(self.open_elements[1]);
                while let Some(node) = self.current_node() {
                    if self.nodes[node.0].name == "html" {
                        break;
                    }
                    self.pop_open_element();
                }
                let frameset = self.insert_element("frameset".to_string(), Namespace::Html, attributes, span);
                let _ = frameset;
                self.insertion_mode = InsertionMode::InFrameset;
            }
            "address" | "article" | "aside" | "blockquote" | "center" | "details" | "dialog" | "dir" | "div" | "dl"
            | "fieldset" | "figcaption" | "figure" | "footer" | "header" | "hgroup" | "main" | "menu" | "nav"
            | "ol" | "p" | "search" | "section" | "summary" | "ul" => {
                self.close_p_if_in_button_scope();
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.close_p_if_in_button_scope();
                if self
                    .current_node()
                    .is_some_and(|node| is_heading(&self.nodes[node.0].name))
                {
                    self.pop_open_element();
                }
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            }
            "pre" | "listing" => {
                self.close_p_if_in_button_scope();
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
                self.frameset_ok = false;
                self.drop_next_newline = true;
            }
            "form" => {
                if self.form_pointer.is_some()
                    && !self
                        .open_elements
                        .iter()
                        .any(|node| self.nodes[node.0].name == "template")
                {
                    // Parse error; ignore.
                    return;
                }
                self.close_p_if_in_button_scope();
                let form = self.insert_element("form".to_string(), Namespace::Html, attributes, span);
                if !self
                    .open_elements
                    .iter()
                    .any(|node| self.nodes[node.0].name == "template")
                {
                    self.form_pointer = Some(form);
                }
            }
            "isindex" => {
                // The legacy isindex: parse error; a form + hr + label + prompt text + input (name=isindex, the other
                // attrs) + hr, all closed. The corpus's own law (the handler).
                if self.form_pointer.is_some() {
                    return;
                }
                let mut form_attrs = Vec::new();
                for attribute in attributes {
                    if attribute.name == "action" {
                        form_attrs.push(attribute.clone());
                    }
                }
                self.start_in_body("form", &form_attrs, false, span.clone());
                self.start_in_body("hr", &[], false, span.clone());
                self.start_in_body("label", &[], false, span.clone());
                let prompt = attributes
                    .iter()
                    .find(|attribute| attribute.name == "prompt")
                    .map(|attribute| attribute.value.clone())
                    .unwrap_or_else(|| "This is a searchable index. Enter search keywords: ".to_string());
                self.insert_text(&prompt, span.clone());
                let mut input_attrs = Vec::new();
                for attribute in attributes {
                    if !matches!(attribute.name.as_str(), "action" | "prompt" | "name") {
                        input_attrs.push(attribute.clone());
                    }
                }
                input_attrs.push(Attribute {
                    name: "name".to_string(),
                    value: "isindex".to_string(),
                });
                self.start_in_body("input", &input_attrs, self_closing, span.clone());
                self.end_in_body("label", span.clone());
                self.start_in_body("hr", &[], false, span.clone());
                self.end_in_body("form", span);
            }
            "li" => {
                self.frameset_ok = false;
                for node in self.open_elements.iter().rev() {
                    let element = &self.nodes[node.0];
                    if element.name == "li" {
                        self.generate_implied_end_tags(Some("li"));
                        self.pop_until("li");
                        break;
                    }
                    // the law: the walk breaks at a special/scoping element that is NOT one of the address/div/p
                    // exceptions — the p and div pass through so an li below them is closed (the corpus pins
                    // `<li><div><p> <li>` closing the first li).
                    if element.ns == Namespace::Html
                        && (is_special(&element.name) || is_scoping(&element.name))
                        && !matches!(element.name.as_str(), "address" | "div" | "p")
                    {
                        break;
                    }
                }
                self.close_p_if_in_button_scope();
                self.insert_element("li".to_string(), Namespace::Html, attributes, span);
            }
            "dd" | "dt" => {
                self.frameset_ok = false;
                for node in self.open_elements.iter().rev() {
                    let name = self.nodes[node.0].name.clone();
                    if name == "dd" || name == "dt" {
                        self.generate_implied_end_tags(Some(&name));
                        self.pop_until(&name);
                        break;
                    }
                    // The spec's dd/dt rule: the walk CONTINUES past address/div/p (they are special but not barriers),
                    // and stops at any OTHER special/scoping element.
                    if self.nodes[node.0].ns == Namespace::Html
                        && (is_special(&name) || is_scoping(&name))
                        && !matches!(name.as_str(), "address" | "div" | "p")
                    {
                        break;
                    }
                }
                self.close_p_if_in_button_scope();
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            }
            "plaintext" => {
                self.close_p_if_in_button_scope();
                self.insert_element("plaintext".to_string(), Namespace::Html, attributes, span);
                self.frameset_ok = false;
            }
            "button" => {
                if self.has_in_scope("button") {
                    self.generate_implied_end_tags(None);
                    self.pop_until("button");
                }
                self.reconstruct_formatting();
                self.insert_element("button".to_string(), Namespace::Html, attributes, span);
                self.frameset_ok = false;
            }
            "a" => {
                // the law: the search stops at the LAST MARKER — an `a` BELOW a marker (inside a marquee/object/cell)
                // is protected. The corpus pins it: `<a>aa<marquee>aa<a href=b>bb</marquee>aa` keeps the outer a open
                // (the marquee's marker shields it) and the final "aa" lands inside it.
                let mut found_a = None;
                for entry in self.active_formatting.iter().rev() {
                    match entry {
                        FormattingEntry::Marker => break,
                        FormattingEntry::Element(node) if self.nodes[node.0].name == "a" => {
                            found_a = Some(*node);
                            break;
                        }
                        FormattingEntry::Element(_) => {}
                    }
                }
                if let Some(a) = found_a {
                    self.adoption_agency("a");
                    self.remove_from_formatting(&a);
                    self.retain_open(|open| open != a);
                }
                self.reconstruct_formatting();
                let element = self.insert_element("a".to_string(), Namespace::Html, attributes, span);
                self.active_formatting.push(FormattingEntry::Element(element));
            }
            "b" | "big" | "code" | "em" | "font" | "i" | "s" | "small" | "strike" | "strong" | "tt" | "u" => {
                self.reconstruct_formatting();
                let element = self.insert_element(name.to_string(), Namespace::Html, attributes, span);
                self.push_formatting_entry(element);
            }
            "nobr" => {
                self.reconstruct_formatting();
                if self.has_in_scope("nobr") {
                    self.adoption_agency("nobr");
                    self.reconstruct_formatting();
                }
                let element = self.insert_element("nobr".to_string(), Namespace::Html, attributes, span);
                self.push_formatting_entry(element);
            }
            "applet" | "marquee" | "object" => {
                self.reconstruct_formatting();
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
                self.active_formatting.push(FormattingEntry::Marker);
                self.frameset_ok = false;
            }
            "table" => {
                if self.quirks != QuirksMode::Quirks && self.has_in_button_scope("p") {
                    self.close_p();
                }
                self.insert_element("table".to_string(), Namespace::Html, attributes, span);
                self.frameset_ok = false;
                self.insertion_mode = InsertionMode::InTable;
            }
            "area" | "br" | "embed" | "img" | "keygen" | "menuitem" | "wbr" => {
                self.reconstruct_formatting();
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
                self.pop_open();
                self.frameset_ok = false;
            }
            "input" => {
                self.reconstruct_formatting();
                self.insert_element("input".to_string(), Namespace::Html, attributes, span);
                self.pop_open();
                let type_attr = attributes
                    .iter()
                    .find(|attr| attr.name == "type")
                    .map(|attr| attr.value.to_ascii_lowercase());
                if type_attr.as_deref() != Some("hidden") {
                    self.frameset_ok = false;
                }
            }
            "param" | "source" | "track" => {
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
                self.pop_open();
            }
            "hr" => {
                self.close_p_if_in_button_scope();
                self.insert_element("hr".to_string(), Namespace::Html, attributes, span);
                self.pop_open();
                self.frameset_ok = false;
            }
            "image" => {
                // Parse error; treat as img.
                self.start_in_body("img", attributes, self_closing, span);
            }
            "textarea" => {
                self.insert_element("textarea".to_string(), Namespace::Html, attributes, span);
                self.frameset_ok = false;
                self.drop_next_newline = true;
                // The Text mode's end tag returns to the mode current when the element started (start_raw_text's law;
                // the textarea arm must set it too — a missing save restores the Initial seed and re-runs the whole
                // preamble, the corpus pins it).
                self.original_insertion_mode = self.insertion_mode;
                self.insertion_mode = InsertionMode::Text;
            }
            "xmp" => {
                self.close_p_if_in_button_scope();
                self.reconstruct_formatting();
                self.frameset_ok = false;
                self.start_raw_text("xmp", attributes, span);
            }
            "iframe" => {
                self.frameset_ok = false;
                self.start_raw_text("iframe", attributes, span);
            }
            "noembed" => {
                self.start_raw_text("noembed", attributes, span);
            }
            "noscript" => {
                self.reconstruct_formatting();
                self.insert_element("noscript".to_string(), Namespace::Html, attributes, span);
            }
            "select" => {
                self.reconstruct_formatting();
                self.insert_element("select".to_string(), Namespace::Html, attributes, span);
                self.frameset_ok = false;
                // The mode test reads the ORIGINAL mode: the table-family delegations run the body rules with the
                // insertion mode temporarily set to "in body", so the pre-delegation mode (the delegation origin) is
                // what decides between the select-in-table and plain select modes (the own phase test). The corpus pins
                // it: `<table><tbody><select> <tr>` must take "in select in table" so the tr returns to the table.
                let origin = self.delegation_origin.unwrap_or(self.insertion_mode);
                if origin == InsertionMode::InTable
                    || origin == InsertionMode::InCaption
                    || origin == InsertionMode::InTableBody
                    || origin == InsertionMode::InRow
                    || origin == InsertionMode::InCell
                {
                    self.insertion_mode = InsertionMode::InSelectInTable;
                } else {
                    self.insertion_mode = InsertionMode::InSelect;
                }
            }
            "optgroup" | "option" => {
                if self
                    .current_node()
                    .is_some_and(|node| self.nodes[node.0].name == "option")
                {
                    self.pop_open_element();
                }
                self.reconstruct_formatting();
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            }
            "rb" | "rtc" => {
                if self.has_in_scope("ruby") {
                    self.generate_implied_end_tags(None);
                }
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            }
            "rp" | "rt" => {
                if self.has_in_scope("ruby") {
                    self.generate_implied_end_tags(Some("rtc"));
                }
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            }
            "math" => {
                self.reconstruct_formatting();
                let element = self.insert_foreign_element("math", Namespace::MathMl, attributes, span);
                process_foreign_attributes(&mut self.nodes[element.0]);
                if self_closing {
                    self.pop_open();
                }
                if self.foreign_origin_mode.is_none() {
                    self.foreign_origin_mode = Some(self.delegation_origin.unwrap_or(self.insertion_mode));
                }
                self.insertion_mode = InsertionMode::InForeignContent;
            }
            "svg" => {
                self.reconstruct_formatting();
                let element = self.insert_foreign_element("svg", Namespace::Svg, attributes, span);
                process_foreign_attributes(&mut self.nodes[element.0]);
                if self_closing {
                    self.pop_open();
                }
                if self.foreign_origin_mode.is_none() {
                    self.foreign_origin_mode = Some(self.delegation_origin.unwrap_or(self.insertion_mode));
                }
                self.insertion_mode = InsertionMode::InForeignContent;
            }
            "caption" | "col" | "colgroup" | "frame" | "head" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr" => {
                // Parse error; ignore.
            }
            _ => {
                self.reconstruct_formatting();
                self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            }
        }
    }

    /// Closes a `p` element when one is in button scope.
    fn close_p_if_in_button_scope(&mut self) {
        if self.has_in_button_scope("p") {
            self.close_p();
        }
    }

    fn close_p(&mut self) {
        self.generate_implied_end_tags(Some("p"));
        self.pop_until("p");
    }

    /// The formatting-element removal (the "if the list of active formatting elements contains an element with the same
    /// tag name as the entry").
    fn remove_from_formatting(&mut self, node: &NodeId) {
        self.active_formatting.retain(|entry| match entry {
            FormattingEntry::Element(existing) => existing != node,
            FormattingEntry::Marker => true,
        });
    }

    /// The "reconstruct the active formatting elements" algorithm.
    fn reconstruct_formatting(&mut self) {
        let Some(last) = self.active_formatting.last() else {
            return;
        };
        if matches!(last, FormattingEntry::Marker) {
            return;
        }
        if let FormattingEntry::Element(node) = last {
            if self.open_elements.contains(node) {
                return;
            }
        }
        // Find the entry to start from.
        let mut index = self.active_formatting.len() - 1;
        loop {
            if index == 0 {
                break;
            }
            index -= 1;
            match &self.active_formatting[index] {
                FormattingEntry::Marker => {
                    index += 1;
                    break;
                }
                FormattingEntry::Element(node) => {
                    if self.open_elements.contains(node) {
                        index += 1;
                        break;
                    }
                }
            }
        }
        while index < self.active_formatting.len() {
            let entry = self.active_formatting[index].clone_entry();
            match entry {
                FormattingEntry::Marker => break,
                FormattingEntry::Element(node) => {
                    // Recreate the element as a clone of the formatting element's data.
                    let name = self.nodes[node.0].name.clone();
                    let attrs = self.nodes[node.0].attrs.clone();
                    let (parent, position) = self.appropriate_insertion_place();
                    let element = self.new_element(name, Namespace::Html, attrs);
                    match position {
                        Some(position) => self.insert_at(parent, position, element),
                        None => self.append_child(parent, element),
                    }
                    self.push_open(element);
                    self.active_formatting[index] = FormattingEntry::Element(element);
                }
            }
            index += 1;
        }
    }

    /// The adoption agency algorithm (the misnested formatting repair), a faithful port of the implementation.
    fn adoption_agency(&mut self, subject: &str) {
        let mut outer = 0usize;
        let subject = subject.to_string();
        loop {
            if outer >= 8 {
                return;
            }
            outer += 1;
            // Step 4: the formatting element (the last active-formatting entry with the token's name).
            let Some(formatting_index) = self.active_formatting.iter().rposition(
                |entry| matches!(entry, FormattingEntry::Element(node) if self.nodes[node.0].name == subject),
            ) else {
                // No such node: act as the "any other end tag" entry.
                self.end_any_other(&subject);
                return;
            };
            let formatting = match &self.active_formatting[formatting_index] {
                FormattingEntry::Element(node) => *node,
                FormattingEntry::Marker => return,
            };
            if !self.open_elements.contains(&formatting) {
                // Parse error; remove it from the list and abort.
                self.active_formatting.remove(formatting_index);
                return;
            }
            if !self.has_in_scope(&subject) {
                // Parse error; ignore the token.
                return;
            }
            // Step 5: the furthest block.
            let afe_index = self
                .open_elements
                .iter()
                .position(|node| *node == formatting)
                .expect("formatting element open");
            let mut furthest = None;
            for node in &self.open_elements[afe_index..] {
                if is_special_in(&self.nodes[node.0]) {
                    furthest = Some(*node);
                    break;
                }
            }
            // Step 6: no furthest block — pop to the formatting element.
            let Some(furthest) = furthest else {
                while let Some(node) = self.pop_open() {
                    if node == formatting {
                        break;
                    }
                }
                self.active_formatting.remove(formatting_index);
                return;
            };
            // Step 7: the common ancestor.
            let common_ancestor = self.open_elements[afe_index - 1];
            // Step 8: the bookmark.
            let mut bookmark = formatting_index;
            // Step 9: the inner loop.
            let mut last_node = furthest;
            let mut node = furthest;
            let mut inner = 0usize;
            let mut index = self
                .open_elements
                .iter()
                .position(|open| *open == node)
                .expect("furthest open");
            while inner < 3 {
                inner += 1;
                // Node is the element before node in the open elements.
                if index == 0 {
                    break;
                }
                index -= 1;
                node = self.open_elements[index];
                if !self
                    .active_formatting
                    .iter()
                    .any(|entry| matches!(entry, FormattingEntry::Element(n) if *n == node))
                {
                    self.remove_open(index);
                    if index == 0 {
                        break;
                    }
                    continue;
                }
                if node == formatting {
                    break;
                }
                // Step 9.7: the bookmark moves.
                if last_node == furthest {
                    bookmark = self
                        .active_formatting
                        .iter()
                        .position(|entry| matches!(entry, FormattingEntry::Element(n) if *n == node))
                        .expect("node in active formatting")
                        + 1;
                }
                // Step 9.8: clone the node and replace it in both lists.
                let name = self.nodes[node.0].name.clone();
                let attrs = self.nodes[node.0].attrs.clone();
                let clone = self.new_element(name, Namespace::Html, attrs);
                for entry in &mut self.active_formatting {
                    if let FormattingEntry::Element(n) = entry {
                        if *n == node {
                            *entry = FormattingEntry::Element(clone);
                        }
                    }
                }
                self.open_elements[index] = clone;
                node = clone;
                // Step 9.9: remove lastNode from its parent, append to node.
                self.remove_from_parent(last_node);
                self.append_child(node, last_node);
                // Step 9.10.
                last_node = node;
            }
            // Step 10: remove lastNode from its parent; append to the common ancestor (or foster when it is a table).
            // The foster is DIRECT (the getTableMisnestedNodePosition): the last table's position — never the
            // current-node-checked place, which would append the node to itself.
            self.remove_from_parent(last_node);
            if matches!(
                self.nodes[common_ancestor.0].name.as_str(),
                "table" | "tbody" | "tfoot" | "thead" | "tr"
            ) {
                let last_table = self
                    .open_elements
                    .iter()
                    .rev()
                    .find(|node| self.nodes[node.0].name == "table")
                    .copied();
                if let Some(table) = last_table {
                    if let Some(parent) = self.nodes[table.0].parent {
                        let index = self.nodes[parent.0].children.iter().position(|child| *child == table);
                        match index {
                            Some(index) => self.insert_at(parent, index, last_node),
                            None => self.append_child(parent, last_node),
                        }
                    } else {
                        let target = self
                            .open_elements
                            .iter()
                            .rev()
                            .skip_while(|node| **node != table)
                            .nth(1)
                            .copied()
                            .or_else(|| self.open_elements.first().copied())
                            .unwrap_or(self.document);
                        self.append_child(target, last_node);
                    }
                } else {
                    let target = self.open_elements.first().copied().unwrap_or(self.document);
                    self.append_child(target, last_node);
                }
            } else {
                self.append_child(common_ancestor, last_node);
            }
            // Step 11-13: clone the formatting element; move the furthest block's children into the clone; append the
            // clone to the furthest block.
            let formatting_name = self.nodes[formatting.0].name.clone();
            let formatting_attrs = self.nodes[formatting.0].attrs.clone();
            let clone = self.new_element(formatting_name, Namespace::Html, formatting_attrs);
            let children = core::mem::take(&mut self.nodes[furthest.0].children);
            for child in children {
                self.nodes[child.0].parent = Some(clone);
                self.nodes[clone.0].children.push(child);
            }
            self.append_child(furthest, clone);
            // Step 14: replace the formatting element in the active list with the clone at the bookmark.
            let entry_position = self
                .active_formatting
                .iter()
                .position(|entry| matches!(entry, FormattingEntry::Element(n) if *n == formatting))
                .expect("formatting in active list");
            self.active_formatting.remove(entry_position);
            let bookmark = bookmark.min(self.active_formatting.len());
            self.active_formatting.insert(bookmark, FormattingEntry::Element(clone));
            // Step 15: remove the formatting element from the open stack and insert the clone AFTER the furthest block
            // (the exact placement — the re-run of the outer loop then finds no furthest block below the clone and
            // terminates).
            let formatting_position = self
                .open_elements
                .iter()
                .position(|open| *open == formatting)
                .expect("formatting in open stack");
            self.remove_open(formatting_position);
            let furthest_position = self
                .open_elements
                .iter()
                .position(|open| *open == furthest)
                .expect("furthest in open stack");
            self.insert_open(furthest_position + 1, clone);
        }
    }

    fn insert_foreign_element(
        &mut self,
        name: &str,
        ns: Namespace,
        attributes: &[Attribute],
        _span: Range<usize>,
    ) -> NodeId {
        let adjusted = match ns {
            Namespace::Svg => adjust_svg_name(name),
            Namespace::MathMl => name.to_string(),
            Namespace::Html => name.to_string(),
        };
        let (parent, position) = self.appropriate_insertion_place();
        let element = self.new_element(adjusted, ns, attributes.to_vec());
        match position {
            Some(position) => self.insert_at(parent, position, element),
            None => self.append_child(parent, element),
        }
        self.push_open(element);
        // The foreign attribute adjustments apply to EVERY foreign element, not just the integration-point roots (the
        // generic foreign-content arm inserts through this function too). Idempotent, so the explicit calls after the
        // math/svg arms are harmless.
        process_foreign_attributes(&mut self.nodes[element.0]);
        element
    }

    // ------------------------------------------------------------------ End tags
    // ------------------------------------------------------------------

    fn process_end_tag(&mut self, name: &str, span: Range<usize>) {
        #[cfg(jqf_trace)]
        std::eprintln!(
            "END {name} mode={:?} afe={:?}",
            self.insertion_mode,
            self.active_formatting
                .iter()
                .map(|e| match e {
                    FormattingEntry::Element(n) => self.nodes[n.0].name.clone(),
                    FormattingEntry::Marker => "Marker".to_string(),
                })
                .collect::<Vec<_>>()
        );
        // The selection law for END tags is broader than the start tags': the integration-point exemption applies only
        // to start tags and characters (the mainLoop condition) — an end tag under ANY foreign current node runs the
        // foreign walk.
        if !self.delegating
            && self.insertion_mode != InsertionMode::InForeignContent
            && self
                .adjusted_current_node()
                .is_some_and(|node| self.nodes[node.0].ns != Namespace::Html)
        {
            self.end_in_foreign_content(name, span);
            return;
        }
        let mode = self.insertion_mode;
        match mode {
            InsertionMode::Initial
            | InsertionMode::BeforeHtml
            | InsertionMode::BeforeHead
            | InsertionMode::InHead
            | InsertionMode::AfterHead => self.end_by_mode(name, span),
            InsertionMode::InBody => self.end_in_body(name, span),
            InsertionMode::Text => {
                // Any end tag in the text mode: pop and return to the original insertion mode.
                self.pop_open_element();
                self.insertion_mode = self.original_insertion_mode;
            }
            InsertionMode::InTable => self.end_in_table(name, span),
            InsertionMode::InTableText => {
                // The flush restores the ORIGINAL mode; the token then re-dispatches through it (see the start-tag
                // arm).
                self.flush_pending_table_text();
                self.process_end_tag(name, span);
            }
            InsertionMode::InCaption => self.end_in_caption(name, span),
            InsertionMode::InColumnGroup => self.end_in_column_group(name, span),
            InsertionMode::InTableBody => self.end_in_table_body(name, span),
            InsertionMode::InRow => self.end_in_row(name, span),
            InsertionMode::InCell => self.end_in_cell(name, span),
            InsertionMode::InSelect => self.end_in_select(name, span),
            InsertionMode::InSelectInTable => self.end_in_select_in_table(name, span),
            InsertionMode::InTemplate => self.end_in_template(name, span),
            InsertionMode::AfterBody => self.end_after_body(name, span),
            InsertionMode::InFrameset => self.end_in_frameset(name, span),
            InsertionMode::AfterFrameset => {
                if name == "html" {
                    self.insertion_mode = InsertionMode::AfterAfterFrameset;
                }
            }
            InsertionMode::AfterAfterBody => {
                // the law: EVERY end tag in after-after-body moves to the in-body phase (html → in body; anything else
                // → in body and reprocessed).
                self.insertion_mode = InsertionMode::InBody;
                if name != "html" {
                    self.process_end_tag(name, span);
                }
            }
            InsertionMode::AfterAfterFrameset => {}
            InsertionMode::InForeignContent => self.end_in_foreign_content(name, span),
        }
    }

    fn end_by_mode(&mut self, name: &str, span: Range<usize>) {
        // The head-adjacent modes' end-tag "anything else" arms act as if a structural START tag was seen, then
        // reprocess the token.
        match self.insertion_mode {
            InsertionMode::InHead => {
                if name == "head" {
                    self.pop_open_element();
                    self.insertion_mode = InsertionMode::AfterHead;
                    return;
                }
                if name == "template" {
                    self.end_template(span);
                    return;
                }
                if matches!(name, "body" | "html" | "br") {
                    // Act as if a head END tag was seen (which pops the head), then as an IMPLIED body — the implied
                    // body keeps the frameset-ok flag TRUE (the anythingElse), which is what lets a frameset replace it
                    // — then reprocess.
                    self.pop_until("head");
                    self.insertion_mode = InsertionMode::AfterHead;
                    self.insert_body_only();
                    self.process_end_tag(name, span);
                    return;
                }
                // Any other end tag in head: parse error, ignore (the endTagOther law — the head stays).
            }
            InsertionMode::AfterHead => {
                if name == "template" {
                    self.end_template(span);
                    return;
                }
                if matches!(name, "body" | "html" | "br") {
                    // Act as if an IMPLIED body was seen (the frameset-ok flag stays TRUE), then reprocess.
                    self.insert_body_only();
                    self.process_end_tag(name, span);
                    return;
                }
                // the after-head law: any other end tag is a parse error and IGNORED (the corpus pins it: `</p>` after
                // `</head>` must not create a p and must leave the mode in after-head for the following comment).
            }
            InsertionMode::BeforeHead => {
                // the law: only head/body/html/br end tags imply a head and reprocess; ANY OTHER end tag is a parse
                // error and IGNORED — the corpus pins it: `</p>` before the head never creates a p, and the following
                // comment lands on the html element because the mode never advanced.
                if !matches!(name, "head" | "body" | "html" | "br") {
                    return;
                }
                let head = self.new_element("head".to_string(), Namespace::Html, Vec::new());
                self.append_child(self.current_node().unwrap_or(self.document), head);
                self.push_open(head);
                self.head_pointer = Some(head);
                self.insertion_mode = InsertionMode::InHead;
                self.process_end_tag(name, span);
                return;
            }
            InsertionMode::Initial | InsertionMode::BeforeHtml => {
                // the before-html law: only head/body/html/br end tags imply an html element and reprocess; any other
                // end tag is a parse error and ignored.
                if !matches!(name, "head" | "body" | "html" | "br") {
                    return;
                }
                let html = self.new_element("html".to_string(), Namespace::Html, Vec::new());
                self.append_child(self.document, html);
                self.push_open(html);
                self.insertion_mode = InsertionMode::BeforeHead;
                self.process_end_tag(name, span);
                return;
            }
            _ => {
                self.start_after_head("body", &[], false, span);
            }
        }
    }

    fn end_in_body(&mut self, name: &str, span: Range<usize>) {
        match name {
            "body" => {
                if !self.has_in_scope("body") {
                    // Parse error; ignore.
                    return;
                }
                self.insertion_mode = InsertionMode::AfterBody;
            }
            "html" => {
                if !self.has_in_scope("body") {
                    return;
                }
                // the law: `</html>` runs the body end tag (which switches to "after body"), then the after-body html
                // end tag — landing in AFTER AFTER BODY. The corpus pins it: the comment after `</html>` is a DOCUMENT
                // child.
                self.process_end_tag("body", span);
                self.insertion_mode = InsertionMode::AfterAfterBody;
            }
            "address" | "article" | "aside" | "blockquote" | "button" | "center" | "details" | "dialog" | "dir"
            | "div" | "dl" | "fieldset" | "figcaption" | "figure" | "footer" | "header" | "hgroup" | "listing"
            | "main" | "menu" | "nav" | "ol" | "pre" | "search" | "section" | "summary" | "ul" => {
                if !self.has_in_scope(name) {
                    // Parse error; ignore.
                    return;
                }
                self.generate_implied_end_tags(None);
                self.pop_until(name);
            }
            "form" => {
                let Some(node) = self.form_pointer else {
                    // Parse error; ignore.
                    return;
                };
                self.form_pointer = None;
                if !self.open_elements.contains(&node) {
                    // Parse error; ignore.
                    return;
                }
                // the law: REMOVE the form's node from the stack — the elements above it (a div inside the form) stay
                // open.
                self.generate_implied_end_tags(None);
                self.retain_open(|open| open != node);
            }
            "p" => {
                #[cfg(jqf_trace)]
                std::eprintln!(
                    "END P scope={} stack={:?}",
                    self.has_in_button_scope("p"),
                    self.open_elements
                        .iter()
                        .map(|n| self.nodes[n.0].name.clone())
                        .collect::<Vec<_>>()
                );
                if !self.has_in_button_scope("p") {
                    // Parse error; insert an empty p.
                    self.insert_element("p".to_string(), Namespace::Html, &[], span);
                }
                self.close_p();
            }
            "li" => {
                if !self.has_in_list_item_scope("li") {
                    // Parse error; ignore.
                    return;
                }
                self.generate_implied_end_tags(Some("li"));
                self.pop_until("li");
            }
            "dd" | "dt" => {
                if !self.has_in_scope(name) {
                    // Parse error; ignore.
                    return;
                }
                self.generate_implied_end_tags(Some(name));
                self.pop_until(name);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if !self
                    .open_elements
                    .iter()
                    .rev()
                    .any(|node| self.nodes[node.0].ns == Namespace::Html && is_heading(&self.nodes[node.0].name))
                {
                    // Parse error; ignore.
                    return;
                }
                self.generate_implied_end_tags(None);
                while let Some(node) = self.current_node() {
                    if is_heading(&self.nodes[node.0].name) {
                        break;
                    }
                    self.pop_open_element();
                }
                self.pop_open_element();
            }
            "a" | "b" | "big" | "code" | "em" | "font" | "i" | "nobr" | "s" | "small" | "strike" | "strong" | "tt"
            | "u" => {
                self.adoption_agency(name);
            }
            "applet" | "marquee" | "object" => {
                if !self.has_in_scope(name) {
                    // Parse error; ignore.
                    return;
                }
                self.generate_implied_end_tags(None);
                self.pop_until(name);
                self.clear_active_formatting_to_marker();
            }
            "br" => {
                // Parse error; act as a br start tag.
                self.start_in_body("br", &[], false, span);
            }
            "template" => {
                self.end_template(span);
            }
            _ => {
                // The "any other end tag" rules: walk the open stack.
                self.end_any_other(name);
            }
        }
    }

    fn end_any_other(&mut self, name: &str) {
        // the own law: the walk matches on the NAME alone (a foreign element with the same name is popped); only an
        // HTML-namespace special element stops the walk.
        for index in (0..self.open_elements.len()).rev() {
            let node = self.open_elements[index];
            let element = &self.nodes[node.0];
            if element.name == name {
                self.generate_implied_end_tags(Some(name));
                while let Some(current) = self.current_node() {
                    if current == node {
                        break;
                    }
                    self.pop_open_element();
                }
                self.pop_open_element();
                return;
            }
            if is_special_in(element) {
                // Parse error; ignore.
                return;
            }
        }
    }

    /// Pops the stack back to a table context (the current node becomes the table or html element — the
    /// clearStackToTableContext).
    fn clear_stack_to_table_context(&mut self) {
        while let Some(node) = self.current_node() {
            // The spec's stop set is table, template, or html — a template stays on the stack so its contents keep
            // receiving the table-context start tags (the clearStackToTableContext).
            if matches!(self.nodes[node.0].name.as_str(), "table" | "template" | "html") {
                break;
            }
            self.pop_open_element();
        }
    }

    fn end_in_table(&mut self, name: &str, span: Range<usize>) {
        if name == "table" {
            if !self.has_in_table_scope("table") {
                // Parse error; ignore.
                return;
            }
            self.pop_until("table");
            self.reset_insertion_mode();
            return;
        }
        if name == "template" {
            self.end_template(span);
            return;
        }
        if name == "body"
            || name == "caption"
            || name == "col"
            || name == "colgroup"
            || name == "html"
            || name == "tbody"
            || name == "td"
            || name == "tfoot"
            || name == "th"
            || name == "thead"
            || name == "tr"
        {
            // Parse error; ignore.
            return;
        }
        // Anything else: foster-parenting flag on, process in body.
        let previous = self.foster_parenting;
        self.foster_parenting = true;
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_end_tag(name, span);
        self.foster_parenting = previous;
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InBody {
            self.insertion_mode = InsertionMode::InTable;
        }
    }

    fn end_in_caption(&mut self, name: &str, span: Range<usize>) {
        if name == "caption" {
            if !self.has_in_table_scope("caption") {
                // Parse error; ignore.
                return;
            }
            self.generate_implied_end_tags(None);
            self.pop_until("caption");
            // the law: the caption end clears the formatting list UP TO AND INCLUDING the last marker (the entries
            // below it — fostered formatting elements — survive for reconstruction).
            self.clear_active_formatting_to_marker();
            self.insertion_mode = InsertionMode::InTable;
            return;
        }
        if name == "table" {
            if !self.has_in_table_scope("caption") {
                // Parse error; ignore.
                return;
            }
            self.generate_implied_end_tags(None);
            self.pop_until("caption");
            // the law: the caption end clears the formatting list UP TO AND INCLUDING the last marker (the entries
            // below it — fostered formatting elements — survive for reconstruction).
            self.clear_active_formatting_to_marker();
            self.insertion_mode = InsertionMode::InTable;
            self.process_end_tag(name, span);
            return;
        }
        if matches!(
            name,
            "body" | "col" | "colgroup" | "html" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
        ) {
            // Parse error; ignore.
            return;
        }
        // Anything else: process in body.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_end_tag(name, span);
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InBody {
            self.insertion_mode = InsertionMode::InCaption;
        }
    }

    fn end_in_column_group(&mut self, name: &str, span: Range<usize>) {
        if name == "colgroup" {
            if self
                .current_node()
                .is_some_and(|node| self.nodes[node.0].name == "colgroup")
            {
                self.pop_open_element();
                self.insertion_mode = InsertionMode::InTable;
            }
            return;
        }
        if name == "col" {
            // Parse error; ignore.
            return;
        }
        if name == "template" {
            self.end_template(span);
            return;
        }
        // Anything else: when the current node is not a colgroup, this is a parse error and the token is IGNORED.
        // Otherwise close the colgroup and reprocess.
        if self
            .current_node()
            .is_some_and(|node| self.nodes[node.0].name == "colgroup")
        {
            self.pop_open_element();
            self.insertion_mode = InsertionMode::InTable;
            self.process_end_tag(name, span);
        }
    }

    fn end_in_table_body(&mut self, name: &str, span: Range<usize>) {
        if name == "template" {
            // The template end is handled DIRECTLY — the anything-else delegation would restore the table-body mode
            // over the end-template's own mode law ([69] pins the restored mode being "in table").
            self.end_template(span);
            return;
        }
        if matches!(name, "tbody" | "tfoot" | "thead") {
            if !self.has_in_table_scope(name) {
                // Parse error; ignore.
                return;
            }
            self.pop_until(name);
            self.insertion_mode = InsertionMode::InTable;
            return;
        }
        if name == "table" {
            if !self.has_in_table_scope(name) {
                // Parse error; ignore.
                return;
            }
            // Pop the section element (any of the three) — never three separate pop_until calls (the later ones would
            // empty the stack once the first consumed the section).
            while let Some(node) = self.current_node() {
                if matches!(self.nodes[node.0].name.as_str(), "tbody" | "tfoot" | "thead") {
                    break;
                }
                self.pop_open_element();
            }
            self.pop_open_element();
            self.insertion_mode = InsertionMode::InTable;
            self.process_end_tag(name, span);
            return;
        }
        if matches!(
            name,
            "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th" | "tr"
        ) {
            // Parse error; ignore.
            return;
        }
        // Anything else: process in table.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InTable;
        self.process_end_tag(name, span);
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InTable {
            self.insertion_mode = InsertionMode::InTableBody;
        }
    }

    fn end_in_row(&mut self, name: &str, span: Range<usize>) {
        if name == "template" {
            self.end_template(span);
            return;
        }
        if name == "tr" {
            if !self.has_in_table_scope("tr") {
                // Parse error; ignore.
                return;
            }
            self.pop_until("tr");
            self.insertion_mode = InsertionMode::InTableBody;
            return;
        }
        if name == "table" {
            if !self.has_in_table_scope("tr") {
                // Parse error; ignore.
                return;
            }
            self.pop_until("tr");
            self.insertion_mode = InsertionMode::InTableBody;
            self.process_end_tag(name, span);
            return;
        }
        if matches!(name, "tbody" | "tfoot" | "thead") {
            if !self.has_in_table_scope(name) {
                // Parse error; ignore.
                return;
            }
            self.pop_until("tr");
            self.insertion_mode = InsertionMode::InTableBody;
            self.process_end_tag(name, span);
            return;
        }
        if matches!(name, "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th") {
            // Parse error; ignore.
            return;
        }
        // Anything else: process in table body.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InTableBody;
        self.process_end_tag(name, span);
        self.delegation_origin = saved_delegation;
        self.insertion_mode = InsertionMode::InRow;
    }

    fn end_in_cell(&mut self, name: &str, span: Range<usize>) {
        if name == "template" {
            self.end_template(span);
            return;
        }
        if matches!(name, "td" | "th") {
            if !self.has_in_table_scope(name) {
                // Parse error; ignore.
                return;
            }
            self.generate_implied_end_tags(None);
            self.pop_until(name);
            // the law: the cell end clears the formatting list UP TO AND INCLUDING the last marker (the fostered
            // entries below it survive for reconstruction).
            self.clear_active_formatting_to_marker();
            self.insertion_mode = InsertionMode::InRow;
            return;
        }
        if name == "tr" {
            // the law: `</tr>` in a cell is the in-body generic end — pop until the tr is popped, NO cell close, NO
            // formatting clear (the fostered formatting elements survive; the corpus pins the reconstruction in
            // `<table><b>...bbb`).
            if self.has_in_table_scope("tr") {
                self.pop_until("tr");
            }
            return;
        }
        if matches!(name, "body" | "caption" | "col" | "colgroup" | "html") {
            // Parse error; ignore.
            return;
        }
        if matches!(name, "table" | "tbody" | "tfoot" | "thead") {
            if !self.has_in_table_scope(name) {
                // Parse error; ignore.
                return;
            }
            self.close_cell();
            self.process_end_tag(name, span);
            return;
        }
        // Anything else: process in body.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_end_tag(name, span);
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InBody {
            self.insertion_mode = InsertionMode::InCell;
        }
    }

    fn close_cell(&mut self) {
        self.generate_implied_end_tags(None);
        // the law: pop while the current node is NOT td/th, then pop the cell itself — never two separate pop_until
        // calls (the second would empty the stack when the first already consumed the cell).
        while let Some(node) = self.current_node() {
            if matches!(self.nodes[node.0].name.as_str(), "td" | "th") {
                break;
            }
            self.pop_open_element();
        }
        self.pop_open_element();
        // the law: the cell end clears the formatting list UP TO AND INCLUDING the last marker.
        self.clear_active_formatting_to_marker();
        self.insertion_mode = InsertionMode::InRow;
    }

    fn end_in_select(&mut self, name: &str, span: Range<usize>) {
        if name == "template" {
            self.end_template(span);
            return;
        }
        if name == "optgroup" {
            // `</optgroup>` implicitly closes an option, then the optgroup (the own law); anything else is a parse
            // error.
            let top = self.open_elements.last().copied();
            let below = self.open_elements.get(self.open_elements.len() - 2).copied();
            if let (Some(top), Some(below)) = (top, below) {
                if self.nodes[top.0].name == "option" && self.nodes[below.0].name == "optgroup" {
                    self.pop_open();
                }
            }
            if let Some(top) = self.open_elements.last().copied() {
                if self.nodes[top.0].name == "optgroup" {
                    self.pop_open();
                }
            }
            return;
        }
        if name == "option" {
            // `</option>` pops an option current node (the law).
            if let Some(top) = self.open_elements.last().copied() {
                if self.nodes[top.0].name == "option" {
                    self.pop_open();
                }
            }
            return;
        }
        if matches!(
            name,
            "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th"
        ) {
            // The corpus law (the endTagTableElements): a table-mode end tag in select CLOSES the select when the named
            // element is in table scope, then reprocesses the token.
            if self.has_in_table_scope(name) {
                self.pop_until("select");
                self.reset_insertion_mode();
                self.process_end_tag(name, span);
            }
            return;
        }
        if name != "select" {
            // Parse error; ignore.
            return;
        }
        if !self.has_in_select_scope("select") {
            return;
        }
        self.pop_until("select");
        self.reset_insertion_mode();
    }

    fn end_in_select_in_table(&mut self, name: &str, span: Range<usize>) {
        if matches!(
            name,
            "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th"
        ) {
            // Parse error; process in select.
            self.pop_until("select");
            self.reset_insertion_mode();
            self.process_end_tag(name, span);
            return;
        }
        self.end_in_select(name, span);
    }

    fn end_in_template(&mut self, name: &str, span: Range<usize>) {
        if name == "template" {
            self.end_template(span);
            return;
        }
        // Any other end tag: a parse error; ignore it (the spec's rule — the template's insertion mode is not entered
        // for arbitrary end tags).
        let _ = name;
        let _ = span;
    }

    /// The template end tag: pop the template insertion mode and the template element.
    fn end_template(&mut self, span: Range<usize>) {
        let _ = span;
        if !self
            .open_elements
            .iter()
            .any(|node| self.nodes[node.0].ns == Namespace::Html && self.nodes[node.0].name == "template")
        {
            // Parse error; ignore.
            return;
        }
        self.generate_implied_end_tags(None);
        // Pop THROUGH any foreign (SVG-namespace) template to the HTML template (the corpus law: the foreign template
        // survives in the tree, its stack position is popped).
        while let Some(node) = self.current_node() {
            if self.nodes[node.0].ns == Namespace::Html && self.nodes[node.0].name == "template" {
                break;
            }
            self.pop_open_element();
        }
        self.pop_open_element();
        self.clear_active_formatting_to_marker();
        self.template_modes.pop();
        // The corpus's end-template law: the mode becomes the REMAINING template insertion mode (the outer template's
        // pushed mode — the thead's "in table" in [69]) rather than a fresh reset; only the last template falls back to
        // the reset.
        if let Some(&mode) = self.template_modes.last() {
            self.insertion_mode = mode;
        } else {
            self.reset_insertion_mode();
        }
    }

    fn end_after_body(&mut self, name: &str, span: Range<usize>) {
        if name == "html" {
            self.insertion_mode = InsertionMode::AfterAfterBody;
            return;
        }
        // Anything else: process in body.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_end_tag(name, span);
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InBody {
            self.insertion_mode = InsertionMode::AfterBody;
        }
    }

    fn end_in_frameset(&mut self, name: &str, span: Range<usize>) {
        let _ = span;
        if name == "frameset" {
            if self
                .current_node()
                .is_some_and(|node| self.nodes[node.0].name == "html")
            {
                // Parse error; ignore.
                return;
            }
            self.pop_open_element();
            if self
                .current_node()
                .is_some_and(|node| self.nodes[node.0].name != "frameset")
            {
                self.insertion_mode = InsertionMode::AfterFrameset;
            }
        }
    }

    // ------------------------------------------------------------------ The table-related start tags
    // ------------------------------------------------------------------

    fn start_in_table(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        // The table-context start tags first CLEAR THE STACK back to a table context (the clearStackToTableContext): a
        // fostered formatting element above the table is popped, so the implied tbody/caption land inside the TABLE.
        if matches!(
            name,
            "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
        ) {
            self.clear_stack_to_table_context();
        }
        if name == "caption" {
            // the law: no formatting clear — only the marker push.
            self.active_formatting.push(FormattingEntry::Marker);
            self.insert_element("caption".to_string(), Namespace::Html, attributes, span);
            self.insertion_mode = InsertionMode::InCaption;
            return;
        }
        if name == "colgroup" {
            self.insert_element("colgroup".to_string(), Namespace::Html, attributes, span);
            self.insertion_mode = InsertionMode::InColumnGroup;
            return;
        }
        if name == "col" {
            self.insert_element("colgroup".to_string(), Namespace::Html, &[], span.clone());
            self.insertion_mode = InsertionMode::InColumnGroup;
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        if matches!(name, "tbody" | "tfoot" | "thead") {
            self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            self.insertion_mode = InsertionMode::InTableBody;
            return;
        }
        if matches!(name, "td" | "th" | "tr") {
            self.insert_element("tbody".to_string(), Namespace::Html, &[], span.clone());
            self.insertion_mode = InsertionMode::InTableBody;
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        if name == "table" {
            // Parse error; if there's a table in table scope, pop it and reprocess — otherwise FALL THROUGH to the
            // anything-else (the 0.90 reprocesses the token, fostering the table — the corpus pins it:
            // `<template><template><table>Foo` gets its table inside the template).
            if self.has_in_table_scope("table") {
                self.pop_until("table");
                self.reset_insertion_mode();
                self.process_start_tag(name, attributes, self_closing, span);
                return;
            }
        }
        if matches!(name, "style" | "script" | "template") {
            self.start_in_head(name, attributes, self_closing, span);
            return;
        }
        if name == "input" {
            let type_attr = attributes
                .iter()
                .find(|attr| attr.name == "type")
                .map(|attr| attr.value.to_ascii_lowercase());
            if type_attr.as_deref() == Some("hidden") {
                // Parse error; insert and pop.
                self.insert_element("input".to_string(), Namespace::Html, attributes, span);
                self.pop_open();
                return;
            }
        }
        if name == "form" {
            // Parse error; if there's no template and no form pointer, insert.
            if self.form_pointer.is_none()
                && !self
                    .open_elements
                    .iter()
                    .any(|node| self.nodes[node.0].name == "template")
            {
                let form = self.insert_element("form".to_string(), Namespace::Html, attributes, span);
                self.form_pointer = Some(form);
                self.pop_open();
            }
            return;
        }
        // Anything else: foster-parenting flag on, process in body.
        let previous = self.foster_parenting;
        self.foster_parenting = true;
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_start_tag(name, attributes, self_closing, span);
        self.foster_parenting = previous;
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InBody {
            self.insertion_mode = InsertionMode::InTable;
        }
    }

    fn start_in_caption(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if matches!(
            name,
            "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
        ) {
            if !self.has_in_table_scope("caption") {
                // Parse error; ignore.
                return;
            }
            self.generate_implied_end_tags(None);
            self.pop_until("caption");
            // the law: the caption end clears the formatting list UP TO AND INCLUDING the last marker (the entries
            // below it — fostered formatting elements — survive for reconstruction).
            self.clear_active_formatting_to_marker();
            self.insertion_mode = InsertionMode::InTable;
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        // Anything else: process in body.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_start_tag(name, attributes, self_closing, span);
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InBody {
            self.insertion_mode = InsertionMode::InCaption;
        }
    }

    fn start_in_column_group(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if name == "html" {
            self.start_in_body(name, attributes, self_closing, span);
            return;
        }
        if name == "col" {
            self.insert_element("col".to_string(), Namespace::Html, attributes, span);
            self.pop_open();
            return;
        }
        if name == "template" {
            self.start_template(name, attributes, span);
            return;
        }
        // Anything else: when the current node is not a colgroup, this is a parse error and the token is IGNORED
        // (reprocessing it in the same mode would loop). Otherwise close the colgroup and reprocess.
        if self
            .current_node()
            .is_some_and(|node| self.nodes[node.0].name == "colgroup")
        {
            self.pop_open_element();
            self.insertion_mode = InsertionMode::InTable;
            self.process_start_tag(name, attributes, self_closing, span);
        }
    }

    fn start_in_table_body(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if name == "tr" {
            self.insert_element("tr".to_string(), Namespace::Html, attributes, span);
            self.insertion_mode = InsertionMode::InRow;
            return;
        }
        if matches!(name, "th" | "td") {
            // the law: the implied tr and the reprocess — no formatting clear (the fostered formatting elements survive
            // for the reconstruction law).
            self.insert_element("tr".to_string(), Namespace::Html, &[], span.clone());
            self.insertion_mode = InsertionMode::InRow;
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        if matches!(name, "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead") {
            // Parse error; if there's a tbody/tfoot/thead in table scope, pop and reprocess.
            for section in ["tbody", "tfoot", "thead"] {
                if self.has_in_table_scope(section) {
                    self.pop_until(section);
                    self.insertion_mode = InsertionMode::InTable;
                    self.process_start_tag(name, attributes, self_closing, span);
                    return;
                }
            }
            return;
        }
        // Anything else: process in table.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InTable;
        self.process_start_tag(name, attributes, self_closing, span);
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InTable {
            self.insertion_mode = InsertionMode::InTableBody;
        }
    }

    fn start_in_row(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if matches!(name, "th" | "td") {
            // The row-context clear (the clearStackToTableRowContext): a fostered element above the row is popped so
            // the cell lands inside the TR — but a TEMPLATE stops the clear (the corpus law: `<td>` inside a template
            // in a table section stays in the template).
            while let Some(node) = self.current_node() {
                if matches!(self.nodes[node.0].name.as_str(), "tr" | "template" | "html") {
                    break;
                }
                self.pop_open_element();
            }
            self.insert_element(name.to_string(), Namespace::Html, attributes, span);
            self.insertion_mode = InsertionMode::InCell;
            self.active_formatting.push(FormattingEntry::Marker);
            return;
        }
        if matches!(
            name,
            "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead" | "tr"
        ) {
            if !self.has_in_table_scope("tr") {
                // Parse error; ignore.
                return;
            }
            self.pop_until("tr");
            self.insertion_mode = InsertionMode::InTableBody;
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        // Anything else: process in table body.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InTableBody;
        self.process_start_tag(name, attributes, self_closing, span);
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InTableBody {
            self.insertion_mode = InsertionMode::InRow;
        }
    }

    fn start_in_cell(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if matches!(
            name,
            "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th" | "thead" | "tr"
        ) {
            if !self.has_in_table_scope("td") && !self.has_in_table_scope("th") {
                // Parse error; ignore.
                return;
            }
            self.close_cell();
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        // Anything else: process in body.
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_start_tag(name, attributes, self_closing, span);
        self.delegation_origin = saved_delegation;
        if self.insertion_mode == InsertionMode::InBody {
            self.insertion_mode = InsertionMode::InCell;
        }
    }

    fn start_in_select(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if name == "html" {
            self.start_in_body(name, attributes, self_closing, span);
            return;
        }
        if name == "option" {
            if self
                .current_node()
                .is_some_and(|node| self.nodes[node.0].name == "option")
            {
                self.pop_open_element();
            }
            self.insert_element("option".to_string(), Namespace::Html, attributes, span);
            return;
        }
        if name == "optgroup" {
            if self
                .current_node()
                .is_some_and(|node| self.nodes[node.0].name == "option")
            {
                self.pop_open_element();
            }
            if self
                .current_node()
                .is_some_and(|node| self.nodes[node.0].name == "optgroup")
            {
                self.pop_open_element();
            }
            self.insert_element("optgroup".to_string(), Namespace::Html, attributes, span);
            return;
        }
        if name == "select" {
            // Parse error; act as an end tag.
            self.process_end_tag("select", span);
            return;
        }
        if matches!(name, "input" | "keygen" | "textarea") {
            // Parse error; act as a select end tag, then reprocess.
            self.process_end_tag("select", span.clone());
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        if matches!(
            name,
            "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th"
        ) {
            // the law: the START tags for the table family are IGNORED in "in select" (only the end tags break out —
            // see [`TreeBuilder::end_in_select`]). The corpus pins it: `<table><select><option>A<tr>` keeps the tr out
            // of the select's own mode until the table path re-enters.
            return;
        }
        if name == "script" || name == "template" {
            self.start_in_head(name, attributes, self_closing, span);
            return;
        }
        // Anything else: parse error; ignore.
    }

    fn start_in_select_in_table(
        &mut self,
        name: &str,
        attributes: &[Attribute],
        self_closing: bool,
        span: Range<usize>,
    ) {
        if matches!(
            name,
            "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th"
        ) {
            // Parse error; close the select and reprocess.
            self.pop_until("select");
            self.reset_insertion_mode();
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        self.start_in_select(name, attributes, self_closing, span);
    }

    fn start_in_template(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        match name {
            "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style" | "template"
            | "title" => {
                self.start_in_head(name, attributes, self_closing, span);
            }
            "caption" | "colgroup" | "tbody" | "tfoot" | "thead" => {
                // The spec's own law: POP the current template insertion mode, PUSH the target mode, switch, and
                // REPROCESS — the reprocess routes through the table modes (so a `<tr>` after `</tr>` nests nothing and
                // a caption with no section in scope is ignored, exactly as the corpus pins).
                self.template_modes.pop();
                self.template_modes.push(InsertionMode::InTable);
                self.insertion_mode = InsertionMode::InTable;
                self.process_start_tag(name, attributes, self_closing, span);
            }
            "col" => {
                self.template_modes.pop();
                self.template_modes.push(InsertionMode::InColumnGroup);
                self.insertion_mode = InsertionMode::InColumnGroup;
                self.process_start_tag(name, attributes, self_closing, span);
            }
            "tr" => {
                self.template_modes.pop();
                self.template_modes.push(InsertionMode::InTableBody);
                self.insertion_mode = InsertionMode::InTableBody;
                self.process_start_tag(name, attributes, self_closing, span);
            }
            "td" | "th" => {
                self.template_modes.pop();
                self.template_modes.push(InsertionMode::InRow);
                self.insertion_mode = InsertionMode::InRow;
                self.process_start_tag(name, attributes, self_closing, span);
            }
            _ => {
                // Any other start tag: pop, push "in body", switch, and reprocess — the corpus pins the pushed mode:
                // `<template> <div>` at EOF must still find a live template mode and create the body.
                self.template_modes.pop();
                self.template_modes.push(InsertionMode::InBody);
                self.insertion_mode = InsertionMode::InBody;
                self.process_start_tag(name, attributes, self_closing, span);
            }
        }
    }

    fn start_after_body(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if name == "html" {
            self.start_in_body(name, attributes, self_closing, span);
            return;
        }
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_start_tag(name, attributes, self_closing, span);
        self.delegation_origin = saved_delegation;
        // the after-body law: the reprocess STAYS in "in body" — there is no restore. The corpus pins it: `x<!-- Hi
        // -->` after `</html>` puts the comment in the BODY (mode in body), and only a further `</html>` returns to
        // "after body".
    }

    fn start_in_frameset(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if name == "html" {
            self.start_in_body(name, attributes, self_closing, span);
            return;
        }
        if name == "frameset" {
            self.insert_element("frameset".to_string(), Namespace::Html, attributes, span);
            return;
        }
        if name == "frame" {
            self.insert_element("frame".to_string(), Namespace::Html, attributes, span);
            self.pop_open();
            return;
        }
        if name == "noframes" {
            self.start_in_head(name, attributes, self_closing, span);
            return;
        }
        // Anything else: parse error; ignore.
    }

    fn start_after_after_body(&mut self, name: &str, attributes: &[Attribute], self_closing: bool, span: Range<usize>) {
        if name == "html" {
            self.start_in_body(name, attributes, self_closing, span);
            return;
        }
        let saved_delegation = self.delegation_origin;
        if self.delegation_origin.is_none() {
            self.delegation_origin = Some(self.insertion_mode);
        }
        self.insertion_mode = InsertionMode::InBody;
        self.process_start_tag(name, attributes, self_closing, span);
        self.delegation_origin = saved_delegation;
        // the after-after-body law: the reprocess STAYS in body.
    }

    // ------------------------------------------------------------------ Foreign content
    // ------------------------------------------------------------------

    fn start_in_foreign_content(
        &mut self,
        name: &str,
        attributes: &[Attribute],
        self_closing: bool,
        span: Range<usize>,
    ) {
        // If the adjusted current node is an element in the HTML namespace, or an HTML integration point (or a MathML
        // text integration point for anything but mglyph/malignmark), process the token using the rules of the mode
        // that was current BEFORE foreign content — the mainLoop's phase selection law.
        let integration_point = self.adjusted_current_node().is_some_and(|node| {
            let element = &self.nodes[node.0];
            element.ns == Namespace::Html
                || is_html_integration_point(element)
                || (is_mathml_text_integration_point(element) && !matches!(name, "mglyph" | "malignmark"))
        });
        if integration_point {
            if let Some(origin) = self.foreign_origin_mode {
                self.insertion_mode = origin;
            }
            self.process_start_tag(name, attributes, self_closing, span);
            // Restore the foreign overlay for the NEXT token when the current node is still foreign (the mode model:
            // the overlay persists; transitions through the real mode move it).
            if self.insertion_mode != InsertionMode::InForeignContent
                && self
                    .adjusted_current_node()
                    .is_some_and(|node| self.nodes[node.0].ns != Namespace::Html)
            {
                self.insertion_mode = InsertionMode::InForeignContent;
            }
            return;
        }
        if matches!(
            name,
            "b" | "big"
                | "blockquote"
                | "body"
                | "br"
                | "center"
                | "code"
                | "dd"
                | "div"
                | "dl"
                | "dt"
                | "em"
                | "embed"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "head"
                | "hr"
                | "i"
                | "img"
                | "li"
                | "listing"
                | "menu"
                | "meta"
                | "nobr"
                | "ol"
                | "p"
                | "pre"
                | "ruby"
                | "s"
                | "small"
                | "span"
                | "strong"
                | "strike"
                | "sub"
                | "sup"
                | "table"
                | "tt"
                | "u"
                | "ul"
                | "var"
        ) || (name == "font"
            && attributes
                .iter()
                .any(|attr| matches!(attr.name.as_str(), "color" | "face" | "size")))
        {
            // Parse error; pop the foreign nodes (stopping at an HTML integration point or a MathML text integration
            // point — the pop-loop law) and REPROCESS the token through the mode that was current BEFORE foreign
            // content.
            while let Some(node) = self.current_node() {
                let element = &self.nodes[node.0];
                if element.ns == Namespace::Html
                    || is_html_integration_point(element)
                    || is_mathml_text_integration_point(element)
                {
                    break;
                }
                self.pop_open_element();
            }
            if let Some(origin) = self.foreign_origin_mode.take() {
                self.insertion_mode = origin;
            }
            self.process_start_tag(name, attributes, self_closing, span);
            return;
        }
        if name == "svg" {
            // A nested svg: insert in the SVG namespace.
            let element = self.insert_foreign_element("svg", Namespace::Svg, attributes, span);
            process_foreign_attributes(&mut self.nodes[element.0]);
            if self_closing {
                self.pop_open();
            }
            return;
        }
        if name == "math" {
            let element = self.insert_foreign_element("math", Namespace::MathMl, attributes, span);
            process_foreign_attributes(&mut self.nodes[element.0]);
            if self_closing {
                self.pop_open();
            }
            return;
        }
        // Any other start tag: insert in the adjusted current node's namespace.
        let ns = self
            .adjusted_current_node()
            .map(|node| self.nodes[node.0].ns)
            .unwrap_or(Namespace::Html);
        let element = self.insert_foreign_element(name, ns, attributes, span);
        let _ = element;
        if self_closing {
            self.pop_open();
        }
    }

    fn end_in_foreign_content(&mut self, name: &str, span: Range<usize>) {
        // The corpus law (2010's foreign end): the end tag is processed by the mode that was current BEFORE foreign
        // content (its scope checks stop at the SVG foreignObject), and the foreign overlay returns for the next token
        // while the current node is still foreign. The origin may be None after a breakout took it; fall back to InBody
        // rather than recursing through the foreign dispatch.
        let origin = self.foreign_origin_mode.unwrap_or(InsertionMode::InBody);
        self.insertion_mode = origin;
        self.delegating = true;
        // The SVG tag-name adjustment applies to END tags too (the adjustSVGTagNames before the secondary phase's
        // walk): the token `</foreignobject>` must match the stored `foreignObject`.
        let adjusted = adjust_svg_name(name);
        self.process_end_tag(&adjusted, span);
        self.delegating = false;
        if self.insertion_mode != InsertionMode::InForeignContent
            && self
                .adjusted_current_node()
                .is_some_and(|node| self.nodes[node.0].ns != Namespace::Html)
        {
            self.insertion_mode = InsertionMode::InForeignContent;
        }
    }

    // ------------------------------------------------------------------ Comments and doctypes
    // ------------------------------------------------------------------

    fn process_comment(&mut self, data: &str, _span: Range<usize>) {
        // The AfterBody family appends comments to the HTML ELEMENT (the `insertComment(token, openElements[0])` law);
        // the AfterAfterBody and the frameset edges append to the DOCUMENT (the after-after-body law; the §4.10
        // document-root comment role).
        let comment = self.new_comment(data.to_string());
        if matches!(self.insertion_mode, InsertionMode::InTable | InsertionMode::InTableText) {
            // the in-table comment law: the pending table text flushes FIRST (into the current node), then the comment
            // appends to the current node — the corpus pins both inside the table: `<table> <!--foo-->` keeps its
            // spaces.
            if self.insertion_mode == InsertionMode::InTableText {
                self.flush_pending_table_text();
            }
            self.append_child(self.current_node().unwrap_or(self.document), comment);
            return;
        }
        if self.insertion_mode == InsertionMode::AfterBody {
            // the after-body law: the comment appends to the first element (the html element).
            let html = self.open_elements.first().copied().unwrap_or(self.document);
            self.append_child(html, comment);
            return;
        }
        // The frameset edges: an "in frameset" comment appends to the CURRENT node (the frameset itself — the corpus
        // pins the depth), while "after frameset" appends to the html element (the frameset has been popped; the
        // corpus's `<!-- 3 -->` after `</frameset>` sits at the html level).
        if self.insertion_mode == InsertionMode::InFrameset {
            self.append_child(self.current_node().unwrap_or(self.document), comment);
            return;
        }
        if self.insertion_mode == InsertionMode::AfterFrameset {
            let html = self.open_elements.first().copied().unwrap_or(self.document);
            self.append_child(html, comment);
            return;
        }
        // The after-after family appends to the Document (the §4.10 document-root comment role).
        let document_edge = matches!(
            self.insertion_mode,
            InsertionMode::AfterAfterBody | InsertionMode::AfterAfterFrameset
        );
        if document_edge {
            self.append_child(self.document, comment);
            return;
        }
        let (parent, index) = self.appropriate_insertion_place();
        match index {
            Some(index) => self.insert_at(parent, index, comment),
            None => self.append_child(parent, comment),
        }
    }

    fn process_doctype(
        &mut self,
        name: Option<String>,
        public_identifier: Option<String>,
        system_identifier: Option<String>,
        force_quirks: bool,
        _span: Range<usize>,
    ) {
        if self.insertion_mode != InsertionMode::Initial {
            // Parse error; ignore.
            return;
        }
        let doctype = DoctypeData {
            name,
            public_identifier,
            system_identifier,
            force_quirks,
        };
        let node = self.new_doctype(doctype.clone());
        self.append_child(self.document, node);
        self.quirks = self.determine_quirks(&doctype);
        self.insertion_mode = InsertionMode::BeforeHtml;
    }

    /// The quirks-mode determination (the doctype rules).
    fn determine_quirks(&self, doctype: &DoctypeData) -> QuirksMode {
        if doctype.force_quirks {
            return QuirksMode::Quirks;
        }
        let name = doctype.name.as_deref().unwrap_or("");
        let public = doctype.public_identifier.as_deref().unwrap_or("");
        let system = doctype.system_identifier.as_deref().unwrap_or("");
        if name != "html" {
            return QuirksMode::Quirks;
        }
        if public.starts_with("-//W3O//DTD W3 HTML Strict 3.0//EN//")
            || public.starts_with("-/W3C/DTD HTML 4.0 Transitional/EN")
            || public.starts_with("HTML")
        {
            return QuirksMode::Quirks;
        }
        let lower_public = public.to_ascii_lowercase();
        if lower_public.starts_with("-//w3o//dtd w3 html strict 3.0//en//")
            || lower_public.starts_with("-/w3c/dtd html 4.0 transitional/en")
            || lower_public.starts_with("html")
            || (system.is_empty()
                && (lower_public.starts_with("-//w3c//dtd html 4.01 frameset//")
                    || lower_public.starts_with("-//w3c//dtd html 4.01 transitional//")))
        {
            return QuirksMode::Quirks;
        }
        if system.starts_with("http://www.ibm.com/data/dtd/v11/ibmxhtml1-transitional.dtd") {
            return QuirksMode::Quirks;
        }
        // The limited-quirks cases.
        let limited = public.starts_with("-//W3C//DTD XHTML 1.0 Frameset//")
            || public.starts_with("-//W3C//DTD XHTML 1.0 Transitional//")
            || (system.is_empty()
                && (public.starts_with("-//W3C//DTD HTML 4.01 Frameset//")
                    || public.starts_with("-//W3C//DTD HTML 4.01 Transitional//")));
        if limited {
            return QuirksMode::LimitedQuirks;
        }
        QuirksMode::NoQuirks
    }

    // ------------------------------------------------------------------ The insertion-mode reset
    // ------------------------------------------------------------------

    fn reset_insertion_mode(&mut self) {
        for index in (0..self.open_elements.len()).rev() {
            let node = self.open_elements[index];
            let is_last = index == 0;
            // WHATWG fragment substitution: at the root node, a fragment parser matches the CONTEXT element, never the
            // bare `html` wrapper. Without this, `</table>` / `</select>` reset into BeforeHead/AfterHead and fabricate
            // a head/body.
            let name = if is_last && self.fragment_context.is_some() {
                self.fragment_context.as_deref().unwrap_or("div")
            } else {
                self.nodes[node.0].name.as_str()
            };
            let mode = match name {
                "select" => {
                    // the law: a select RESET inside a table context takes "in select in table" (the corpus pins it:
                    // the caption after `</template>` inside a select in a cell returns to the table). A template
                    // earlier in the stack does the same.
                    if self
                        .open_elements
                        .iter()
                        .take(index)
                        .any(|earlier| matches!(self.nodes[earlier.0].name.as_str(), "template" | "table"))
                    {
                        InsertionMode::InSelectInTable
                    } else {
                        InsertionMode::InSelect
                    }
                }
                "td" | "th" if !is_last => InsertionMode::InCell,
                "tr" => InsertionMode::InRow,
                "tbody" | "thead" | "tfoot" => InsertionMode::InTableBody,
                "caption" => InsertionMode::InCaption,
                "colgroup" => InsertionMode::InColumnGroup,
                "table" => InsertionMode::InTable,
                "template" => {
                    if let Some(&mode) = self.template_modes.last() {
                        mode
                    } else {
                        InsertionMode::InTemplate
                    }
                }
                "head" if !is_last => InsertionMode::InHead,
                "body" => InsertionMode::InBody,
                "frameset" => InsertionMode::InFrameset,
                "html" => {
                    if self.head_pointer.is_none() {
                        InsertionMode::BeforeHead
                    } else {
                        InsertionMode::AfterHead
                    }
                }
                _ => {
                    if is_last {
                        self.insertion_mode = InsertionMode::InBody;
                        return;
                    }
                    // Not the root: keep walking (the law — an unknown non-root element does not decide the mode).
                    continue;
                }
            };
            // The FIRST match decides (the law): the topmost element that names a mode sets it and the walk returns.
            self.insertion_mode = mode;
            return;
        }
        self.insertion_mode = InsertionMode::InBody;
    }

    // ------------------------------------------------------------------ EOF
    // ------------------------------------------------------------------

    fn process_eof_by_mode(&mut self) {
        if self.insertion_mode == InsertionMode::InForeignContent {
            // The corpus's foreign EOF law: the EOF routes through the mode that was current BEFORE the foreign content
            // — the template/head chain that creates the body ([99] pins it).
            if let Some(origin) = self.foreign_origin_mode {
                self.insertion_mode = origin;
                self.process_eof_by_mode();
                return;
            }
            return;
        }
        match self.insertion_mode {
            InsertionMode::Initial
            | InsertionMode::BeforeHtml
            | InsertionMode::BeforeHead
            | InsertionMode::InHead
            | InsertionMode::AfterHead => {
                // The stop-parsing preamble: create html, head, and body (the mode chain a body start tag would have
                // run).
                if self.open_elements.is_empty() {
                    let html = self.new_element("html".to_string(), Namespace::Html, Vec::new());
                    self.append_child(self.document, html);
                    self.push_open(html);
                }
                if self.head_pointer.is_none() {
                    let head = self.new_element("head".to_string(), Namespace::Html, Vec::new());
                    self.append_child(self.current_node().unwrap_or(self.document), head);
                    self.push_open(head);
                    self.head_pointer = Some(head);
                }
                while let Some(node) = self.current_node() {
                    if self.nodes[node.0].name == "body" {
                        break;
                    }
                    if self.nodes[node.0].name == "html" {
                        let body = self.new_element("body".to_string(), Namespace::Html, Vec::new());
                        self.append_child(self.current_node().unwrap_or(self.document), body);
                        self.push_open(body);
                        break;
                    }
                    self.pop_open_element();
                }
                self.insertion_mode = InsertionMode::InBody;
            }
            InsertionMode::InBody => {
                // EOF in "in body" DELEGATES to "in template" while a template is open (the spec's own law), then stops
                // parsing: the template pop at EOF closes the contents and the reset-mode chain creates the body the
                // preamble skipped.
                if self.template_modes.is_empty() {
                    return;
                }
                self.insertion_mode = InsertionMode::InTemplate;
                self.process_eof_by_mode();
            }
            InsertionMode::InTableText => {
                self.flush_pending_table_text();
                self.process_eof_by_mode();
            }
            InsertionMode::Text => {
                // Pop the raw-text element and return to the original mode.
                self.pop_open_element();
                self.insertion_mode = self.original_insertion_mode;
                self.process_eof_by_mode();
            }
            InsertionMode::InCaption | InsertionMode::InColumnGroup | InsertionMode::InTable => {
                // The current spec's delegation law: EOF in these three modes is processed using the "in body" rules —
                // NO popping — which routes through the template-modes check and lands the stop-parsing chain on the
                // head-ish preamble that creates the body. The old pop-until-table chain would pop a live template off
                // the stack and strand the EOF (the corpus pins the delegation: `<template><template><tbody>` gets its
                // body from this chain).
                self.insertion_mode = InsertionMode::InBody;
                self.process_eof_by_mode();
            }
            InsertionMode::InTableBody | InsertionMode::InRow => {
                // the law: EOF in these two modes delegates to the "in table" rules UNCONDITIONALLY — no scope check,
                // no popping. The corpus pins it: `<template><template><tr>` at EOF (no section in scope) must still
                // route through the template chain and create the body.
                self.insertion_mode = InsertionMode::InTable;
                self.process_eof_by_mode();
            }
            InsertionMode::InCell => {
                // the law: EOF in "in cell" delegates to the "in body" rules (which route through the template-modes
                // check). The corpus pins it via the template EOF chains.
                self.insertion_mode = InsertionMode::InBody;
                self.process_eof_by_mode();
            }
            InsertionMode::InSelect | InsertionMode::InSelectInTable => {
                // The corpus's EOF law for select: pop until the select is popped (when in select scope), reset the
                // insertion mode, and reprocess — the chain that lets a template's body appear
                // (`<template><template><tbody><select>` gets its body from this). A plain `<select>` at EOF resets to
                // "in body" and stops — the same tree.
                if !self.has_in_select_scope("select") {
                    return;
                }
                while let Some(node) = self.pop_open() {
                    if self.nodes[node.0].name == "select" {
                        break;
                    }
                }
                self.reset_insertion_mode();
                self.process_eof_by_mode();
            }
            InsertionMode::InTemplate => {
                if self
                    .open_elements
                    .iter()
                    .any(|node| self.nodes[node.0].name == "template")
                {
                    // Parse error; close the template.
                    self.end_template(0..0);
                    self.process_eof_by_mode();
                }
            }
            InsertionMode::AfterBody | InsertionMode::AfterAfterBody => {
                self.insertion_mode = InsertionMode::InBody;
                self.process_eof_by_mode();
            }
            InsertionMode::InFrameset => {
                // Stop.
            }
            InsertionMode::AfterFrameset | InsertionMode::AfterAfterFrameset => {}
            _ => {}
        }
    }
}

/// The foreign attribute adjustments (MathML and SVG tables).
fn process_foreign_attributes(element: &mut Node) {
    for attribute in &mut element.attrs {
        if element.ns == Namespace::MathMl {
            attribute.name = adjust_mathml_attr(&attribute.name);
        } else if element.ns == Namespace::Svg {
            attribute.name = adjust_svg_attr(&attribute.name);
        }
    }
}

impl TreeBuilder {
    /// Pushes one formatting entry with the NOAH'S ARK law: when the list already holds THREE entries with the same tag
    /// name, namespace, and attributes after the last marker, the OLDEST matching entry is removed first (the
    /// addFormattingElement).
    fn push_formatting_entry(&mut self, element: NodeId) {
        let mut matches = 0usize;
        for entry in self.active_formatting.iter().rev() {
            match entry {
                FormattingEntry::Marker => break,
                FormattingEntry::Element(node) => {
                    if self.nodes[node.0].name == self.nodes[element.0].name
                        && self.nodes[node.0].ns == self.nodes[element.0].ns
                        && self.nodes[node.0]
                            .attrs
                            .iter()
                            .map(|a| (a.name.as_str(), a.value.as_str()))
                            .eq(self.nodes[element.0]
                                .attrs
                                .iter()
                                .map(|a| (a.name.as_str(), a.value.as_str())))
                    {
                        matches += 1;
                        if matches == 3 {
                            let oldest = *node;
                            self.active_formatting
                                .retain(|entry| !matches!(entry, FormattingEntry::Element(n) if *n == oldest));
                            break;
                        }
                    }
                }
            }
        }
        self.active_formatting.push(FormattingEntry::Element(element));
    }
}

impl FormattingEntry {
    fn clone_entry(&self) -> FormattingEntry {
        match self {
            FormattingEntry::Element(node) => FormattingEntry::Element(*node),
            FormattingEntry::Marker => FormattingEntry::Marker,
        }
    }
}

#[cfg(test)]
mod smoke_tests {
    use super::*;

    #[test]
    fn builds_a_simple_document() {
        let tree = TreeBuilder::build("<html><body><p>hi</p></body></html>");
        // No doctype means QUIRKS mode. A proper doctype sets NoQuirks — see `a_proper_html_doctype_is_no_quirks`.
        assert_eq!(tree.quirks, QuirksMode::Quirks);
        let document = &tree.nodes[tree.document.0];
        assert_eq!(document.children.len(), 1);
    }

    #[test]
    fn builds_fragments_under_their_context() {
        // The fragment's `document` is the bare html root; its children are the fragment content.
        let div = TreeBuilder::build_fragment("direct div content", "div");
        let root = &div.nodes[div.document.0];
        assert_eq!(root.name, "html");
        assert_eq!(root.children.len(), 1);
        assert_eq!(div.nodes[root.children[0].0].data, "direct div content");

        // A textarea CONTEXT starts the tokenizer in RCDATA: `<em>` is text.
        let textarea = TreeBuilder::build_fragment("a <em>pseudo</em> b", "textarea");
        let root = &textarea.nodes[textarea.document.0];
        assert_eq!(root.children.len(), 1);
        assert_eq!(textarea.nodes[root.children[0].0].data, "a <em>pseudo</em> b");

        // An html CONTEXT runs the preamble chain: head and body appear.
        let html = TreeBuilder::build_fragment("setting html's innerHTML", "html");
        let root = &html.nodes[html.document.0];
        let names: Vec<&str> = root
            .children
            .iter()
            .map(|child| html.nodes[child.0].name.as_str())
            .collect();
        assert_eq!(names, vec!["head", "body"]);
    }

    fn child_names(tree: &Tree, parent: NodeId) -> Vec<&str> {
        tree.nodes[parent.0]
            .children
            .iter()
            .map(|child| tree.nodes[child.0].name.as_str())
            .collect()
    }

    #[test]
    fn fragment_table_close_does_not_fabricate_head_or_body() {
        // After `</table>` the insertion-mode reset walks to the bare html root; a fragment must match the context
        // element, not BeforeHead.
        let tree = TreeBuilder::build_fragment("<table></table>hello", "div");
        let root = tree.document;
        let names = child_names(&tree, root);
        assert!(
            !names.contains(&"head") && !names.contains(&"body"),
            "fragment must not fabricate head/body: {names:?}"
        );
        assert!(names.contains(&"table"), "table must survive: {names:?}");
        assert!(
            tree.nodes[root.0]
                .children
                .iter()
                .any(|child| tree.nodes[child.0].kind == NodeKind::Text && tree.nodes[child.0].data == "hello"),
            "trailing text must stay a sibling of the table"
        );
    }

    #[test]
    fn noscript_in_body_parses_as_elements_with_entity_decoding() {
        let tree = TreeBuilder::build("<body><noscript><p>x</noscript>");
        let html = tree.document_element().expect("html");
        let body = tree.nodes[html.0]
            .children
            .iter()
            .find(|child| tree.nodes[child.0].name == "body")
            .copied()
            .expect("body");
        let noscript = tree.nodes[body.0]
            .children
            .iter()
            .find(|child| tree.nodes[child.0].name == "noscript")
            .copied()
            .expect("noscript");
        let names = child_names(&tree, noscript);
        assert_eq!(names, vec!["p"]);
        let p = tree.nodes[noscript.0].children[0];
        assert_eq!(tree.nodes[p.0].children.len(), 1);
        assert_eq!(tree.nodes[tree.nodes[p.0].children[0].0].data, "x");

        let tree = TreeBuilder::build("<body><noscript>&amp;</noscript>");
        let html = tree.document_element().expect("html");
        let body = tree.nodes[html.0]
            .children
            .iter()
            .find(|child| tree.nodes[child.0].name == "body")
            .copied()
            .expect("body");
        let noscript = tree.nodes[body.0]
            .children
            .iter()
            .find(|child| tree.nodes[child.0].name == "noscript")
            .copied()
            .expect("noscript");
        assert_eq!(tree.nodes[noscript.0].children.len(), 1);
        assert_eq!(tree.nodes[tree.nodes[noscript.0].children[0].0].data, "&");
    }

    /// A proper HTML doctype is NoQuirks. The no-doctype sibling `builds_a_simple_document` pins Quirks.
    #[test]
    fn a_proper_html_doctype_is_no_quirks() {
        let tree = TreeBuilder::build("<!DOCTYPE html><p>x");
        assert_eq!(tree.quirks, QuirksMode::NoQuirks);
    }

    #[test]
    fn template_col_terminates() {
        for input in [
            "<body>",
            "<body><template>",
            "<body><template><col>",
            "<body><template><col><colgroup>",
        ] {
            let _ = TreeBuilder::build(input);
        }
    }

    #[test]
    fn tricky_inputs_terminate() {
        for input in [
            "<table><tr><td>a<td>b",
            "<b><i>text</b></i>",
            "<select><option>a<option>b",
            "<template><div>x</div></template>",
            "<svg><foreignObject><p>x</p></foreignObject></svg>",
            "<table>x</table>",
            "<p>a<p>b",
            "a<br>b",
            "<form><form>",
            "<!doctype html><html><body>",
        ] {
            let _ = TreeBuilder::build(input);
        }
    }
}
