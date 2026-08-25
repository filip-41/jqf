//! The SELECTOR families: `xpath/1` and `css/1`, the engine's two doors onto the codec-native selector seam
//! (`jqf-selector`).
//!
//! The seam is codec-agnostic machinery in a shared crate; these two overloads are the engine's dispatch into it.
//! `xpath/1` executes the `xml.xpath@1` profile (the XML codec's closed `XPath 3.1` subset) and `css/1` executes the
//! `html.css@1` profile (the HTML codec's static Selectors Level 4 surface). Neither name exists in the reference, so
//! both are `jqf-extension` families under the standing collision law.
//!
//! The argument law is the ordinary argument product: the call answers once per output of its selector-text argument,
//! and an argument that yields nothing makes the call yield nothing. Each activation requires a LOCATED input whose
//! document format matches the profile's declared format; the format mismatch, a missing document authority, a compile
//! rejection, and a budget exhaustion are all catchable string raises (the `try` barrier can absorb them).

use alloc::format;
use alloc::string::String;

use super::id;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};

/// The selector law discriminants: which profile one call executes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectorLaw {
    /// `xpath/1` — the `xml.xpath@1` profile.
    XPath,
    /// `css/1` — the `html.css@1` profile.
    Css,
}

impl SelectorLaw {
    /// The profile's stable identity text.
    #[must_use]
    pub const fn language(self) -> crate::selector::SelectorLanguage {
        match self {
            Self::XPath => crate::selector::SelectorLanguage::XmlXPath1,
            Self::Css => crate::selector::SelectorLanguage::HtmlCss1,
        }
    }

    /// The builtin's user-facing name (for error messages).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::XPath => "xpath",
            Self::Css => "css",
        }
    }
}

const ONE_VALUE: &[ParameterKind] = &[ParameterKind::Value];

const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "jqf-extension",
        summary,
        detail,
    }
}

const fn example(program: &'static str, input: &'static str, expected: &'static str) -> BuiltinExample {
    BuiltinExample {
        program,
        input,
        expected,
    }
}

const fn overload(
    id: u16,
    family_id: u16,
    name: &'static str,
    examples: &'static [BuiltinExample],
) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(id),
        family: BuiltinFamilyId::new(family_id),
        canonical_name: name,
        arity: 1,
        parameters: ONE_VALUE,
        execution: BuiltinExecution::Evaluator,
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: Effects::Pure,
        examples,
    }
}

/// The two selector families.
pub const FAMILIES: &[BuiltinFamilyRecord] = &[
    family(
        id::XPATH_FAMILY_ID,
        "xpath",
        "Selects elements of an XML document with the xml.xpath@1 profile.",
        concat!(
            "The closed XPath 3.1 subset: absolute ",
            "and relative paths, the child/descendant/descendant-or-self/parent/self ",
            "element axes, expanded-name and wildcard tests, union, and predicates — ",
            "position ([N], [position() = last()]), attribute/text()/string(.) ",
            "equality, and the general COMPARISON law (=, !=, <, <=, ",
            ">, >= over literals, @attributes, text(), string(.), name(), position(), ",
            "last(), and the pure functions count(), concat(), string-length(), ",
            "name()), compared numerically when either side is a number and as ",
            "strings otherwise (XPath 1.0's general-comparison law). Results are ",
            "elements only, in document order, deduplicated — except a TOP-LEVEL ",
            "function call, whose result is the SCALAR XPath 1.0 assigns (the ",
            "elements-only law widens for it): count(path) is ",
            "the node-set's cardinality (an exact integer), concat(...) and ",
            "string-length(atom) answer strings/numbers, and name() is the empty ",
            "string over the document-node context. The attribute axis and ",
            "the text() result axis are deliberately NOT result axes: an ",
            "attribute is a fact, not a node, so it has no place in a node-set — ",
            "read it with the .& attribute accessor, and use the element text() the ",
            "same way an element node's own value is read. A union of a scalar ",
            "result with a path is an XPath type error, rejected at compile. The ",
            "input must be a located XML document; a format ",
            "mismatch, a compile rejection, or a budget exhaustion is a catchable ",
            "error.",
        ),
    ),
    family(
        id::CSS_FAMILY_ID,
        "css",
        "Selects elements of an HTML document with the html.css@1 profile.",
        concat!(
            "The static-tree-applicable Selectors Level 4 profile: ",
            "type/universal/ID/class/attribute/",
            "namespace selectors, the tree combinators, and the static pseudo-classes ",
            "(:is/:where/:not/:has, the structural :nth-* family, :lang, :dir, :root, ",
            ":empty, :scope). Requires the complete recovered document mode in its ",
            "input authority. Results are elements only, in document order, ",
            "deduplicated.",
        ),
    ),
];

/// The two selector overloads. Both examples pin the FORMAT-MISMATCH law (the example harness's input route is strict
/// JSON, so the honest pin here is the catchable raise a JSON input produces; the XML/HTML end-to-end surface is pinned
/// by the codec smoke lanes and the CLI tests).
pub const OVERLOADS: &[BuiltinOverloadRecord] = &[
    overload(
        id::XPATH_1,
        id::XPATH_FAMILY_ID,
        "xpath",
        &[example(
            "try xpath(\"//item\") catch .",
            "{}",
            "\"xpath serves xml documents; the input is a json document\"\n",
        )],
    ),
    overload(
        id::CSS_1,
        id::CSS_FAMILY_ID,
        "css",
        &[example(
            "try css(\"div.item\") catch .",
            "{}",
            "\"css serves html documents; the input is a json document\"\n",
        )],
    ),
];

/// The selector-seam execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
#[derive(Clone, Copy, Debug)]
pub enum SelectorPayload {
    /// `xpath/1` — the `xml.xpath@1` profile through the selector seam.
    XPath,
    /// `css/1` — the `html.css@1` profile through the selector seam.
    Css,
}

pub const PAYLOADS: &[(u16, SelectorPayload)] =
    &[(id::XPATH_1, SelectorPayload::XPath), (id::CSS_1, SelectorPayload::Css)];

/// Renders one selector failure as the catchable raise text.
pub fn selector_message(law: SelectorLaw, error: &crate::selector::SelectorError) -> String {
    use crate::selector::SelectorError as E;
    let name = law.name();
    match error {
        E::Compile { message, offset } => {
            format!("{name}: {message} (at byte {offset})")
        }
        E::Budget { what } => format!("{name}: selector budget exceeded ({what})"),
        E::FormatMismatch { format, .. } => {
            let served = law.language().format();
            format!("{name} serves {served} documents; the input is a {format} document")
        }
        E::NotMarkup => format!("{name}: the document has no element authority"),
        E::MissingModeAuthority => {
            format!("{name}: html.css@1 requires the recovered document mode")
        }
        E::Allocation | E::Control => format!("{name}: the selector run was refused resources"),
        E::Internal { .. } => format!("{name}: internal selector contract violation"),
    }
}
