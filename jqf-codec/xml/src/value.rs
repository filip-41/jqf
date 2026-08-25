//! XML document tree storage and expanded-name laws.
//!
//! This module owns the private element tree the parser builds and the
//! projection consumes. It is deliberately distinct from a generic CST:
//! per portfolio §4.9, XML keeps its own namespace stack and compact tree,
//! and the expanded-name rules are exact — an element or attribute identity
//! is a resolved `(namespace URI, local name)` pair.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

/// Interned name/URI identity. Distinct strings get distinct ids; equality
/// of [`ExpandedName`] is therefore two integer compares.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NameId(u32);

/// A crate-local name intern table: last-hit probe then a `BTreeMap` index.
#[derive(Clone, Debug)]
pub struct NameInterner {
    strings: Vec<String>,
    index: BTreeMap<String, NameId>,
    last: Option<NameId>,
}

impl Default for NameInterner {
    fn default() -> Self {
        Self::new()
    }
}

impl NameInterner {
    /// Empty string at id 0 so the no-namespace URI is a constant.
    #[must_use]
    pub fn new() -> Self {
        let mut index = BTreeMap::new();
        index.insert(String::new(), Self::EMPTY);
        Self {
            strings: alloc::vec![String::new()],
            index,
            last: None,
        }
    }

    /// Interns `text`, returning a stable id for this table.
    pub fn intern(&mut self, text: &str) -> NameId {
        if let Some(last) = self.last
            && self.get(last) == text
        {
            return last;
        }
        if let Some(&id) = self.index.get(text) {
            self.last = Some(id);
            return id;
        }
        let id = NameId(self.strings.len() as u32);
        self.strings.push(String::from(text));
        self.index.insert(String::from(text), id);
        self.last = Some(id);
        id
    }

    /// The interned text for `id`.
    #[must_use]
    pub fn get(&self, id: NameId) -> &str {
        &self.strings[id.0 as usize]
    }

    /// Empty-string id (no-namespace URI, empty prefix).
    pub const EMPTY: NameId = NameId(0);
}

/// A resolved expanded name: an `(optional namespace URI, local name)` pair.
///
/// The empty-string URI is the no-namespace (`""`) case. Prefix spelling is
/// not part of the expanded name; the deterministic serializer re-binds
/// gathered URIs canonically, so the source spelling is never retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpandedName {
    /// The interned namespace URI, or [`NameInterner::EMPTY`] for no namespace.
    pub uri: NameId,
    /// The interned local name.
    pub local: NameId,
}

impl ExpandedName {
    /// The Clark-notation spelling `{uri}local` for namespaced names, or just
    /// `local` for the no-namespace case.
    #[must_use]
    pub fn clark(self, intern: &NameInterner) -> String {
        let uri = intern.get(self.uri);
        let local = intern.get(self.local);
        if uri.is_empty() {
            String::from(local)
        } else {
            let mut out = String::with_capacity(uri.len() + local.len() + 2);
            out.push('{');
            out.push_str(uri);
            out.push('}');
            out.push_str(local);
            out
        }
    }

    /// Clark-notation equality without allocating: `{uri}local` or `local`.
    #[must_use]
    pub fn clark_eq(self, intern: &NameInterner, key: &[u8]) -> bool {
        let uri = intern.get(self.uri).as_bytes();
        let local = intern.get(self.local).as_bytes();
        if uri.is_empty() {
            return local == key;
        }
        key.len() == uri.len() + local.len() + 2
            && key[0] == b'{'
            && key[uri.len() + 1] == b'}'
            && key[1..=uri.len()] == *uri
            && key[uri.len() + 2..] == *local
    }
}

/// The kind of one character-data event inside an element's content.
///
/// The PI payload is boxed so the enum is 32 bytes (a Text/Comment
/// payload) rather than 56.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentEvent {
    /// Ordinary character data (text or CDATA already unescaped).
    Text(String),
    /// A child element, named by its index into [`Tree::elements`].
    Element(usize),
    /// A comment's text (between `<!--` and `-->`).
    Comment(String),
    /// A processing instruction `<?target data?>`.
    ProcessingInstruction(Box<(String, String)>),
}

/// One element node in the tree.
#[derive(Clone, Debug)]
pub struct Element {
    /// The element's resolved expanded name.
    pub name: ExpandedName,
    /// Resolved attributes, in source order, as `(expanded name, value)`.
    /// Namespace declarations are NOT included here (they are namespace
    /// state, not ordinary attributes).
    pub attributes: Vec<(ExpandedName, String)>,
    /// Ordered content events (character data, children, comments, PIs).
    pub content: Vec<ContentEvent>,
    /// The element's authored extent: `start` is its start tag's first byte
    /// (`<`), `end` is one past its end tag's closing `>` (or past a
    /// self-closing `/>`). The edit lane's structural splices read it to
    /// find the end tag. Zero before the element closes.
    pub start: usize,
    /// See [`Element::start`].
    pub end: usize,
    /// One authored value span per ORDINARY attribute, aligned 1:1 with
    /// [`Element::attributes`]. Each span names the attribute's authored
    /// bytes INCLUDING its quotes (`"value"`), so the edit lane can
    /// re-escape position-correctly (an attribute value escapes differently
    /// from a text node).
    pub attribute_spans: Vec<(usize, usize)>,
    /// One authored byte span per CONTENT event position, aligned 1:1 with
    /// [`Element::content`]: `Some` for a text event (from its first
    /// fragment's first byte through its last fragment's last byte — interior
    /// entity references and CDATA markup included, because they ARE the
    /// authored bytes whose decode is the text), `None` for element/comment/
    /// processing-instruction events. A text run coalesced across a CDATA
    /// section extends the LAST span's end.
    pub content_spans: Vec<Option<(usize, usize)>>,
}

/// The complete parsed document tree.
#[derive(Debug, Default)]
pub struct Tree {
    /// Interned element, attribute, and namespace-URI strings.
    pub intern: NameInterner,
    /// All element nodes in document order (a child's index is a parent
    /// content-event reference).
    pub elements: Vec<Element>,
    /// The index of the document element.
    pub root: usize,
    /// Whether the source carried a DOCTYPE (the deterministic encoder's
    /// preflight rejects doctype-bearing documents).
    pub had_doctype: bool,
}

impl Tree {
    /// The local name text for an interned expanded name.
    #[cfg(test)]
    #[must_use]
    pub fn local_of(&self, name: ExpandedName) -> &str {
        self.intern.get(name.local)
    }

    /// The namespace URI text for an interned expanded name.
    #[cfg(test)]
    #[must_use]
    pub fn uri_of(&self, name: ExpandedName) -> &str {
        self.intern.get(name.uri)
    }
}

/// Renders a processing instruction the way `xml.jqf-deterministic@1`
/// emits it: `<?target?>` when the data is empty, `<?target data?>` otherwise.
pub(crate) fn pi_spelling(target: &str, data: &str) -> String {
    if data.is_empty() {
        alloc::format!("<?{target}?>")
    } else {
        alloc::format!("<?{target} {data}?>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_event_is_32_bytes() {
        assert_eq!(
            core::mem::size_of::<ContentEvent>(),
            32,
            "boxing the PI payload must keep ContentEvent at 32 bytes"
        );
    }

    #[test]
    fn clark_eq_matches_clark_spelling() {
        let mut intern = NameInterner::new();
        let local = ExpandedName {
            uri: NameInterner::EMPTY,
            local: intern.intern("item"),
        };
        assert!(local.clark_eq(&intern, b"item"));
        assert!(!local.clark_eq(&intern, b"other"));
        let namespaced = ExpandedName {
            uri: intern.intern("http://n/"),
            local: intern.intern("x"),
        };
        assert!(namespaced.clark_eq(&intern, b"{http://n/}x"));
        assert_eq!(namespaced.clark(&intern).as_bytes(), b"{http://n/}x");
    }
}
