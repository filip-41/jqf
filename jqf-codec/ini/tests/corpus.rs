//! The flat-config grammar corpus and span receipts.
//!
//! One case per written grammar clause, each naming the clause it pins. Every case drives the full decode path —
//! registration → provider → session → document — so the corpus pins scan, materialize, the last-wins law, and the
//! comment facts together. The span receipts assert that every value node's span slices the source to its own authored
//! bytes.

use jqf_codec_core::{
    CodecDemand, CodecRunContext, DecodeRequest, DemandClause, DiagnosticPolicy, FactIntent, ValidationMode,
};
use jqf_data::{DialectId, FactPayloadView, FormatId, ObjectBuilder, ObjectKey, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// One corpus case: the clause it pins, the source bytes, and the expected root object (as an ordered list of (key,
/// value) members).
struct Case {
    /// The grammar clause this case pins.
    clause: &'static str,
    bytes: &'static [u8],
    /// Expected (key, value) members in first-insertion order.
    members: &'static [(&'static str, &'static str)],
    /// Expected leading-comment texts on the LAST member's value node.
    comments: &'static [&'static str],
}

/// The `properties.jdk@1` grammar corpus.
const PROPERTIES_CASES: &[Case] = &[
    Case {
        clause: "a logical line is terminated by \\n, \\r\\n, or \\r",
        bytes: b"a=1\r\nb=2\rc=3\nd=4\n",
        members: &[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")],
        comments: &[],
    },
    Case {
        clause: "a logical line holding only whitespace is ignored",
        bytes: b"   \n\t\x0c\na=1\n",
        members: &[("a", "1")],
        comments: &[],
    },
    Case {
        clause: "a comment line has # as its first non-whitespace character",
        bytes: b"# hello\na=1\n",
        members: &[("a", "1")],
        comments: &["hello"],
    },
    Case {
        clause: "a comment line has ! as its first non-whitespace character",
        bytes: b"! note\na=1\n",
        members: &[("a", "1")],
        comments: &["note"],
    },
    Case {
        clause: "a comment line may be indented",
        bytes: b"  # indented\na=1\n",
        members: &[("a", "1")],
        comments: &["indented"],
    },
    Case {
        clause: "a trailing backslash on a # comment is literal, not a continuation",
        bytes: b"# comment \\\nkey=value\n",
        members: &[("key", "value")],
        comments: &["comment \\"],
    },
    Case {
        clause: "a trailing backslash on a ! comment is literal, not a continuation",
        bytes: b"! comment \\\nkey=value\n",
        members: &[("key", "value")],
        comments: &["comment \\"],
    },
    Case {
        clause: "a backslash immediately before a line terminator continues the logical line",
        bytes: b"a=b\\\nc\n",
        members: &[("a", "bc")],
        comments: &[],
    },
    Case {
        clause: "a line continuation in the key joins the next natural line",
        bytes: b"a\\\nb=c\n",
        members: &[("ab", "c")],
        comments: &[],
    },
    Case {
        clause: "an even trailing-backslash run ends the logical line (the pair cooks to one literal backslash)",
        bytes: b"a=b\\\\\nc=d\n",
        members: &[("a", "b\\"), ("c", "d")],
        comments: &[],
    },
    Case {
        clause: "the key ends at the first unescaped '=' separator",
        bytes: b"key=value\n",
        members: &[("key", "value")],
        comments: &[],
    },
    Case {
        clause: "the key ends at the first unescaped ':' separator",
        bytes: b"key:value\n",
        members: &[("key", "value")],
        comments: &[],
    },
    Case {
        clause: "a whitespace separator terminates the key",
        bytes: b"key value\nkey2\tvalue2\n",
        members: &[("key", "value"), ("key2", "value2")],
        comments: &[],
    },
    Case {
        clause: "an escaped '=' is a key character, not a separator",
        bytes: b"a\\=b=1\n",
        members: &[("a=b", "1")],
        comments: &[],
    },
    Case {
        clause: "an escaped space is a key character",
        bytes: b"a\\ b=1\n",
        members: &[("a b", "1")],
        comments: &[],
    },
    Case {
        clause: "whitespace after the key is skipped and a following separator consumed",
        bytes: b"key   =   value\n",
        members: &[("key", "value")],
        comments: &[],
    },
    Case {
        clause: "a \\uXXXX escape is cooked in the value",
        bytes: b"a=\\u0041\\u00e9\n",
        members: &[("a", "A\u{e9}")],
        comments: &[],
    },
    Case {
        clause: "\\t \\n \\r \\f escapes cook to their control characters",
        bytes: b"a=b\\tc\\nd\\re\\ff\n",
        members: &[("a", "b\tc\nd\re\u{000c}f")],
        comments: &[],
    },
    Case {
        clause: "\\\\ is a literal backslash",
        bytes: b"a=b\\\\c\n",
        members: &[("a", "b\\c")],
        comments: &[],
    },
    Case {
        clause: "a surrogate pair cooks to the supplementary character",
        bytes: b"a=\\uD83D\\uDE00\n",
        members: &[("a", "\u{1f600}")],
        comments: &[],
    },
    Case {
        clause: "a lone low surrogate decodes to U+FFFD",
        bytes: b"a=\\uDC00\n",
        members: &[("a", "\u{fffd}")],
        comments: &[],
    },
    Case {
        clause: "leading whitespace before the key is skipped",
        bytes: b"  key=value\n",
        members: &[("key", "value")],
        comments: &[],
    },
    Case {
        clause: "a line with no key yields an empty key",
        bytes: b"=value\n",
        members: &[("", "value")],
        comments: &[],
    },
    Case {
        clause: "a line with a key and nothing after the separator yields an empty value",
        bytes: b"key=\n",
        members: &[("key", "")],
        comments: &[],
    },
    Case {
        clause: "a key with no separator at all yields an empty value",
        bytes: b"key\n",
        members: &[("key", "")],
        comments: &[],
    },
    Case {
        clause: "trailing raw whitespace is value text",
        bytes: b"a=b   \n",
        members: &[("a", "b   ")],
        comments: &[],
    },
    Case {
        clause: "a backslash before a final blank cooks to that blank",
        bytes: b"a=v\\ \n",
        members: &[("a", "v ")],
        comments: &[],
    },
    Case {
        clause: "an escaped '#' in a value is value text, not a comment",
        bytes: b"a=b\\#c\n",
        members: &[("a", "b#c")],
        comments: &[],
    },
    Case {
        clause: "duplicate keys are last-value-wins; first insertion fixes position",
        bytes: b"a=1\nk=first\nb=2\nk=second\n",
        members: &[("a", "1"), ("k", "second"), ("b", "2")],
        comments: &[],
    },
    Case {
        clause: "leading comments attach to the following value node",
        bytes: b"# one\n# two\na=1\n",
        members: &[("a", "1")],
        comments: &["one", "two"],
    },
];

/// The `ini.jqf-strict@1` clause-list corpus.
const INI_CASES: &[Case] = &[
    Case {
        clause: "a key line before any section belongs to the root object",
        bytes: b"root = 1\nroot2 = 2\n",
        members: &[("root", "1"), ("root2", "2")],
        comments: &[],
    },
    Case {
        clause: "the value is the rest of the logical line, trimmed",
        bytes: b"a =  b  \n",
        members: &[("a", "b")],
        comments: &[],
    },
    Case {
        clause: "; at the first non-blank byte is a comment",
        bytes: b"; c\na = b\n",
        members: &[("a", "b")],
        comments: &["c"],
    },
    Case {
        clause: "an inline ';' after a value is value text, not a comment",
        bytes: b"a = b ; text\n",
        members: &[("a", "b ; text")],
        comments: &[],
    },
    Case {
        clause: "an inline '#' after a value is value text, not a comment",
        bytes: b"a = b # text\n",
        members: &[("a", "b # text")],
        comments: &[],
    },
    Case {
        clause: "duplicate keys are last-value-wins; first insertion fixes position",
        bytes: b"a=1\nk=first\nb=2\nk=second\n",
        members: &[("a", "1"), ("k", "second"), ("b", "2")],
        comments: &[],
    },
    Case {
        clause: "no quote processing: a quoted value is literal bytes",
        bytes: b"a = \"x\"\n",
        members: &[("a", "\"x\"")],
        comments: &[],
    },
    Case {
        clause: "a colon separator is accepted",
        bytes: b"a: b\n",
        members: &[("a", "b")],
        comments: &[],
    },
];

/// The `dotenv.jqf-strict@1` clause-list corpus.
const DOTENV_CASES: &[Case] = &[
    Case {
        clause: "single-quoted values are literal (no escapes)",
        bytes: b"a='b\\nc'\n",
        members: &[("a", "b\\nc")],
        comments: &[],
    },
    Case {
        clause: "double-quoted values take the standard escape set",
        bytes: b"a=\"b\\nc\\t\\\\\"\n",
        members: &[("a", "b\nc\t\\")],
        comments: &[],
    },
    Case {
        clause: "unquoted values are literal and no $VAR interpolation is performed",
        bytes: b"a=$HOME/x\n",
        members: &[("a", "$HOME/x")],
        comments: &[],
    },
    Case {
        clause: "double-quoted $VAR is literal (sibling of the unquoted row)",
        bytes: b"A=\"$HOME\"\n",
        members: &[("A", "$HOME")],
        comments: &[],
    },
    Case {
        clause: "a colon is a legal dotenv key byte",
        bytes: b"FOO:BAR=baz\n",
        members: &[("FOO:BAR", "baz")],
        comments: &[],
    },
    Case {
        clause: "duplicate keys are last-value-wins; first insertion fixes position",
        bytes: b"a=1\nk=first\nb=2\nk=second\n",
        members: &[("a", "1"), ("k", "second"), ("b", "2")],
        comments: &[],
    },
    Case {
        clause: "an export prefix is accepted and stripped",
        bytes: b"export A=1\n",
        members: &[("A", "1")],
        comments: &[],
    },
    Case {
        clause: "# at the first non-blank byte is a comment",
        bytes: b"# hi\na=1\n",
        members: &[("a", "1")],
        comments: &["hi"],
    },
];

/// Terminal-failure corpus: each case must fail the whole decode.
const TERMINAL_CASES: &[(&str, &str, &[u8])] = &[
    (
        "properties",
        "a malformed \\uxxxx escape is a terminal failure",
        b"a=\\u00\n",
    ),
    (
        "properties",
        "a high surrogate not followed by a valid low escape raises",
        b"a=\\uD800x\n",
    ),
    ("properties", "non-UTF-8 bytes fail terminally", b"a=\xff\n"),
    ("properties", "a byte-order mark is refused", b"\xef\xbb\xbfa=b\n"),
    ("ini", "a bare key with no separator is a terminal failure", b"bare\n"),
    ("ini", "an unterminated '[' is a terminal failure", b"[abc\n"),
    (
        "ini",
        "a duplicate section header is a terminal failure",
        b"[a]\nx=1\n[a]\n",
    ),
    (
        "ini",
        "a root key colliding with a section name is a terminal failure",
        b"x=1\n[x]\n",
    ),
    (
        "ini",
        "a section header with trailing content is a terminal failure",
        b"[a] x=1\n",
    ),
    ("ini", "an empty section header is a terminal failure", b"[]\n"),
    ("dotenv", "a line without '=' is a terminal failure", b"bare\n"),
    (
        "dotenv",
        "an unterminated quote is a terminal failure",
        b"a='unterminated\n",
    ),
    (
        "dotenv",
        "trailing content after a quoted value is a terminal failure",
        b"a='x' junk\n",
    ),
    (
        "dotenv",
        "an unterminated double quote is a terminal failure",
        b"a=\"unterminated\n",
    ),
    (
        "dotenv",
        "trailing content after a double-quoted value is a terminal failure",
        b"a=\"x\" junk\n",
    ),
];

fn resources() -> ResourceContext<'static> {
    static CONTROL: ContinueControl = ContinueControl;
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
            .expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(94), SourceKind::Input),
        "corpus.input",
        bytes,
        0,
    )
}

/// Runs one closure against the whole decoded document, keeping the provider/session/product alive for the closure's
/// borrow.
fn with_document<R>(
    format: &str,
    dialect: &str,
    bytes: &[u8],
    f: impl FnOnce(&jqf_data::Document<'_>) -> Result<R, String>,
) -> Result<R, String> {
    let mut resources = resources();
    let registration = match format {
        "properties" => jqf_codec_ini::registration().map_err(|e| format!("{e:?}"))?,
        "ini" => jqf_codec_ini::registration_ini().map_err(|e| format!("{e:?}"))?,
        "dotenv" => jqf_codec_ini::registration_dotenv().map_err(|e| format!("{e:?}"))?,
        _ => panic!("unknown format"),
    };
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(dialect).map_err(|e| e.to_string())?,
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let requirement = preserve_requirement(&resources);
    let handle = provider.bind(&requirement).map_err(|e| format!("{e:?}"))?;
    let mut session = provider.open(&handle, &mut resources).map_err(|e| format!("{e:?}"))?;
    {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session
            .decode(&mut run)
            .map_err(|e| format!("decode: {:?}", e.kind()))?;
        match result.outcome() {
            jqf_codec_core::AccessOutcome::FullDocument(product) => f(product.document()),
            jqf_codec_core::AccessOutcome::Located { .. } => Err("unexpected outcome".into()),
        }
    }
}

/// Decodes one fixture and hands the closure the FULL DOCUMENT PRODUCT — the shape the located encode path consumes
/// (`EncodeItem::Located`), which is the path that re-emits comment facts.
fn with_product<R>(
    format: &str,
    dialect: &str,
    bytes: &[u8],
    f: impl FnOnce(&jqf_codec_core::DocumentProduct<'_>) -> Result<R, String>,
) -> Result<R, String> {
    let mut resources = resources();
    let registration = match format {
        "properties" => jqf_codec_ini::registration().map_err(|e| format!("{e:?}"))?,
        "ini" => jqf_codec_ini::registration_ini().map_err(|e| format!("{e:?}"))?,
        "dotenv" => jqf_codec_ini::registration_dotenv().map_err(|e| format!("{e:?}"))?,
        _ => panic!("unknown format"),
    };
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(dialect).map_err(|e| e.to_string())?,
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let requirement = preserve_requirement(&resources);
    let handle = provider.bind(&requirement).map_err(|e| format!("{e:?}"))?;
    let mut session = provider.open(&handle, &mut resources).map_err(|e| format!("{e:?}"))?;
    {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session
            .decode(&mut run)
            .map_err(|e| format!("decode: {:?}", e.kind()))?;
        match result.outcome() {
            jqf_codec_core::AccessOutcome::FullDocument(product) => f(product),
            jqf_codec_core::AccessOutcome::Located { .. } => Err("unexpected outcome".into()),
        }
    }
}

/// Encodes one LOCATED document through the registration's encoder factory — the real CLI path, which re-emits the
/// document's comment facts.
fn encode_document(
    format: &str,
    dialect: &str,
    product: &jqf_codec_core::DocumentProduct<'_>,
) -> Result<Vec<u8>, String> {
    let mut resources = resources();
    let registration = match format {
        "properties" => jqf_codec_ini::registration().map_err(|e| format!("{e:?}"))?,
        "ini" => jqf_codec_ini::registration_ini().map_err(|e| format!("{e:?}"))?,
        "dotenv" => jqf_codec_ini::registration_dotenv().map_err(|e| format!("{e:?}"))?,
        _ => panic!("unknown format"),
    };
    let format_id = FormatId::try_new(format).map_err(|e| e.to_string())?;
    let dialect_id = DialectId::try_new(dialect).map_err(|e| e.to_string())?;
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            jqf_codec_core::EncodeRequest {
                format: &format_id,
                dialect: &dialect_id,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: jqf_codec_core::PreservationRequest::None,
                options: None,
            },
            &mut resources,
        )
        .map_err(|e| format!("factory: {:?}", e.kind()))?;
    let mut session = factory
        .start(
            jqf_codec_core::EncodeItem::Located {
                product,
                node: product.document().root_handle(),
            },
            jqf_codec_core::PreservationRequest::None,
            &mut resources,
        )
        .map_err(|e| format!("session: {:?}", e.kind()))?;
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session
            .encode(&mut sink, &mut run)
            .map_err(|e| format!("encode: {:?}", e.kind()))?;
    }
    Ok(out)
}

fn whole_requirement(resources: &ResourceContext<'_>) -> jqf_codec_core::AccessRequirement {
    let mut demand = CodecDemand::try_new(resources);
    demand.try_insert(&DemandClause::SemanticRoot).expect("semantic root");
    demand.try_insert(&DemandClause::ValueShape).expect("value shape");
    jqf_codec_core::AccessRequirement::try_whole(
        demand,
        jqf_codec_core::AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

fn preserve_requirement(resources: &ResourceContext<'_>) -> jqf_codec_core::AccessRequirement {
    whole_requirement(resources).with_fact_intent(FactIntent::Preserve)
}

fn comment_clause_requirement(resources: &ResourceContext<'_>) -> jqf_codec_core::AccessRequirement {
    let mut demand = CodecDemand::try_new(resources);
    let kind = jqf_data::FactKindId::try_new("comment").expect("kind");
    let role = jqf_data::FactRoleId::try_new("comment").expect("role");
    demand
        .try_insert(&DemandClause::AttachedFact { kind, role })
        .expect("insert");
    jqf_codec_core::AccessRequirement::try_whole(
        demand,
        jqf_codec_core::AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .expect("requirement")
}

fn owned_root(document: &jqf_data::Document<'_>) -> Result<Value, String> {
    let mut resources = resources();
    document.materialize_root(&mut resources).map_err(|e| e.to_string())
}

fn assert_members(document: &jqf_data::Document<'_>, expected: &[(&str, &str)]) -> Result<(), String> {
    let root = owned_root(document)?;
    let Value::Object(object) = &root else {
        return Err("root is not an object".into());
    };
    if object.len() != expected.len() {
        return Err(format!("member count {} != {}", object.len(), expected.len()));
    }
    for (index, (key, value)) in expected.iter().enumerate() {
        let entry = object.get_index(index).ok_or("missing member")?;
        if entry.key() != *key {
            return Err(format!("member {index} key {:?} != {key:?}", entry.key()));
        }
        let Value::String(text) = entry.value() else {
            return Err(format!("member {key:?} is not a string"));
        };
        if text != value {
            return Err(format!("member {key:?} value {text:?} != {value:?}"));
        }
    }
    Ok(())
}

/// Reads the `*@1` comment fact texts on one value node.
fn comment_texts(document: &jqf_data::Document<'_>, node: jqf_data::NodeId) -> Vec<String> {
    let mut texts = Vec::new();
    for fact_id in document.owner_fact_ids(node) {
        let Ok(fact) = document.fact(*fact_id) else {
            continue;
        };
        if !fact.role().as_str().ends_with(".comment@1") {
            continue;
        }
        if let FactPayloadView::List(items) = fact.payload() {
            for item in items.iter() {
                if let FactPayloadView::Text(text) = item {
                    texts.push(text.to_string());
                }
            }
        }
    }
    texts
}

fn assert_case(case: &Case, format: &str, dialect: &str) -> Result<(), String> {
    with_document(format, dialect, case.bytes, |document| {
        assert_members(document, case.members)?;
        if !case.comments.is_empty() {
            let root_view = document
                .value_view(document.node_handle(document.root()).expect("root handle"))
                .expect("view");
            let root_object = root_view.object().expect("object").expect("root is an object");
            let last = root_object.len() - 1;
            let node = root_object
                .get_index(last)
                .expect("member")
                .expect("entry")
                .value()
                .node();
            let texts = comment_texts(document, node);
            if texts != case.comments {
                return Err(format!("comments {texts:?} != {:?} on the last member", case.comments));
            }
        }
        Ok(())
    })
}

#[test]
fn properties_grammar_corpus_is_green() {
    for case in PROPERTIES_CASES {
        assert_case(case, "properties", jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID)
            .unwrap_or_else(|error| panic!("{}: {error}", case.clause));
    }
}

#[test]
fn ini_grammar_corpus_is_green() {
    for case in INI_CASES {
        assert_case(case, "ini", jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID)
            .unwrap_or_else(|error| panic!("{}: {error}", case.clause));
    }
}

#[test]
fn dotenv_grammar_corpus_is_green() {
    for case in DOTENV_CASES {
        assert_case(case, "dotenv", jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID)
            .unwrap_or_else(|error| panic!("{}: {error}", case.clause));
    }
}

#[test]
fn terminal_failures_are_terminal() {
    for (format, clause, bytes) in TERMINAL_CASES {
        let dialect = match *format {
            "properties" => jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
            "ini" => jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
            "dotenv" => jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
            _ => unreachable!(),
        };
        let result = with_document(format, dialect, bytes, |_| Ok(()));
        assert!(result.is_err(), "{clause}: expected a terminal failure, got a document");
    }
}

#[test]
fn every_value_nodes_span_slices_the_source_to_its_authored_bytes() {
    // Full-path sibling of `spans_slice_the_authored_bytes` in scan.rs. Every VALUE node's span slices the source to
    // the value's own authored bytes — including cooked escapes, whose raw spelling the span must name.
    let bytes = b"alpha = one two\nbeta=three\\\nfour\ngamma=\\u0041\n";
    with_document(
        "properties",
        jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
        bytes,
        |document| {
            let root_view = document
                .value_view(document.node_handle(document.root()).expect("root handle"))
                .expect("view");
            let root_object = root_view.object().expect("object").expect("root is an object");
            let expected_spans: &[(&str, &[u8])] =
                &[("alpha", b"one two"), ("beta", b"three\\\nfour"), ("gamma", b"\\u0041")];
            for (index, (key, authored)) in expected_spans.iter().enumerate() {
                let entry = root_object.get_index(index).expect("member").expect("entry");
                assert_eq!(entry.key(), *key);
                let node = entry.value().node();
                let span = document.node_source_span(node).expect("span lookup").expect("a span");
                let slice = &bytes[span.start() as usize..span.end() as usize];
                assert_eq!(
                    slice, *authored,
                    "the value node for {key:?} must carry its own authored bytes"
                );
            }
            Ok(())
        },
    )
    .expect("decode");
}

#[test]
fn the_ini_section_object_is_one_nesting_level() {
    // A `[section]` IS an object in the grammar, projected as one; root keys stay at the root.
    let bytes = b"root = 1\n[db]\nhost = localhost\n";
    with_document("ini", jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID, bytes, |document| {
        let root = owned_root(document)?;
        let Value::Object(object) = &root else {
            return Err("root is not an object".into());
        };
        assert_eq!(object.len(), 2);
        // Member order is canonical: sections first, then root scalars.
        assert_eq!(object.get_index(0).expect("db").key(), "db");
        assert_eq!(object.get_index(1).expect("root").key(), "root");
        let Some(Value::String(root_value)) = object.get("root") else {
            return Err("root is not a string".into());
        };
        assert_eq!(root_value.as_str(), "1");
        let Value::Object(section) = object.get_index(0).ok_or("no db member")?.value() else {
            return Err("db is not an object".into());
        };
        let host = section.get_index(0).ok_or("no host")?;
        assert_eq!(host.key(), "host");
        let Value::String(host_value) = host.value() else {
            return Err("host is not a string".into());
        };
        assert_eq!(host_value.as_str(), "localhost");
        Ok(())
    })
    .expect("decode");
}

/// Structural equality for the fixpoint: two objects of strings (with the one INI section level) are equal member-wise.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.as_str() == y.as_str(),
        (Value::Object(x), Value::Object(y)) => {
            if x.len() != y.len() {
                return false;
            }
            for entry in x {
                match y.get(entry.key()) {
                    Some(other) => {
                        if !values_equal(entry.value(), other) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
            true
        }
        _ => false,
    }
}

/// The decode∘encode fixpoint: a normalized document re-encodes to bytes that decode to the SAME document. The encode
/// runs through the LOCATED path, so the comment facts re-emit as `# text` lines and the fixpoint carries them.
#[test]
fn decode_encode_fixpoint_on_normalized_documents() {
    // A properties fixture whose every value round-trips through the escape grammar — including trailing-whitespace
    // values: plain trailing blanks and a backslash-escaped final blank.
    let properties_fixture =
        b"# a comment\nname = ada\nid=42\nexact = 1.50\nflag=true\nescaped = a\\tb\\nc\ntail=b  \ncook=v\\ \n";
    with_product(
        "properties",
        jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
        properties_fixture,
        |product| {
            let document = product.document();
            let root = owned_root(document)?;
            let bytes = encode_document("properties", jqf_codec_ini::PROPERTIES_JQF_1_0_DIALECT_ID, product)?;
            // The canonical encode re-emits VALUES plus the comment facts as `# text` lines; the leading comment above
            // `name` lands before its line. A trailing blank writes RAW (the decoder preserves it) and the escaped
            // blank re-writes raw too — both spellings decode back to the same value.
            assert_eq!(
                String::from_utf8_lossy(&bytes),
                "# a comment\nname=ada\nid=42\nexact=1.50\nflag=true\nescaped=a\\tb\\nc\ntail=b  \ncook=v \n"
            );
            with_document(
                "properties",
                jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
                &bytes,
                |re_decoded| {
                    let again = owned_root(re_decoded)?;
                    if !values_equal(&root, &again) {
                        return Err(format!("the fixpoint must hold:\nfirst:  {root:?}\nsecond: {again:?}"));
                    }
                    Ok(())
                },
            )
        },
    )
    .expect("properties fixpoint");

    // The INI fixpoint carries sections through `[section]` headers.
    let ini_fixture = b"root = 1\n[db]\nhost = localhost\nport = 5432\n";
    with_product(
        "ini",
        jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
        ini_fixture,
        |product| {
            let document = product.document();
            let root = owned_root(document)?;
            let bytes = encode_document("ini", jqf_codec_ini::INI_JQF_1_0_DIALECT_ID, product)?;
            assert_eq!(
                String::from_utf8_lossy(&bytes),
                "root=1\n[db]\nhost=localhost\nport=5432\n"
            );
            with_document("ini", jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID, &bytes, |re_decoded| {
                let again = owned_root(re_decoded)?;
                if !values_equal(&root, &again) {
                    return Err(format!("the fixpoint must hold:\nfirst:  {root:?}\nsecond: {again:?}"));
                }
                Ok(())
            })
        },
    )
    .expect("ini fixpoint");

    // The dotenv fixpoint: quoted values decode to their cooked text and re-encode with quotes when the unquoted
    // grammar cannot carry them. The leading-apostrophe value is one of those: unquoted it would decode as an
    // unterminated single-quoted literal, so the encoder wraps it in a double-quote pair.
    let dotenv_fixture = b"a='b\\nc'\nb=\"x\\t y\"\nc=$HOME\nd=\"'x\"\n";
    with_product(
        "dotenv",
        jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
        dotenv_fixture,
        |product| {
            let document = product.document();
            let root = owned_root(document)?;
            let bytes = encode_document("dotenv", jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID, product)?;
            assert_eq!(
                String::from_utf8_lossy(&bytes),
                "a=\"b\\\\nc\"\nb=\"x\\t y\"\nc=$HOME\nd=\"'x\"\n"
            );
            with_document(
                "dotenv",
                jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
                &bytes,
                |re_decoded| {
                    let again = owned_root(re_decoded)?;
                    if !values_equal(&root, &again) {
                        return Err(format!("the fixpoint must hold:\nfirst:  {root:?}\nsecond: {again:?}"));
                    }
                    Ok(())
                },
            )
        },
    )
    .expect("dotenv fixpoint");
}

/// A leading apostrophe decodes as an OPENING single quote: an unquoted encoding of such a value would be its decoder's
/// own unterminated-quote or quote-trailing-content fault. The quoting trigger fires on the leading apostrophe, so the
/// encoder's bytes wrap the value in a double-quote pair and decode back to the same document.
///
/// Sibling: the dotenv arm of `decode_encode_fixpoint_on_normalized_documents` pins the same `"'x"` spelling under the
/// fixpoint law.
#[test]
fn dotenv_encoder_quotes_a_leading_apostrophe() {
    let fixture = b"u=\"'x\"\nv=\"'a'b\"\n";
    with_product(
        "dotenv",
        jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
        fixture,
        |product| {
            let root = owned_root(product.document())?;
            let bytes = encode_document("dotenv", jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID, product)?;
            assert_eq!(String::from_utf8_lossy(&bytes), "u=\"'x\"\nv=\"'a'b\"\n");
            with_document(
                "dotenv",
                jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
                &bytes,
                |re_decoded| {
                    let again = owned_root(re_decoded)?;
                    if !values_equal(&root, &again) {
                        return Err(format!(
                            "the apostrophe round trip must hold:\nfirst:  {root:?}\nsecond: {again:?}"
                        ));
                    }
                    Ok(())
                },
            )
        },
    )
    .expect("dotenv apostrophe round trip");
}

/// A re-encode re-emits every comment fact — each entry's leading comments as `# text` lines before its line and the
/// document trailer as the ROOT's foot after the body — so a comment-bearing file round-trips its comments, and the
/// fact reads back identically at the SAME positions (`.key.@comment` and `.@comment_foot`).
#[test]
fn comments_survive_reencode_on_all_three_dialects() -> Result<(), String> {
    let cases: &[(&str, &str, &[u8])] = &[
        (
            "properties",
            jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID,
            b"# lead one\n! lead two\nk = v\n# trailer\n",
        ),
        (
            "ini",
            jqf_codec_ini::INI_JQF_STRICT_DIALECT_ID,
            b"; lead one\n# lead two\nk = 1\n[db]\nhost = localhost\n# trailer\n",
        ),
        (
            "dotenv",
            jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
            b"# lead\nk=bar\n# trailer\n",
        ),
    ];
    for (format, dialect, fixture) in cases {
        let output_dialect = match *format {
            "properties" => jqf_codec_ini::PROPERTIES_JQF_1_0_DIALECT_ID,
            "ini" => jqf_codec_ini::INI_JQF_1_0_DIALECT_ID,
            _ => jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID,
        };
        with_product(format, dialect, fixture, |product| {
            let bytes = encode_document(format, output_dialect, product)?;
            // The re-encode carries the comment lines; the leading set is marker-normalized to `#` (the declared
            // shared-renderer decision) and the trailer survives at the end.
            with_document(format, dialect, &bytes, |re_decoded| {
                let leading = comment_texts(re_decoded, entry_node(re_decoded, "k")?);
                let expected_leading: &[&str] = if *format == "dotenv" {
                    &["lead"]
                } else {
                    &["lead one", "lead two"]
                };
                if leading != expected_leading {
                    return Err(format!(
                        "{format}: leading comments {leading:?} != {expected_leading:?}"
                    ));
                }
                let foot = comment_fact_texts(re_decoded, re_decoded.root())?;
                if foot != ["trailer"] {
                    return Err(format!(
                        "{format}: the re-encoded root foot must be the trailer, got {foot:?}"
                    ));
                }
                Ok(())
            })
        })
        .map_err(|error| format!("{format}: {error}"))?;
    }
    Ok(())
}

/// The first entry's value node for `k` (the fixtures above all carry it).
fn entry_node(document: &jqf_data::Document<'_>, key: &str) -> Result<jqf_data::NodeId, String> {
    let root_view = document
        .value_view(document.node_handle(document.root()).expect("root handle"))
        .expect("view");
    let object = root_view.object().expect("object").expect("root object");
    for index in 0..object.len() {
        let entry = object.get_index(index).expect("member").expect("entry");
        if entry.key() == key {
            return Ok(entry.value().node());
        }
    }
    Err(format!("no member named {key}"))
}

/// The first list-of-text comment fact attached to one node, whatever its position role.
fn comment_fact_texts(document: &jqf_data::Document<'_>, node: jqf_data::NodeId) -> Result<Vec<String>, String> {
    let mut reader = document.fact_reader(&mut resources()).map_err(|e| format!("{e:?}"))?;
    loop {
        let poll = reader
            .poll_batch(jqf_data::unbounded_batch_limit(), &mut resources())
            .map_err(|e| format!("{e:?}"))?;
        match poll {
            jqf_data::ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    if fact.owner() != jqf_data::LocalOwnerRef::Node(node) {
                        continue;
                    }
                    if let jqf_data::FactPayloadView::List(texts) = fact.payload() {
                        let mut out = Vec::new();
                        for entry in texts.iter() {
                            if let jqf_data::FactPayloadView::Text(text) = entry {
                                out.push(String::from(text));
                            }
                        }
                        return Ok(out);
                    }
                }
            }
            jqf_data::ReaderPoll::Pending => {
                resources()
                    .try_begin_next_cooperative_entry(4_096)
                    .map_err(|e| format!("{e:?}"))?;
            }
            jqf_data::ReaderPoll::End(_) => break,
        }
    }
    Ok(Vec::new())
}

/// A section name containing `]` cannot round-trip: the scanner closes the header at the FIRST `]`, so `[a]b]`
/// re-decodes as section `a` plus trailing content. The encoder refuses the shape instead of emitting bytes that fail
/// its own decode law.
#[test]
fn ini_encoder_rejects_a_section_name_containing_close_bracket() {
    fn owned_object(entries: &[(&str, Value)]) -> Value {
        let mut builder = ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
        for (key, value) in entries {
            builder
                .try_insert_last(ObjectKey::try_from_str(key).expect("key"), value.clone())
                .expect("insert");
        }
        Value::Object(builder.try_finish().expect("object"))
    }
    let one = Value::String(jqf_data::Shared::try_from_str("1").expect("string"));
    let root = owned_object(&[("a]b", owned_object(&[("x", one)]))]);
    let mut resources = resources();
    let registration = jqf_codec_ini::registration_ini().expect("ini registration");
    let format_id = FormatId::try_new(jqf_codec_ini::INI_FORMAT_ID).expect("format");
    let dialect_id = DialectId::try_new(jqf_codec_ini::INI_JQF_1_0_DIALECT_ID).expect("dialect");
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            jqf_codec_core::EncodeRequest {
                format: &format_id,
                dialect: &dialect_id,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: jqf_codec_core::PreservationRequest::None,
                options: None,
            },
            &mut resources,
        )
        .expect("factory");
    let mut session = factory
        .start(
            jqf_codec_core::EncodeItem::owned(&root),
            jqf_codec_core::PreservationRequest::None,
            &mut resources,
        )
        .expect("session starts; the rejection is a body law");
    let mut out = Vec::new();
    let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(4_096);
    let error = session
        .encode(&mut sink, &mut run)
        .expect_err("a `]` in a section header cannot round-trip");
    assert_eq!(
        error.kind(),
        jqf_codec_core::CodecFailureKind::UnsupportedRepresentation
    );
}

fn owned_object(entries: &[(&str, Value)]) -> Value {
    let mut builder = ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
    for (key, value) in entries {
        builder
            .try_insert_last(ObjectKey::try_from_str(key).expect("key"), value.clone())
            .expect("insert");
    }
    Value::Object(builder.try_finish().expect("object"))
}

fn encode_owned(format: &str, dialect: &str, root: &Value) -> Result<Vec<u8>, jqf_codec_core::CodecFailureKind> {
    let mut resources = resources();
    let registration = match format {
        "properties" => jqf_codec_ini::registration().expect("properties"),
        "ini" => jqf_codec_ini::registration_ini().expect("ini"),
        "dotenv" => jqf_codec_ini::registration_dotenv().expect("dotenv"),
        _ => panic!("unknown format"),
    };
    let format_id = FormatId::try_new(match format {
        "properties" => jqf_codec_ini::FORMAT_ID,
        "ini" => jqf_codec_ini::INI_FORMAT_ID,
        _ => jqf_codec_ini::DOTENV_FORMAT_ID,
    })
    .expect("format");
    let dialect_id = DialectId::try_new(dialect).expect("dialect");
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            jqf_codec_core::EncodeRequest {
                format: &format_id,
                dialect: &dialect_id,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: jqf_codec_core::PreservationRequest::None,
                options: None,
            },
            &mut resources,
        )
        .expect("factory");
    let mut session = factory
        .start(
            jqf_codec_core::EncodeItem::owned(root),
            jqf_codec_core::PreservationRequest::None,
            &mut resources,
        )
        .expect("session");
    let mut out = Vec::new();
    let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(4_096);
    session.encode(&mut sink, &mut run).map_err(|error| error.kind())?;
    Ok(out)
}

fn string(text: &str) -> Value {
    Value::String(jqf_data::Shared::try_from_str(text).expect("string"))
}

#[test]
fn adjacent_values_are_a_requirement_mismatch() {
    let mut resources = resources();
    let registration = jqf_codec_ini::registration().expect("properties");
    let error = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(b"a=1\n"),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: true,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect_err("adjacent values are refused");
    assert_eq!(error.kind(), jqf_codec_core::CodecFailureKind::RequirementMismatch);
}

#[test]
fn dotenv_encode_writes_no_export_prefix() {
    with_product(
        "dotenv",
        jqf_codec_ini::DOTENV_JQF_STRICT_DIALECT_ID,
        b"export A=1\n",
        |product| {
            let bytes = encode_document("dotenv", jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID, product)?;
            assert_eq!(String::from_utf8_lossy(&bytes), "A=1\n");
            Ok(())
        },
    )
    .expect("export encode");
}

#[test]
fn dotenv_encoder_refuses_an_export_prefix_shaped_key() {
    let root = owned_object(&[("export FOO", string("1"))]);
    let error = encode_owned("dotenv", jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID, &root)
        .expect_err("export FOO cannot round-trip");
    assert_eq!(error, jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
}

#[test]
fn dotenv_encoder_accepts_a_colon_in_a_key() {
    let root = owned_object(&[("FOO:BAR", string("baz"))]);
    let bytes = encode_owned("dotenv", jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID, &root).expect("colon key");
    assert_eq!(String::from_utf8_lossy(&bytes), "FOO:BAR=baz\n");
}

#[test]
fn ini_encoder_refuses_an_empty_section_name() {
    let root = owned_object(&[("", owned_object(&[("x", string("1"))]))]);
    let error = encode_owned("ini", jqf_codec_ini::INI_JQF_1_0_DIALECT_ID, &root).expect_err("empty section");
    assert_eq!(error, jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
}

#[test]
fn ini_encoder_refuses_a_leading_blank_value() {
    let root = owned_object(&[("a", string(" b"))]);
    let error = encode_owned("ini", jqf_codec_ini::INI_JQF_1_0_DIALECT_ID, &root).expect_err("leading blank");
    assert_eq!(error, jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
}

#[test]
fn dotenv_encoder_quotes_a_leading_blank_value() {
    let root = owned_object(&[("a", string(" b"))]);
    let bytes = encode_owned("dotenv", jqf_codec_ini::DOTENV_JQF_1_0_DIALECT_ID, &root).expect("quote blank");
    assert_eq!(String::from_utf8_lossy(&bytes), "a=\" b\"\n");
}

#[test]
fn encoder_refuses_a_decimal_whose_scale_is_i64_min() {
    let decimal = jqf_data::Decimal::from_parts(jqf_data::Integer::from_i64(1), i64::MIN).expect("decimal");
    let root = owned_object(&[("n", Value::Number(jqf_data::Number::decimal(decimal)))]);
    let error =
        encode_owned("properties", jqf_codec_ini::PROPERTIES_JQF_1_0_DIALECT_ID, &root).expect_err("i64::MIN scale");
    assert_eq!(error, jqf_codec_core::CodecFailureKind::UnsupportedRepresentation);
}

fn last_member_comments_for(
    make_req: impl FnOnce(&ResourceContext<'_>) -> jqf_codec_core::AccessRequirement,
) -> Vec<String> {
    let bytes = b"# hello\na=1\n";
    let mut resources = resources();
    let requirement = make_req(&resources);
    let registration = jqf_codec_ini::registration().expect("properties");
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_ini::PROPERTIES_JDK_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: &[],
            },
            &mut resources,
        )
        .expect("provider");
    let handle = provider.bind(&requirement).expect("bind");
    let mut session = provider.open(&handle, &mut resources).expect("open");
    let mut run = CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(4_096);
    let result = session.decode(&mut run).expect("decode");
    let jqf_codec_core::AccessOutcome::FullDocument(product) = result.outcome() else {
        panic!("expected full document");
    };
    let document = product.document();
    let root_view = document
        .value_view(document.node_handle(document.root()).expect("root handle"))
        .expect("view");
    let root_object = root_view.object().expect("object").expect("root is an object");
    let node = root_object
        .get_index(root_object.len() - 1)
        .expect("member")
        .expect("entry")
        .value()
        .node();
    comment_texts(document, node)
}

#[test]
fn identity_demand_does_not_attach_comment_facts() {
    let comments = last_member_comments_for(whole_requirement);
    assert!(
        comments.is_empty(),
        "identity must skip comment facts, got {comments:?}"
    );
}

#[test]
fn comment_clause_attaches_comment_facts() {
    let comments = last_member_comments_for(comment_clause_requirement);
    assert_eq!(comments, ["hello"]);
}

#[test]
fn preserve_attaches_comment_facts() {
    let comments = last_member_comments_for(preserve_requirement);
    assert_eq!(comments, ["hello"]);
}
