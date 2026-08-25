//! The selector language registry: declaration, not implementation.
//!
//! Fact roles are the kernel markup segments. HTML-only mode/pragma strings live on [`SelectorLanguage::HtmlCss1`], not
//! in codec-core.

use jqf_codec_core::markup;

/// One codec-native selector language on the seam.
///
/// The language is the DECLARATION half of the seam: its stable identity text, the exact format it serves, and the fact
/// roles its markup projection uses. The compiler and evaluator live in this crate's xpath and css modules; a codec
/// never sees either.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectorLanguage {
    /// `xml.xpath@1` — the XML codec's closed XPath 3.1 subset (§4.9).
    XmlXPath1,
    /// `html.css@1` — the HTML codec's static Selectors Level 4 profile (§4.10).
    HtmlCss1,
}

impl SelectorLanguage {
    /// The language's stable identity text.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::XmlXPath1 => "xml.xpath@1",
            Self::HtmlCss1 => "html.css@1",
        }
    }

    /// The format identity text this language serves.
    #[must_use]
    pub const fn format(self) -> &'static str {
        match self {
            Self::XmlXPath1 => "xml",
            Self::HtmlCss1 => "html",
        }
    }

    /// The fact roles the language's markup projection carries.
    #[allow(
        clippy::unused_self,
        reason = "one uniform per-language surface: every markup language answers fact_roles(), \
                  and a future language with distinct roles keeps the same call shape"
    )]
    pub(crate) const fn fact_roles(self) -> FactRoles {
        FactRoles::KERNEL
    }

    /// HTML document-mode fact role (`html.mode@1`), when the language consults it.
    pub(crate) const fn mode_role(self) -> Option<&'static str> {
        match self {
            Self::HtmlCss1 => Some("html.mode@1"),
            Self::XmlXPath1 => None,
        }
    }

    /// HTML pragma-language fact role (`html.pragma-language@1`), when the language consults it.
    pub(crate) const fn pragma_language_role(self) -> Option<&'static str> {
        match self {
            Self::HtmlCss1 => Some("html.pragma-language@1"),
            Self::XmlXPath1 => None,
        }
    }
}

/// The attached-fact roles one language's markup projection uses.
///
/// One kernel table: name, attribute-map, per-attribute, and textContent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FactRoles {
    /// The element name fact role (`name`).
    pub name: &'static str,
    /// The semantic attribute-map fact role.
    pub attrs: &'static str,
    /// The per-attribute fact role (the engine's `.&` contract).
    pub attribute: &'static str,
    /// The textContent fact role.
    pub content: &'static str,
}

impl FactRoles {
    const KERNEL: Self = Self {
        name: markup::NAME_FACT,
        attrs: markup::ATTRS_FACT,
        attribute: markup::ATTRIBUTE_FACT,
        content: markup::CONTENT_FACT,
    };
}
