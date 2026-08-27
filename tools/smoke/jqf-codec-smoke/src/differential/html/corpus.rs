//! The HTML demand-ladder corpus: recovered documents crossed with the exact
//! paths the four demand routes are asked to serve.
//!
//! The documents are chosen for the shapes that are UNIQUE to markup and were
//! unexecuted before this lane existed: a singular named child, a PLURAL named
//! child (two or more same-named siblings — the shape the located route
//! collapses into a range), text leaves beside elements, a name that matches
//! nothing, and the WHATWG recovery shapes (implied tags, foster parenting)
//! that make the recovered tree differ from the source markup.

/// One named corpus document.
pub(crate) struct Case {
    pub(crate) name: &'static str,
    pub(crate) bytes: &'static [u8],
}

/// One exact-path step, mirroring [`jqf_engine::StaticForwardStep`]'s three
/// static forms.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Step {
    /// A member step navigating an element's children by element NAME.
    Member(&'static str),
    /// A signed array position over an element's children.
    Index(i64),
    /// A contiguous signed range over an element's children.
    Range(Option<i64>, Option<i64>),
}

/// One named exact path, run against every corpus document on every route.
pub(crate) struct Path {
    pub(crate) name: &'static str,
    pub(crate) steps: &'static [Step],
}

pub(crate) const CASES: &[Case] = &[
    Case {
        name: "fixture/singular",
        bytes: b"<html><head><title>t</title></head><body><p>only</p></body></html>",
    },
    Case {
        name: "fixture/plural",
        bytes: b"<body><p>a</p><p>b</p><p>c</p></body>",
    },
    Case {
        name: "fixture/mixed-content",
        bytes: b"<body>lead<p>a</p>tail<p>b</p>end</body>",
    },
    Case {
        name: "fixture/nested",
        bytes: b"<body><div><span>x</span><span>y</span></div></body>",
    },
    Case {
        name: "fixture/attributes-and-comments",
        bytes: b"<body id=\"b\"><!--c--><p class=\"k\">t</p></body>",
    },
    Case {
        name: "fixture/implied-tags",
        bytes: b"<p>a<p>b",
    },
    Case {
        name: "fixture/foster-parented-table",
        bytes: b"<table><tr><td>1</td></tr>stray<tr><td>2</td></tr></table>",
    },
    Case {
        name: "fixture/empty-document",
        bytes: b"<html></html>",
    },
    Case {
        name: "fixture/text-only",
        bytes: b"hello",
    },
    Case {
        name: "fixture/entities",
        bytes: b"<body><p>&amp;&#65;</p><p>&lt;</p></body>",
    },
    Case {
        name: "fixture/comment-only",
        bytes: b"<!--x-->",
    },
    Case {
        name: "fixture/nested-plural-deep",
        bytes: b"<body><ul><li>1</li><li>2</li><li>3</li></ul><ul><li>4</li></ul></body>",
    },
];

pub(crate) const PATHS: &[Path] = &[
    Path {
        name: "path/root",
        steps: &[],
    },
    Path {
        name: "path/member-singular",
        steps: &[Step::Member("body")],
    },
    Path {
        name: "path/member-plural",
        steps: &[Step::Member("body"), Step::Member("p")],
    },
    Path {
        name: "path/member-missing",
        steps: &[Step::Member("body"), Step::Member("nope")],
    },
    Path {
        name: "path/member-own-name",
        steps: &[Step::Member("html")],
    },
    Path {
        name: "path/index",
        steps: &[Step::Member("body"), Step::Index(0)],
    },
    Path {
        name: "path/index-negative",
        steps: &[Step::Member("body"), Step::Index(-1)],
    },
    Path {
        name: "path/index-out-of-range",
        steps: &[Step::Member("body"), Step::Index(99)],
    },
    Path {
        name: "path/index-over-plural",
        steps: &[Step::Member("body"), Step::Member("p"), Step::Index(0)],
    },
    Path {
        name: "path/type-mismatch-into-leaf",
        steps: &[Step::Member("body"), Step::Index(0), Step::Member("x")],
    },
    Path {
        name: "path/range-over-singular",
        steps: &[Step::Member("body"), Step::Range(Some(0), Some(1))],
    },
    Path {
        name: "path/range-over-plural",
        steps: &[Step::Member("body"), Step::Member("p"), Step::Range(Some(0), Some(2))],
    },
    Path {
        name: "path/range-open-over-plural",
        steps: &[Step::Member("body"), Step::Member("p"), Step::Range(None, None)],
    },
    Path {
        name: "path/range-negative-over-plural",
        steps: &[Step::Member("body"), Step::Member("p"), Step::Range(Some(-2), None)],
    },
    Path {
        name: "path/range-empty",
        steps: &[Step::Member("body"), Step::Member("p"), Step::Range(Some(2), Some(1))],
    },
    Path {
        name: "path/deep-plural",
        steps: &[Step::Member("body"), Step::Member("ul"), Step::Member("li")],
    },
];
