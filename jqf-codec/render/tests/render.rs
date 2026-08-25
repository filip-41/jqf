//! End-to-end render-codec tests: every registered renderer's byte law, the extraction and layout laws, and the
//! shared-scalar formatter.

use jqf_codec_core::{CodecError, CodecFailureKind, CodecRunContext, EncodeItem, EncodeRequest, PreservationRequest};
use jqf_codec_render::{
    FORMAT_ID, GFM_TABLE_DIALECT_ID, GRID_TABLE_DIALECT_ID, HIST_DIALECT_ID, HTML_TABLE_DIALECT_ID, HeaderPolicy,
    PLAIN_DIALECT_ID, RenderEncodeOptions, SHELL_DIALECT_ID, TERMINAL_DIALECT_ID, TREE_DIALECT_ID, TerminalShape,
    WidthProfile,
};
use jqf_data::{Array, Object, ObjectBuilder, ObjectKey, TagId, Value};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

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

fn encode(value: &Value, dialect: &'static str, options: RenderEncodeOptions) -> Result<String, CodecError> {
    let mut resources = resources();
    let registration = jqf_codec_render::registration().expect("registration");
    let format = jqf_data::FormatId::try_new(FORMAT_ID).expect("format");
    let dialect = jqf_data::DialectId::try_new(dialect).expect("dialect");
    let factory = registration.encoder().expect("encoder").create_factory(
        EncodeRequest {
            format: &format,
            dialect: &dialect,
            diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            options: Some(&options as &(dyn core::any::Any + Send + Sync)),
        },
        &mut resources,
    )?;
    let mut session = factory
        .start(EncodeItem::Owned(value), PreservationRequest::None, &mut resources)
        .expect("session");
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session.encode(&mut sink, &mut run)?;
    }
    Ok(String::from_utf8(out).expect("UTF-8 render output"))
}

fn object(entries: &[(&str, Value)]) -> Value {
    let mut builder = ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
    for (key, value) in entries {
        builder
            .try_insert_last(ObjectKey::try_from_str(key).expect("key"), value.clone())
            .expect("insert");
    }
    Value::Object(builder.try_finish().expect("object"))
}

fn array(values: &[Value]) -> Value {
    let mut owned = Vec::new();
    for value in values {
        owned.push(value.clone());
    }
    Value::Array(Array::try_from_vec(owned).expect("array"))
}

fn string(text: &str) -> Value {
    Value::try_string(text).expect("string")
}

fn number(spelling: &str) -> Value {
    Value::Number(jqf_data::Number::try_json_literal(spelling).expect("number literal"))
}

fn num(value: i64) -> Value {
    number(&value.to_string())
}

/// One object per level, each holding its child under key `a`, over a `null` leaf: the deep-nesting shape the
/// depth-ceiling laws pin.
fn nested(depth: usize) -> Value {
    let mut value = Value::Null;
    for _ in 0..depth {
        value = object(&[("a", value)]);
    }
    value
}

/// Runs `f` on a thread whose stack comfortably holds the construction, walk, and drop-glue recursion of a
/// ceiling-depth value chain (a test thread's own 2 MiB default does not).
fn on_sized_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(128 * 1024 * 1024)
        .spawn(f)
        .expect("sized thread")
        .join()
        .expect("join")
}

fn expect_error(result: Result<String, CodecError>, kind: CodecFailureKind) {
    match result {
        Ok(text) => panic!("expected {kind:?}, got output {text:?}"),
        Err(error) => assert_eq!(error.kind(), kind, "wrong failure kind for {error:?}"),
    }
}

#[test]
fn plain_renders_scalars_with_facade_lf() {
    let default = RenderEncodeOptions::default();
    assert_eq!(encode(&Value::Null, PLAIN_DIALECT_ID, default).unwrap(), "null");
    assert_eq!(encode(&Value::Bool(true), PLAIN_DIALECT_ID, default).unwrap(), "true");
    assert_eq!(encode(&num(42), PLAIN_DIALECT_ID, default).unwrap(), "42");
    assert_eq!(encode(&number("-3.50"), PLAIN_DIALECT_ID, default).unwrap(), "-3.50");
    assert_eq!(encode(&string("hello"), PLAIN_DIALECT_ID, default).unwrap(), "hello");
    assert_eq!(encode(&string("a\nb"), PLAIN_DIALECT_ID, default).unwrap(), "a\nb");
}

#[test]
fn plain_rejects_containers_and_publishes_a_tagged_payload() {
    let default = RenderEncodeOptions::default();
    expect_error(
        encode(&array(&[num(1)]), PLAIN_DIALECT_ID, default),
        CodecFailureKind::UnsupportedRepresentation,
    );
    // The tag half of this case USED to be a refusal. The cross-format encode A tag on a plain-text target is spelled
    // as its payload: a target with no tag spelling publishes the bare payload, and a scalar it can render is not made
    // unrenderable by a label on it. The CONTAINER half is untouched: `render.plain@1` renders one scalar per frame,
    // which is a shape law and not a representability one.
    let tagged = Value::try_tagged(TagId::try_new_unaccounted("!m").expect("tag"), num(1)).expect("tagged");
    assert_eq!(encode(&tagged, PLAIN_DIALECT_ID, default).expect("published"), "1");
}

#[test]
fn plain_renders_render_non_finite_law() {
    let default = RenderEncodeOptions::default();
    let nan = Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NAN)));
    let inf = Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::INFINITY)));
    let neg_inf = Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NEG_INFINITY)));
    let zero = Value::Number(jqf_data::Number::float(jqf_data::Float::new(-0.0)));
    assert_eq!(
        encode(&nan, PLAIN_DIALECT_ID, default).unwrap(),
        "nan(0x7ff8000000000000)"
    );
    assert_eq!(encode(&inf, PLAIN_DIALECT_ID, default).unwrap(), "inf");
    assert_eq!(encode(&neg_inf, PLAIN_DIALECT_ID, default).unwrap(), "-inf");
    assert_eq!(encode(&zero, PLAIN_DIALECT_ID, default).unwrap(), "-0");
}

#[test]
fn plain_renders_bytes_and_temporals() {
    let default = RenderEncodeOptions::default();
    let bytes = Value::try_bytes(&[0x00, 0x0a, 0xff]).expect("bytes");
    assert_eq!(encode(&bytes, PLAIN_DIALECT_ID, default).unwrap(), "0x000aff");
    let date = Value::LocalDate(jqf_data::LocalDate::new(2026, 8, 3).expect("date"));
    assert_eq!(encode(&date, PLAIN_DIALECT_ID, default).unwrap(), "2026-08-03");
}

#[test]
fn gfm_table_from_array_of_objects() {
    let default = RenderEncodeOptions::default();
    let rows = array(&[
        object(&[("name", string("alice")), ("age", num(30))]),
        object(&[("name", string("bob")), ("age", num(40))]),
    ]);
    assert_eq!(
        encode(&rows, GFM_TABLE_DIALECT_ID, default).unwrap(),
        "| name | age |\n| :--- | ---: |\n| alice | 30 |\n| bob | 40 |"
    );
}

#[test]
fn gfm_table_from_single_object() {
    let default = RenderEncodeOptions::default();
    let single = object(&[("timeout", num(30)), ("retries", num(2))]);
    assert_eq!(
        encode(&single, GFM_TABLE_DIALECT_ID, default).unwrap(),
        "| timeout | retries |\n| ---: | ---: |\n| 30 | 2 |"
    );
}

#[test]
fn gfm_escapes_only_what_gfm_requires() {
    let default = RenderEncodeOptions::default();
    // The minimal GFM cell law: `|` (the delimiter) and `&`/`<`/`>` (raw-HTML safety) escape; every other punctuation
    // scalar copies RAW — the old escape-everything law emitted `&#x2D;` for `-` and `&#x2E;` for `.`, which
    // displayed fine but made the markdown source copy-hostile.
    let rows = array(&[object(&[
        ("name", string("a|b")),
        ("ver", string("1.2-beta")),
        ("md", string("x*y & <tag> \"q\" 'a'")),
        ("ctl", string("a\tb")),
    ])]);
    assert_eq!(
        encode(&rows, GFM_TABLE_DIALECT_ID, default).unwrap(),
        concat!(
            "| name | ver | md | ctl |\n",
            "| :--- | :--- | :--- | :--- |\n",
            "| a&#x7C;b | 1.2-beta | x*y &#x26; &#x3C;tag&#x3E; \"q\" 'a' | aU&#x2B;0009b |",
        )
    );
}

#[test]
fn gfm_requires_present_header() {
    let options = RenderEncodeOptions {
        header: HeaderPolicy::Absent,
        ..RenderEncodeOptions::default()
    };
    expect_error(
        encode(&array(&[object(&[("a", num(1))])]), GFM_TABLE_DIALECT_ID, options),
        CodecFailureKind::UnsupportedRepresentation,
    );
}

#[test]
fn gfm_missing_members_become_null_cells() {
    let default = RenderEncodeOptions::default();
    let rows = array(&[
        object(&[("id", num(1)), ("name", string("a"))]),
        object(&[("id", num(2))]),
    ]);
    assert_eq!(
        encode(&rows, GFM_TABLE_DIALECT_ID, default).unwrap(),
        "| id | name |\n| ---: | :--- |\n| 1 | a |\n| 2 | null |"
    );
}

#[test]
fn table_rejects_non_object_rows() {
    let default = RenderEncodeOptions::default();
    expect_error(
        encode(&array(&[num(1)]), GFM_TABLE_DIALECT_ID, default),
        CodecFailureKind::UnsupportedRepresentation,
    );
    expect_error(
        encode(&num(1), GFM_TABLE_DIALECT_ID, default),
        CodecFailureKind::UnsupportedRepresentation,
    );
}

#[test]
fn gfm_renders_nested_container_cells_as_compact_json() {
    let default = RenderEncodeOptions::default();
    // A nested cell is TEXT (the miller convention): the structure travels as its compact JSON spelling instead of
    // failing the whole table.
    let rows = array(&[object(&[(
        "cell",
        object(&[("x", num(1)), ("y", array(&[num(2), Value::Null])), ("z", string("s"))]),
    )])]);
    assert_eq!(
        encode(&rows, GFM_TABLE_DIALECT_ID, default).unwrap(),
        "| cell |\n| :--- |\n| {\"x\":1,\"y\":[2,null],\"z\":\"s\"} |"
    );
}

/// The depth ceiling pins BOTH sides: a cell whose leaf sits 9\_999 containers deep still renders, and one more level
/// refuses by name instead of recursing past the crate's shared ceiling — the walk counts every node entry, so a
/// `nested(10_000)` chain puts its `null` leaf AT the ceiling.
#[test]
fn gfm_refuses_a_cell_nested_past_the_depth_ceiling() {
    on_sized_stack(|| {
        let default = RenderEncodeOptions::default();
        let at_ceiling = array(&[object(&[("a", nested(9_999))])]);
        assert!(encode(&at_ceiling, GFM_TABLE_DIALECT_ID, default).is_ok());
        let past = array(&[object(&[("a", nested(10_000))])]);
        expect_error(
            encode(&past, GFM_TABLE_DIALECT_ID, default),
            CodecFailureKind::UnsupportedRepresentation,
        );
    });
}

#[test]
fn empty_tables_publish_an_empty_frame() {
    let default = RenderEncodeOptions::default();
    // `[]` and `{}` extract to a zero-column table: the frame is the EMPTY text (the facade appends its LF), not a
    // refusal — an empty input prints an empty table.
    for dialect in [GFM_TABLE_DIALECT_ID, HTML_TABLE_DIALECT_ID, GRID_TABLE_DIALECT_ID] {
        for value in [array(&[]), object(&[])] {
            assert_eq!(
                encode(&value, dialect, default).unwrap(),
                "",
                "{dialect} must publish an empty frame"
            );
        }
    }
}

#[test]
fn gfm_renders_non_finite_members_inside_json_cells_per_the_json_law() {
    let default = RenderEncodeOptions::default();
    let nan = Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NAN)));
    let inf = Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::INFINITY)));
    let neg_inf = Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NEG_INFINITY)));
    let rows = array(&[object(&[("cell", array(&[array(&[]), nan, inf, neg_inf]))])]);
    // A non-finite number has no JSON literal inside a JSON-text cell: it renders the JSON encoder's own non-finite
    // law. A TOP-LEVEL scalar cell keeps the render scalar law (`nan(0x...)`, `inf`) — only container cells travel as
    // JSON text.
    assert_eq!(
        encode(&rows, GFM_TABLE_DIALECT_ID, default).unwrap(),
        "| cell |\n| :--- |\n| [[],null,1.7976931348623157e+308,-1.7976931348623157e+308] |"
    );
}

#[test]
fn html_table_fragment() {
    let default = RenderEncodeOptions::default();
    let rows = array(&[
        object(&[("name", string("a<b")), ("age", num(30))]),
        object(&[("name", string("c&d")), ("age", num(40))]),
    ]);
    assert_eq!(
        encode(&rows, HTML_TABLE_DIALECT_ID, default).unwrap(),
        concat!(
            "<table>\n",
            "<thead>\n<tr><th scope=\"col\" style=\"text-align: left; white-space: pre-wrap\">name</th>",
            "<th scope=\"col\" style=\"text-align: right; white-space: pre-wrap\">age</th></tr>\n",
            "</thead>\n",
            "<tbody>\n",
            "<tr><td style=\"text-align: left; white-space: pre-wrap\">a&lt;b</td>",
            "<td style=\"text-align: right; white-space: pre-wrap\">30</td></tr>\n",
            "<tr><td style=\"text-align: left; white-space: pre-wrap\">c&amp;d</td>",
            "<td style=\"text-align: right; white-space: pre-wrap\">40</td></tr>\n",
            "</tbody>\n</table>",
        )
    );
}

#[test]
fn html_absent_header() {
    let options = RenderEncodeOptions {
        header: HeaderPolicy::Absent,
        ..RenderEncodeOptions::default()
    };
    let single = object(&[("a", num(1))]);
    assert_eq!(
        encode(&single, HTML_TABLE_DIALECT_ID, options).unwrap(),
        concat!(
            "<table>\n",
            "<tbody>\n",
            "<tr><td style=\"text-align: right; white-space: pre-wrap\">1</td></tr>\n",
            "</tbody>\n</table>",
        )
    );
}

#[test]
fn grid_table_ascii_only() {
    let default = RenderEncodeOptions::default();
    let rows = array(&[
        object(&[("name", string("a")), ("age", num(30))]),
        object(&[("name", string("longer")), ("age", num(400))]),
    ]);
    assert_eq!(
        encode(&rows, GRID_TABLE_DIALECT_ID, default).unwrap(),
        concat!(
            "+--------+-----+\n",
            "| name   | age |\n",
            "+--------+-----+\n",
            "| a      |  30 |\n",
            "+--------+-----+\n",
            "| longer | 400 |\n",
            "+--------+-----+",
        )
    );
}

#[test]
fn grid_table_escapes_controls_and_backslashes() {
    let default = RenderEncodeOptions::default();
    let single = object(&[("a", string("x\\y")), ("b", string("a\tb"))]);
    assert_eq!(
        encode(&single, GRID_TABLE_DIALECT_ID, default).unwrap(),
        concat!(
            "+------+------+\n",
            "| a    | b    |\n",
            "+------+------+\n",
            "| x\\\\y | a\\tb |\n",
            "+------+------+",
        )
    );
}

#[test]
fn grid_wraps_cells_at_frozen_width() {
    let options = RenderEncodeOptions {
        max_width: 4,
        ..RenderEncodeOptions::default()
    };
    let rows = array(&[
        object(&[("name", string("abcdef")), ("age", num(1))]),
        object(&[("name", string("xy")), ("age", num(2))]),
    ]);
    // Column natural width is 6 (name), capped to 4; `abcdef` wraps.
    assert_eq!(
        encode(&rows, GRID_TABLE_DIALECT_ID, options).unwrap(),
        concat!(
            "+------+-----+\n",
            "| name | age |\n",
            "+------+-----+\n",
            "| abcd |   1 |\n",
            "| ef   |     |\n",
            "+------+-----+\n",
            "| xy   |   2 |\n",
            "+------+-----+",
        )
    );
}

#[test]
fn grid_table_rejects_wide_atom() {
    let options = RenderEncodeOptions {
        max_width: 1,
        ..RenderEncodeOptions::default()
    };
    let rows = array(&[object(&[("name", string("\u{4e00}"))])]);
    // The CJK scalar is one width-2 atom; the frozen column width is 1, so it is CellTooWide before any frame.
    expect_error(
        encode(&rows, GRID_TABLE_DIALECT_ID, options),
        CodecFailureKind::UnsupportedRepresentation,
    );
}

#[test]
fn tree_renders_nested_document() {
    let default = RenderEncodeOptions::default();
    let value = object(&[
        ("name", string("alice")),
        ("age", num(30)),
        ("tags", array(&[string("a"), string("b")])),
    ]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        concat!(
            "$ = object(3)\n",
            "  $[\"name\"]#0 = \"alice\"\n",
            "  $[\"age\"]#1 = 30\n",
            "  $[\"tags\"]#2 = array(2)\n",
            "    $[\"tags\"]#2[0] = \"a\"\n",
            "    $[\"tags\"]#2[1] = \"b\"",
        )
    );
}

#[test]
fn tree_anchors_shared_containers() {
    let default = RenderEncodeOptions::default();
    let shared = array(&[num(1), num(2)]);
    let Value::Array(shared) = shared else { unreachable!() };
    let again = Value::Array(shared.clone_shared());
    let first = Value::Array(shared.clone_shared());
    let value = array(&[again, first]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        concat!(
            "$ = array(2)\n",
            "  $[0] = &0 array(2)\n",
            "    $[0][0] = 1\n",
            "    $[0][1] = 2\n",
            "  $[1] = *0",
        )
    );
}

#[test]
fn tree_renders_tags_and_quotes_keys() {
    let default = RenderEncodeOptions::default();
    let tagged = Value::try_tagged(
        TagId::try_new_unaccounted("!money").expect("tag"),
        object(&[("amount", num(12))]),
    )
    .expect("tagged");
    let value = object(&[("price", tagged)]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        concat!(
            "$ = object(1)\n",
            "  $[\"price\"]#0 = tag(\"!money\")\n",
            "    $[\"price\"]#0.payload = object(1)\n",
            "      $[\"price\"]#0.payload[\"amount\"]#0 = 12",
        )
    );
}

#[test]
fn tree_forces_controls_visible() {
    let default = RenderEncodeOptions::default();
    let value = object(&[("a", string("x\u{007f}y"))]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        "$ = object(1)\n  $[\"a\"]#0 = \"x\\u007fy\""
    );
}

#[test]
fn tree_anchors_containers_shared_under_tags() {
    let default = RenderEncodeOptions::default();
    // The SAME object allocation appears under two different tags. The sharing prepass must descend into tag payloads
    // (as emit does) or the container prints in full at every occurrence instead of anchoring.
    let shared = object(&[("amount", num(12))]);
    let Value::Object(shared) = shared else { unreachable!() };
    let first = Value::try_tagged(
        TagId::try_new_unaccounted("!first").expect("tag"),
        Value::Object(shared.clone_shared()),
    )
    .expect("tagged");
    let again = Value::try_tagged(
        TagId::try_new_unaccounted("!again").expect("tag"),
        Value::Object(shared.clone_shared()),
    )
    .expect("tagged");
    let value = array(&[first, again]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        concat!(
            "$ = array(2)\n",
            "  $[0] = tag(\"!first\")\n",
            "    $[0].payload = &0 object(1)\n",
            "      $[0].payload[\"amount\"]#0 = 12\n",
            "  $[1] = tag(\"!again\")\n",
            "    $[1].payload = *0",
        )
    );
}

#[test]
fn tree_tagged_root_anchors_shared_descendants() {
    let default = RenderEncodeOptions::default();
    // A TAGGED ROOT used to disable anchoring for the whole document: the prepass stopped at the root tag, so no slot
    // was ever counted. It must descend through the tag exactly as emit does.
    let shared = array(&[num(1), num(2)]);
    let Value::Array(shared) = shared else { unreachable!() };
    let tagged = Value::try_tagged(
        TagId::try_new_unaccounted("!doc").expect("tag"),
        array(&[Value::Array(shared.clone_shared()), Value::Array(shared.clone_shared())]),
    )
    .expect("tagged");
    assert_eq!(
        encode(&tagged, TREE_DIALECT_ID, default).unwrap(),
        concat!(
            "$ = tag(\"!doc\")\n",
            "  $.payload = array(2)\n",
            "    $.payload[0] = &0 array(2)\n",
            "      $.payload[0][0] = 1\n",
            "      $.payload[0][1] = 2\n",
            "    $.payload[1] = *0",
        )
    );
}

#[test]
fn tree_anchors_a_shared_empty_container_through_the_table_path() {
    let default = RenderEncodeOptions::default();
    // One empty-array allocation seen twice: an empty container has no element buffer, so its identity travels the
    // allocation-key path rather than any element address. It must anchor exactly like a shared non-empty container.
    let empty = Value::Array(Array::try_new().expect("empty"));
    let value = array(&[empty.clone(), empty]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        concat!("$ = array(2)\n", "  $[0] = &0 array(0)\n", "  $[1] = *0",)
    );
}

#[test]
fn tree_anchors_a_shared_empty_object_through_the_table_path() {
    let default = RenderEncodeOptions::default();
    // The object twin of the empty-anchor law.
    let empty = Value::Object(Object::try_new().expect("empty"));
    let value = object(&[("a", empty.clone()), ("b", empty)]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        concat!("$ = object(2)\n", "  $[\"a\"]#0 = &0 object(0)\n", "  $[\"b\"]#1 = *0",)
    );
}

#[test]
fn tree_keeps_distinct_empty_containers_distinct() {
    let default = RenderEncodeOptions::default();
    // Two independently built empties are two allocations and must stay two slots; only the aliased pair anchors. A key
    // collision here would merge distinct nodes or fabricate anchors.
    let shared = Value::Array(Array::try_new().expect("empty"));
    let lone = Value::Array(Array::try_new().expect("empty"));
    let value = array(&[shared.clone(), lone.clone(), shared]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        concat!(
            "$ = array(3)\n",
            "  $[0] = &0 array(0)\n",
            "  $[1] = array(0)\n",
            "  $[2] = *0",
        )
    );
    // Mixed kinds and mixed sharing in one document: each kind's keys live in their own namespace, so an empty array
    // never merges with an empty object even by address coincidence.
    let shared_array = Value::Array(Array::try_new().expect("empty"));
    let shared_object = Value::Object(Object::try_new().expect("empty"));
    let value = array(&[
        shared_array.clone(),
        shared_array,
        shared_object.clone(),
        shared_object,
        Value::Array(Array::try_new().expect("empty")),
    ]);
    assert_eq!(
        encode(&value, TREE_DIALECT_ID, default).unwrap(),
        concat!(
            "$ = array(5)\n",
            "  $[0] = &0 array(0)\n",
            "  $[1] = *0\n",
            "  $[2] = &1 object(0)\n",
            "  $[3] = *1\n",
            "  $[4] = array(0)",
        )
    );
}

#[test]
fn tree_completes_over_ten_thousand_distinct_empty_containers() {
    let default = RenderEncodeOptions::default();
    // Ten thousand DISTINCT empty allocations. The retired miss-scan compared each sighting against every earlier
    // bucket entry (quadratic); the keyed lookup answers each in O(log C). This lane pins completion and shape, not
    // speed — the timing evidence lives in the measurement receipts.
    let mut owned = Vec::new();
    for _ in 0..10_000 {
        owned.push(Value::Array(Array::try_new().expect("empty")));
    }
    let value = Value::Array(Array::try_from_vec(owned).expect("array"));
    let frame = encode(&value, TREE_DIALECT_ID, default).expect("ten thousand distinct empties must render");
    assert!(
        frame.starts_with("$ = array(10000)\n"),
        "root line must name the count: {}",
        frame.lines().next().unwrap_or("")
    );
    assert_eq!(frame.matches('\n').count(), 10_000, "one line per node");
    assert_eq!(
        frame.matches("array(0)").count(),
        10_000,
        "every element prints in full — no anchoring between distinct empties"
    );
}

#[test]
fn terminal_plain_escapes_controls() {
    let options = RenderEncodeOptions {
        terminal_shape: TerminalShape::Plain,
        ..RenderEncodeOptions::default()
    };
    let value = string("a\tb\\c\nd");
    assert_eq!(encode(&value, TERMINAL_DIALECT_ID, options).unwrap(), "a\\tb\\\\c\\nd");
}

#[test]
fn terminal_tree_shape_accepts_containers() {
    let default = RenderEncodeOptions::default();
    let value = object(&[("a", num(1))]);
    assert_eq!(
        encode(&value, TERMINAL_DIALECT_ID, default).unwrap(),
        "$ = object(1)\n  $[\"a\"]#0 = 1"
    );
}

#[test]
fn terminal_plain_rejects_containers() {
    let options = RenderEncodeOptions {
        terminal_shape: TerminalShape::Plain,
        ..RenderEncodeOptions::default()
    };
    expect_error(
        encode(&object(&[("a", num(1))]), TERMINAL_DIALECT_ID, options),
        CodecFailureKind::UnsupportedRepresentation,
    );
}

#[test]
fn width_profiles_differ_on_ambiguous() {
    let default = RenderEncodeOptions::default();
    // U+00A1 is ambiguous: width 1 under western, 2 under cjk. Two cells of it pad differently in the grid.
    let western = RenderEncodeOptions {
        width: WidthProfile::Western,
        ..default
    };
    let cjk = RenderEncodeOptions {
        width: WidthProfile::Cjk,
        ..default
    };
    let value = object(&[("a", string("\u{00a1}"))]);
    let west = encode(&value, GRID_TABLE_DIALECT_ID, western).unwrap();
    let east = encode(&value, GRID_TABLE_DIALECT_ID, cjk).unwrap();
    assert_ne!(west, east);
}

#[test]
fn table_row_cap_fails_without_a_frame() {
    let options = RenderEncodeOptions {
        sample_rows: 2,
        ..RenderEncodeOptions::default()
    };
    let rows = array(&[
        object(&[("a", num(1))]),
        object(&[("a", num(2))]),
        object(&[("a", num(3))]),
    ]);
    expect_error(
        encode(&rows, GRID_TABLE_DIALECT_ID, options),
        CodecFailureKind::UnsupportedRepresentation,
    );
}

#[test]
fn shell_renders_ordinary_assignments() {
    let default = RenderEncodeOptions::default();
    assert_eq!(
        encode(&object(&[("a", num(1)), ("b", string("x"))]), SHELL_DIALECT_ID, default).unwrap(),
        "a=1\nb='x'"
    );
}

#[test]
fn shell_renders_empty_containers_as_json_literals() {
    let default = RenderEncodeOptions::default();
    // An empty container has no leaves to flatten and is never dropped: its assignment spells its quoted JSON literal.
    assert_eq!(
        encode(
            &object(&[("a", array(&[])), ("b", object(&[]))]),
            SHELL_DIALECT_ID,
            default
        )
        .unwrap(),
        "a='[]'\nb='{}'"
    );
    // A root empty container sits under the fixed key `value`, like a root scalar.
    assert_eq!(encode(&array(&[]), SHELL_DIALECT_ID, default).unwrap(), "value='[]'");
}

#[test]
fn shell_renders_non_finite_through_the_render_law() {
    let default = RenderEncodeOptions::default();
    // The shared non-finite law, not JSON's `null`: a NaN spelled `null` would be indistinguishable from a real null.
    // The NaN spelling carries parentheses — not legal bare assignment text — so the word is quoted; a sourced
    // consumer reads the exact bits back.
    assert_eq!(
        encode(
            &object(&[
                (
                    "nan",
                    Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NAN)))
                ),
                (
                    "inf",
                    Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::INFINITY)))
                ),
                (
                    "ninf",
                    Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NEG_INFINITY)))
                ),
            ]),
            SHELL_DIALECT_ID,
            default
        )
        .unwrap(),
        "nan='nan(0x7ff8000000000000)'\ninf='inf'\nninf='-inf'"
    );
}

#[test]
fn gfm_escapes_the_delimiter_inside_a_multi_scalar_cluster() {
    let default = RenderEncodeOptions::default();
    // Escaping is decided PER SCALAR: a combining mark attached to `|` must not smuggle a raw cell delimiter into the
    // markdown source (a re-parse of the table would split the row at it). The entity displays as the pipe and the mark
    // rides after it.
    let rows = array(&[object(&[("cell", string("|\u{0301}"))])]);
    let out = encode(&rows, GFM_TABLE_DIALECT_ID, default).unwrap();
    assert!(
        out.contains("| &#x7C;\u{0301} |"),
        "the pipe must escape even under a combining mark: {out}"
    );
}

#[test]
fn html_escapes_raw_html_inside_a_multi_scalar_cluster() {
    let default = RenderEncodeOptions::default();
    // Same per-scalar law for the HTML renderer: `<` under a combining mark must not leak raw HTML into the cell.
    let rows = array(&[object(&[("cell", string("<\u{0301}"))])]);
    let out = encode(&rows, HTML_TABLE_DIALECT_ID, default).unwrap();
    assert!(
        out.contains("&lt;\u{0301}"),
        "the angle bracket must escape even under a combining mark: {out}"
    );
}

#[test]
fn shell_refuses_caller_significant_names() {
    let default = RenderEncodeOptions::default();
    for key in ["IFS", "PATH", "LD_PRELOAD"] {
        let error = encode(&object(&[(key, string("x"))]), SHELL_DIALECT_ID, default)
            .expect_err("a trap-set key must refuse, not emit a bare assignment");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
        let diagnostic = error.diagnostic().expect("shell-special diagnostic");
        assert!(
            diagnostic.message().contains(key),
            "refusal must name {key}: {}",
            diagnostic.message()
        );
        assert!(
            !diagnostic.message().is_empty(),
            "refusal must carry prose, not a bare kind"
        );
    }
}

#[test]
fn shell_refuses_a_flattened_trap_set_name() {
    let default = RenderEncodeOptions::default();
    // Nested keys join with `_`, so `LD` + `PRELOAD` is a bare `LD_PRELOAD=` assignment unless the flattened name is
    // refused.
    let nested = object(&[("LD", object(&[("PRELOAD", string("x.so"))]))]);
    let error =
        encode(&nested, SHELL_DIALECT_ID, default).expect_err("flattening must not construct a trap-set assignment");
    assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    let diagnostic = error.diagnostic().expect("shell-special diagnostic");
    assert!(
        diagnostic.message().contains("LD_PRELOAD"),
        "refusal must name the flattened variable: {}",
        diagnostic.message()
    );
}

/// Two distinct document paths that flatten to the same variable name refuse with both paths named — the collision is
/// never a silent overwrite.
#[test]
fn shell_refuses_a_flattened_collision_with_both_paths() {
    let default = RenderEncodeOptions::default();
    // `a_b` and `a.b` (nested) both flatten to `a_b`.
    let colliding = object(&[("a", object(&[("b", string("x"))])), ("a_b", string("y"))]);
    let error =
        encode(&colliding, SHELL_DIALECT_ID, default).expect_err("two paths flattening to one variable must refuse");
    assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    let diagnostic = error.diagnostic().expect("shell-collision diagnostic");
    assert!(
        diagnostic.message().contains(".a.b") && diagnostic.message().contains(".a_b"),
        "refusal must name BOTH document paths: {}",
        diagnostic.message()
    );
}

/// The depth ceiling pins BOTH sides: the flattened walk counts every node entry (the root object adds one), so a
/// `nested(9_998)` chain's leaf sits at 9\_999 and still flattens, while one more level puts a node AT the crate's
/// shared ceiling and refuses instead of recursing past it.
#[test]
fn shell_refuses_a_value_nested_past_the_depth_ceiling() {
    on_sized_stack(|| {
        let default = RenderEncodeOptions::default();
        let at_ceiling = object(&[("a", nested(9_998))]);
        assert!(encode(&at_ceiling, SHELL_DIALECT_ID, default).is_ok());
        let past = object(&[("a", nested(9_999))]);
        expect_error(
            encode(&past, SHELL_DIALECT_ID, default),
            CodecFailureKind::UnsupportedRepresentation,
        );
    });
}

// --- render.hist@1: the plain-ASCII frequency histogram ---

fn hist(value: &Value) -> Result<String, CodecError> {
    encode(value, HIST_DIALECT_ID, RenderEncodeOptions::default())
}

/// The golden ten-bin layout over a clean span: labels left-aligned and padded on the right to the widest label, counts
/// right-aligned, `#` bars scaled to the peak bin.
#[test]
fn hist_renders_ten_equal_width_bins_over_the_span() {
    let frame = hist(&array(&[num(0), num(5), num(10), num(15), num(20), num(20), num(20)]))
        .expect("histogram of an array of numbers");
    let expected = [
        "[0, 2)   | 1 | ##############",
        "[2, 4)   | 0 |",
        "[4, 6)   | 1 | ##############",
        "[6, 8)   | 0 |",
        "[8, 10)  | 0 |",
        "[10, 12) | 1 | ##############",
        "[12, 14) | 0 |",
        "[14, 16) | 1 | ##############",
        "[16, 18) | 0 |",
        "[18, 20] | 3 | ########################################",
    ]
    .join("\n");
    assert_eq!(frame, expected);
}

/// A span whose difference overflows binary64 degenerates to ONE closed bin over the authored endpoints — never
/// non-finite edges spelled "null", and never counts lost out of every bin.
#[test]
fn hist_span_overflow_degenerates_to_one_closed_bin() {
    let frame = hist(&array(&[number("-1e308"), number("1e308")])).expect("histogram over an overflowing span");
    let low = jqf_data::format_binary64(-1e308).expect("finite edge spelling");
    let high = jqf_data::format_binary64(1e308).expect("finite edge spelling");
    let expected = format!("[{}, {}] | 2 | {}", low.as_str(), high.as_str(), "#".repeat(40));
    assert_eq!(frame, expected);
}

/// A span so narrow its equal-width step underflows to zero (near-subnormal endpoints) takes the SAME one-closed-bin
/// degenerate law as the overflow arm — never ten identical labels with bins placed by NaN/saturating-cast accident,
/// and never a count lost.
#[test]
fn hist_step_underflow_degenerates_to_one_closed_bin() {
    let frame = hist(&array(&[number("5e-324"), number("1e-323")])).expect("histogram over an underflowing step");
    let low = jqf_data::format_binary64(5e-324).expect("finite edge spelling");
    let high = jqf_data::format_binary64(1e-323).expect("finite edge spelling");
    let expected = format!("[{}, {}] | 2 | {}", low.as_str(), high.as_str(), "#".repeat(40));
    assert_eq!(frame, expected);
}

/// An empty array publishes an empty frame — never an error.
#[test]
fn hist_publishes_an_empty_frame_for_an_empty_array() {
    assert_eq!(hist(&array(&[])).expect("empty histogram"), "");
}

/// One distinct value is one bin labeled by the value itself, at full bar.
#[test]
fn hist_single_value_is_one_bin_at_full_bar() {
    let frame = hist(&array(&[num(7)])).expect("single-value histogram");
    assert_eq!(frame, format!("7 | 1 | {}", "#".repeat(40)));
}

/// The pre-aggregated `{value,count}` shape: each object contributes its count to its value's bin.
#[test]
fn hist_accepts_pre_aggregated_value_count_objects() {
    let weighted = array(&[
        object(&[("value", num(0)), ("count", num(2))]),
        object(&[("value", num(10)), ("count", num(3))]),
    ]);
    let frame = hist(&weighted).expect("pre-aggregated histogram");
    let expected = [
        "[0, 1)  | 2 | ###########################",
        "[1, 2)  | 0 |",
        "[2, 3)  | 0 |",
        "[3, 4)  | 0 |",
        "[4, 5)  | 0 |",
        "[5, 6)  | 0 |",
        "[6, 7)  | 0 |",
        "[7, 8)  | 0 |",
        "[8, 9)  | 0 |",
        "[9, 10] | 3 | ########################################",
    ]
    .join("\n");
    assert_eq!(frame, expected);
}

/// Counts whose accumulation exceeds u64 refuse by name — they never wrap into a wrong histogram at exit 0.
#[test]
fn hist_refuses_when_counts_accumulate_past_u64() {
    let weighted = array(&[
        object(&[("value", num(1)), ("count", number("9223372036854775807"))]),
        object(&[("value", num(1)), ("count", number("9223372036854775807"))]),
        object(&[("value", num(1)), ("count", number("9223372036854775807"))]),
    ]);
    let error = hist(&weighted).expect_err("counts past u64 must refuse");
    assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    let diagnostic = error.diagnostic().expect("hist-count-overflow diagnostic");
    assert!(
        diagnostic.message().contains("largest representable"),
        "the overflow refusal must name the accumulation law: {}",
        diagnostic.message()
    );
}

/// Every refusal names the shape problem and fires before any byte of the frame is staged.
#[test]
fn hist_refusals_name_the_shape_problem_and_publish_zero_bytes() {
    // A non-array root refuses with the dialect's input law.
    let error = hist(&num(5)).expect_err("a scalar root must refuse");
    assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    let diagnostic = error.diagnostic().expect("hist-shape diagnostic");
    assert!(
        diagnostic.message().contains("array of numbers"),
        "the root refusal must name the accepted shapes: {}",
        diagnostic.message()
    );
    // A non-number element names its index.
    let error = hist(&array(&[num(1), string("x")])).expect_err("a string element must refuse");
    assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    let diagnostic = error.diagnostic().expect("hist-element diagnostic");
    assert!(
        diagnostic.message().contains("[1]"),
        "the element refusal must name the index: {}",
        diagnostic.message()
    );
    // A NaN element has no finite bin edge.
    let nan = Value::Number(jqf_data::Number::float(jqf_data::Float::new(f64::NAN)));
    let error = hist(&array(&[nan])).expect_err("NaN must refuse");
    assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    let diagnostic = error.diagnostic().expect("hist-value diagnostic");
    assert!(
        diagnostic.message().contains("finite"),
        "the NaN refusal must name finiteness: {}",
        diagnostic.message()
    );
    // A {value,count} object with extra members is not the pre-aggregated shape.
    let fat = object(&[("value", num(1)), ("count", num(1)), ("n", num(2))]);
    let error = hist(&array(&[fat])).expect_err("a three-member object must refuse");
    assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    // A negative count refuses by name.
    let negative = object(&[("value", num(1)), ("count", number("-2"))]);
    let error = hist(&array(&[negative])).expect_err("a negative count must refuse");
    let diagnostic = error.diagnostic().expect("hist-count diagnostic");
    assert!(
        diagnostic.message().contains("non-negative"),
        "the count refusal must name the law: {}",
        diagnostic.message()
    );
}

/// A min..max span whose difference overflows binary64 degenerates to ONE closed bin over the authored endpoints —
/// never non-finite edges spelled "null", never counts lost, and never a refusal of finite inputs.
#[test]
fn hist_span_overflow_degenerates_to_one_closed_bin_wide() {
    let wide = array(&[number("-1.7e308"), number("1.7e308")]);
    let frame = hist(&wide).expect("an overflowing span still renders");
    let low = jqf_data::format_binary64(-1.7e308).expect("finite edge spelling");
    let high = jqf_data::format_binary64(1.7e308).expect("finite edge spelling");
    let expected = format!("[{}, {}] | 2 | {}", low.as_str(), high.as_str(), "#".repeat(40));
    assert_eq!(frame, expected);
}

/// Pre-aggregated counts whose sum passes `u64` refuse by name — never a silent release-mode wrap publishing a wrong
/// total.
#[test]
fn hist_refuses_a_count_total_past_u64() {
    let overflowing = array(&[
        object(&[("value", num(1)), ("count", num(i64::MAX))]),
        object(&[("value", num(1)), ("count", num(i64::MAX))]),
        object(&[("value", num(1)), ("count", num(i64::MAX))]),
    ]);
    let error = hist(&overflowing).expect_err("counts summing past u64 must refuse");
    assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
    let diagnostic = error.diagnostic().expect("hist-count-total diagnostic");
    assert!(
        diagnostic.message().contains("u64"),
        "the count-total refusal must name the bound: {}",
        diagnostic.message()
    );
}
