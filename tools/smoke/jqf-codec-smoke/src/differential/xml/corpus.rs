//! The XML differential corpus: well-formedness and document-policy cases
//! from the codec's own smoke battery (mixed content, entities, namespaces,
//! prolog, the reject corpus), each paired with the verdict both jqf and
//! `quick-xml` must agree on.
//!
//! The comparison layer is ACCEPT/REJECT over the shared
//! well-formedness core: malformed markup (mismatched tags, bad names,
//! malformed attributes, unclosed CDATA/comments) is rejected by both. The
//! two parsers then part by DESIGN on the document policies jqf enforces and
//! a pull parser does not — single root, closed root at EOF, declared
//! prefixes, entity resolution, duplicate attributes — each of which is its
//! own declared-split row. A disagreement that is not on the table is a
//! defect.

/// What both decoders are expected to agree on for one corpus case.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Expect {
    /// Both decoders must accept.
    Accept,
    /// Both decoders must reject (error kinds need not match).
    Reject,
}

/// One named differential case.
pub(crate) struct Case {
    pub(crate) category: &'static str,
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) expect: Expect,
}

fn case(category: &'static str, name: impl Into<String>, bytes: Vec<u8>, expect: Expect) -> Case {
    Case {
        category,
        name: name.into(),
        bytes,
        expect,
    }
}

/// Builds the complete XML corpus.
pub(crate) fn build() -> Vec<Case> {
    let mut cases = Vec::new();
    cases.extend(fixtures());
    cases.extend(rejects());
    cases.extend(declared_splits());
    cases
}

// --- fixtures (the shared well-formedness core) ------------------------------

fn fixtures() -> Vec<Case> {
    vec![
        case("fixture", "fixture/empty-element", b"<a/>".to_vec(), Expect::Accept),
        case("fixture", "fixture/self-closing", b"<a></a>".to_vec(), Expect::Accept),
        case(
            "fixture",
            "fixture/mixed-content",
            b"<a>b<x/>e</a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/nested",
            b"<a><b><c>v</c></b></a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/comments",
            b"<a>x<!--cmt-->y</a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/processing-instruction",
            b"<a><?pi data?></a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/cdata",
            b"<a><![CDATA[raw <>&]]></a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/predefined-entities",
            b"<a>&lt;&amp;A&#65;</a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/internal-entity",
            b"<!DOCTYPE r [<!ENTITY co \"Codec\">]><r>&co;</r>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/declared-namespace",
            b"<p xmlns:n=\"urn:x\"><n:e>v</n:e></p>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/predeclared-xml-prefix",
            b"<a xml:lang=\"en\">v</a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/xml-declaration",
            b"<?xml version=\"1.0\"?><a>v</a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/xml-declaration-utf8",
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><a>v</a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/attributes",
            b"<a b=\"1\" c='2' d=\"x&amp;y\"/>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/entity-nesting",
            b"<!DOCTYPE r [<!ENTITY a \"1\"><!ENTITY b \"&a;2\">]><r>&b;</r>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/unicode-text",
            "<?xml version=\"1.0\"?><a>héllo ☃</a>".as_bytes().to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/default-namespace",
            b"<a xmlns=\"urn:x\"><b>v</b></a>".to_vec(),
            Expect::Accept,
        ),
        case(
            "fixture",
            "fixture/pi-with-data",
            // `<?pi bad?>` is WELL-FORMED: `?>` terminates, so the data is
            // `bad` — both parsers accept.
            b"<a><?pi bad?></a>".to_vec(),
            Expect::Accept,
        ),
    ]
}

// --- rejects (both decoders must reject) -------------------------------------

fn rejects() -> Vec<Case> {
    vec![
        case(
            "reject",
            "reject/mismatched-tags",
            b"<a><b></a>".to_vec(),
            Expect::Reject,
        ),
        case(
            "reject",
            "reject/unclosed-cdata",
            b"<a><![CDATA[raw</a>".to_vec(),
            Expect::Reject,
        ),
        case(
            "reject",
            "reject/unclosed-comment",
            b"<a><!--x</a>".to_vec(),
            Expect::Reject,
        ),
        case(
            "reject",
            "reject/malformed-comment",
            b"<a><!--></a>".to_vec(),
            Expect::Reject,
        ),
        case("reject", "reject/mismatched-close", b"<a></b>".to_vec(), Expect::Reject),
    ]
}

// --- declared splits (the divergence register's XML rows) ---------------------
//
// Each case is EXPECTED to disagree, and the disagreement is the point of the
// row: it proves the register against a real incumbent. The reason is written
// here and reprinted by main.rs when the row fires. A row whose case STOPPED
// disagreeing fails the run (the stale-entry rule).

fn declared_splits() -> Vec<Case> {
    vec![
        case(
            "declared",
            "declared/second-root",
            // The whole document is ONE document: jqf rejects a second root
            // element; quick-xml is a pull parser and reads events freely.
            b"<a/><b/>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/empty-document",
            // jqf requires a document element; quick-xml's reader reaches EOF
            // without error.
            b"".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/unclosed-root-at-eof",
            // jqf requires the document element to close; quick-xml reads the
            // inner close and does not check that the root closed at EOF.
            b"<a><b></b>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/undeclared-prefix-start",
            // Namespaces in XML: jqf rejects an undeclared prefix; quick-xml
            // without ns_resolution treats it as an ordinary QName.
            b"<a><a:bad/></a>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/undeclared-prefix-root",
            b"<a:bad/>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/bad-name",
            // jqf enforces XML name validity (`<1a/>` is not a Name); a pull
            // parser without check_element_names accepts any tag text.
            b"<1a/>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/unquoted-attribute",
            // jqf requires attribute values to be quoted; quick-xml's default
            // reader accepts an unquoted value.
            b"<a b=1/>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/attributes-no-space",
            // jqf requires whitespace between attributes per the XML
            // grammar; quick-xml's default reader accepts `<a b="1"c="2"/>`.
            b"<a b=\"1\"c=\"2\"/>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/raw-lt-in-attribute",
            // jqf rejects a raw `<` inside an attribute value; quick-xml does
            // not scan attribute content for it by default.
            b"<a b=\"<\"/>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/unbound-entity",
            // jqf resolves and REQUIRES entity references to be bound; the
            // pull parser leaves `&b;` in the text.
            b"<!DOCTYPE r [<!ENTITY a \"1\">]><r>&b;</r>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/duplicate-attribute",
            // jqf rejects a duplicated expanded attribute name; a pull parser
            // does not compare attributes across events.
            b"<a xmlns:x=\"x\" x:b=\"1\" x:b=\"2\"/>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/utf16-encoding-declaration",
            // jqf's encoding declaration is a grammar step of the selected
            // format; quick-xml ignores it and reads the bytes as given.
            b"<?xml version=\"1.0\" encoding=\"UTF-16\"?><a>v</a>".to_vec(),
            Expect::Reject,
        ),
        case(
            "declared",
            "declared/external-entity-declaration",
            // jqf disables external entities; quick-xml never resolves them.
            b"<!DOCTYPE r [<!ENTITY ext SYSTEM \"http://x/y\">]><r>&ext;</r>".to_vec(),
            Expect::Reject,
        ),
    ]
}
