//! Kernel markup accessor and leaf vocabulary.
//!
//! Format-neutral identities the selector, engine, and markup codecs share. Interned codec kind strings
//! (`xml.element@1`, `html.element@1`, …) stay codec-owned; core publishes only these kernel segments. A format's
//! schema, wire identity, or indent/escape grammar does not belong here.

/// The markup-attribute fact role shared by every markup format: one fact PER ATTRIBUTE, with the attribute's expanded
/// name as the fact kind, so the engine's `.&name` accessor serves each attribute directly.
pub const ATTRIBUTE_FACT: &str = "attribute";

/// The element-name fact role. Presence of this fact is the selector's element test; codecs intern this segment, not a
/// format-prefixed name.
pub const NAME_FACT: &str = "name";

/// The text-leaf node-kind segment. Selector tests match this segment value itself (`"text"`) — never a
/// format-prefixed identity such as `xml.text@1`; `XPath` `text()` and CSS text nodes resolve through it.
pub const TEXT_KIND: &str = "text";

/// The comment-leaf node-kind segment. Selector tests match this segment value itself (`"comment"`); `XPath`
/// `comment()` resolves through it. Comment *facts* stay format-prefixed (`xml.comment@1`) so the cross-format comment
/// overlay can name a codec's own fact role.
pub const COMMENT_KIND: &str = "comment";

/// The processing-instruction leaf node-kind segment. Selector tests match this segment value itself (`"pi"`); `XPath`
/// `processing-instruction()` resolves through it. HTML never interns it.
pub const PI_KIND: &str = "pi";

/// The textContent fact role. Selector and engine name this without importing a format crate.
pub const CONTENT_FACT: &str = "content";

/// The semantic attribute-map fact role. Selector and engine name this without importing a format crate.
pub const ATTRS_FACT: &str = "attrs";

/// The `.@name` accessor hint for a missed member step that equals the element's own name on a NESTED element: the
/// accessor is the whole answer.
pub const OWN_NAME_MISS_HINT: &str = "this is the element's own name — read it with the .@name accessor";

/// The root-element miss hint: on the DOCUMENT element the miss is the projection seam (the shown JSON wraps the root
/// under its own key), so the hint names the real navigation — children by name — plus the accessor that reads the
/// name itself.
#[must_use]
pub fn root_element_miss_hint(key: &str) -> alloc::string::String {
    alloc::format!(
        "the root element IS the key: it has no child named {key} — navigate children by \
         name, or read the element's own name with the .@name accessor"
    )
}

/// The `.&name` accessor hint for a missed member step that equals one of the element's attributes: the value is an
/// attribute fact.
#[must_use]
pub fn attribute_miss_hint(key: &str) -> alloc::string::String {
    alloc::format!("{key} is an attribute of this element — read it with the .&{key} accessor")
}
