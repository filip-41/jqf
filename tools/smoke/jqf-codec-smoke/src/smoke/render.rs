//! Render-codec receipt battery.
//!
//! Pins the output-only render registration (seven profiles, encode-only), the
//! physical encoder identity, and the byte laws of every renderer driven
//! through the registry's own encoder factory — the same entry the CLI and SDK
//! use.

use crate::drive::resources;
use jqf_codec_core::{CodecRunContext, EncodeItem, EncodeRequest, PreservationRequest};
use jqf_codec_render::{
    FORMAT_ID, GFM_TABLE_DIALECT_ID, GRID_TABLE_DIALECT_ID, HTML_TABLE_DIALECT_ID, HeaderPolicy, PLAIN_DIALECT_ID,
    RenderEncodeOptions, SHELL_DIALECT_ID, TERMINAL_DIALECT_ID, TREE_DIALECT_ID, TerminalShape,
};
use jqf_data::{Array, DialectId, FormatId, Number, ObjectBuilder, ObjectKey, Value};

fn object(entries: &[(&str, Value)]) -> Value {
    let _resources = resources();
    let mut builder = ObjectBuilder::try_with_capacity(entries.len()).expect("builder");
    for (key, value) in entries {
        builder
            .try_insert_last(ObjectKey::try_from_str(key).expect("key"), value.clone())
            .expect("insert");
    }
    Value::Object(builder.try_finish().expect("object"))
}

fn array(values: &[Value]) -> Value {
    let _resources = resources();
    let mut owned = Vec::new();
    for value in values {
        owned.push(value.clone());
    }
    Value::Array(Array::try_from_vec(owned).expect("array"))
}

fn string(text: &str) -> Value {
    let _resources = resources();
    Value::try_string(text).expect("string")
}

fn num(value: i64) -> Value {
    Value::Number(Number::try_json_literal(&value.to_string()).expect("number"))
}

fn encode_with_physical(
    value: &Value,
    dialect: &'static str,
    options: RenderEncodeOptions,
) -> Result<(String, jqf_codec_core::PhysicalRouteId), String> {
    let mut resources = resources();
    let registration = jqf_codec_render::registration().expect("registration");
    let format = FormatId::try_new(FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(dialect).expect("dialect");
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(
            EncodeRequest {
                format: &format,
                dialect: &dialect,
                diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                options: Some(&options as &(dyn core::any::Any + Send + Sync)),
            },
            &mut resources,
        )
        .map_err(|error| format!("create_factory: {:?}", error.kind()))?;
    let mut session = factory
        .start(EncodeItem::Owned(value), PreservationRequest::None, &mut resources)
        .expect("session");
    let physical = session.physical_encoder();
    let mut out = Vec::new();
    {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session
            .encode(&mut sink, &mut run)
            .map_err(|error| format!("encode: {:?}", error.kind()))?;
    }
    Ok((String::from_utf8(out).expect("UTF-8 render output"), physical))
}

fn encode(value: &Value, dialect: &'static str, options: RenderEncodeOptions) -> Result<String, String> {
    encode_with_physical(value, dialect, options).map(|(text, _)| text)
}

/// Encodes one item, returning the RAW codec error for prose inspection.
fn encode_full(
    value: &Value,
    dialect: &'static str,
    options: RenderEncodeOptions,
) -> Result<String, jqf_codec_core::CodecError> {
    let mut resources = resources();
    let registration = jqf_codec_render::registration().expect("registration");
    let format = FormatId::try_new(FORMAT_ID).expect("format");
    let dialect = DialectId::try_new(dialect).expect("dialect");
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

fn expect_reject(value: &Value, dialect: &'static str, options: RenderEncodeOptions) -> Result<(), String> {
    match encode(value, dialect, options) {
        Ok(text) => Err(format!("expected a reject for {dialect}, got output {text:?}")),
        Err(error) if error.contains("UnsupportedRepresentation") => Ok(()),
        Err(other) => Err(format!("expected UnsupportedRepresentation, got {other}")),
    }
}

/// Pins the registration surface: format `render`, eight dialects, encode only.
fn registration_surface() -> Result<(), String> {
    let registration = jqf_codec_render::registration().map_err(|error| format!("{error:?}"))?;
    let descriptor = registration.descriptor();
    if descriptor.format().as_str() != "render" {
        return Err(format!("unexpected format {}", descriptor.format().as_str()));
    }
    let dialects = descriptor.dialects();
    let expected = [
        "render.plain@1",
        "render.gfm-table@1",
        "render.html-table@1",
        "render.grid-table@1",
        "render.tree@1",
        "render.terminal@1",
        "render.shell@1",
        "render.hist@1",
    ];
    if dialects.len() != expected.len()
        || dialects
            .iter()
            .zip(expected)
            .any(|(left, right)| left.as_str() != right)
    {
        return Err(format!(
            "unexpected render dialect set: {}",
            dialects
                .iter()
                .map(|dialect| dialect.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let operations = descriptor.operations();
    if operations.decode() || !operations.encode() || operations.validate_tags() {
        return Err("the render registration must advertise encode only".into());
    }
    if registration.decoder().is_some() || registration.tag_validator().is_some() {
        return Err("the render registration must carry no decoder or tag validator".into());
    }
    Ok(())
}

/// Pins the physical encoder identity through a started session.
fn physical_route() -> Result<(), String> {
    let (_, physical) = encode_with_physical(&num(1), TREE_DIALECT_ID, RenderEncodeOptions::default())?;
    if physical != jqf_codec_render::ENCODE_PHYSICAL_ROUTE_ID {
        return Err(format!("unexpected physical encoder {physical:?}"));
    }
    Ok(())
}

/// Pins the GFM, HTML, and grid byte laws.
fn table_byte_laws() -> Result<(), String> {
    let default = RenderEncodeOptions::default();
    let rows = array(&[
        object(&[("name", string("alice")), ("age", num(30))]),
        object(&[("name", string("bob")), ("age", num(40))]),
    ]);
    let gfm = encode(&rows, GFM_TABLE_DIALECT_ID, default)?;
    if gfm != "| name | age |\n| :--- | ---: |\n| alice | 30 |\n| bob | 40 |" {
        return Err(format!("gfm byte law drifted: {gfm:?}"));
    }
    let escaped = object(&[("a", string("x|y<z"))]);
    let gfm_esc = encode(&escaped, GFM_TABLE_DIALECT_ID, default)?;
    if !gfm_esc.contains("x&#x7C;y") || !gfm_esc.contains("&#x3C;") {
        return Err(format!("gfm must escape | and <: {gfm_esc:?}"));
    }
    let single = object(&[("a", string("x<y&z")), ("n", num(7))]);
    let html = encode(&single, HTML_TABLE_DIALECT_ID, default)?;
    let expected_html = concat!(
        "<table>\n",
        "<thead>\n<tr><th scope=\"col\" style=\"text-align: left; white-space: pre-wrap\">a</th>",
        "<th scope=\"col\" style=\"text-align: right; white-space: pre-wrap\">n</th></tr>\n",
        "</thead>\n",
        "<tbody>\n",
        "<tr><td style=\"text-align: left; white-space: pre-wrap\">x&lt;y&amp;z</td>",
        "<td style=\"text-align: right; white-space: pre-wrap\">7</td></tr>\n",
        "</tbody>\n</table>",
    );
    if html != expected_html {
        return Err(format!("html byte law drifted: {html:?}"));
    }
    let grid = encode(&rows, GRID_TABLE_DIALECT_ID, default)?;
    let expected_grid = concat!(
        "+-------+-----+\n",
        "| name  | age |\n",
        "+-------+-----+\n",
        "| alice |  30 |\n",
        "+-------+-----+\n",
        "| bob   |  40 |\n",
        "+-------+-----+",
    );
    if grid != expected_grid {
        return Err(format!("grid byte law drifted: {grid:?}"));
    }
    Ok(())
}

/// Pins the plain and tree byte laws, including the shared-scalar formatter.
fn plain_and_tree_byte_laws() -> Result<(), String> {
    let default = RenderEncodeOptions::default();
    let plain = encode(&num(42), PLAIN_DIALECT_ID, default)?;
    if plain != "42" {
        return Err(format!("plain integer drifted: {plain:?}"));
    }
    let nan = Value::Number(Number::float(jqf_data::Float::new(f64::NAN)));
    let nan_text = encode(&nan, PLAIN_DIALECT_ID, default)?;
    if nan_text != "nan(0x7ff8000000000000)" {
        return Err(format!("plain NaN law drifted: {nan_text:?}"));
    }
    let value = object(&[("name", string("alice")), ("tags", array(&[string("a"), string("b")]))]);
    let tree = encode(&value, TREE_DIALECT_ID, default)?;
    let expected_tree = concat!(
        "$ = object(2)\n",
        "  $[\"name\"]#0 = \"alice\"\n",
        "  $[\"tags\"]#1 = array(2)\n",
        "    $[\"tags\"]#1[0] = \"a\"\n",
        "    $[\"tags\"]#1[1] = \"b\"",
    );
    if tree != expected_tree {
        return Err(format!("tree byte law drifted: {tree:?}"));
    }
    Ok(())
}

/// Pins the terminal escaping and shape selection.
fn terminal_byte_laws() -> Result<(), String> {
    let options = RenderEncodeOptions {
        terminal_shape: TerminalShape::Plain,
        ..RenderEncodeOptions::default()
    };
    let text = encode(&string("a\tb\\c\nd"), TERMINAL_DIALECT_ID, options)?;
    if text != "a\\tb\\\\c\\nd" {
        return Err(format!("terminal plain escaping drifted: {text:?}"));
    }
    Ok(())
}

/// Pins the shell dialect's flattening, quoting, scalar, and separator laws.
fn shell_byte_laws() -> Result<(), String> {
    let default = RenderEncodeOptions::default();
    // D7 flattening + D6 quoting + D10 scalars, one document.
    let nested = object(&[
        ("a", object(&[("b", num(1))])),
        ("c", array(&[string("x y"), string("O'Hara!")])),
        ("d", array(&[object(&[("e", Value::Null), ("f", Value::Bool(true))])])),
    ]);
    let text = encode(&nested, SHELL_DIALECT_ID, default)?;
    let expected = concat!(
        "a_b=1\n",
        "c_0='x y'\n",
        "c_1='O'\\''Hara!'\n",
        "d_0_e=null\n",
        "d_0_f=true",
    );
    if text != expected {
        return Err(format!("shell flattening law drifted: {text:?}"));
    }
    // Root scalar under `value`, root array under the empty prefix.
    if encode(&num(7), SHELL_DIALECT_ID, default)? != "value=7" {
        return Err("shell root scalar must emit under `value`".into());
    }
    if encode(&string("a b"), SHELL_DIALECT_ID, default)? != "value='a b'" {
        return Err("shell root string must single-quote under `value`".into());
    }
    let root_array = encode(&array(&[num(1), string("x")]), SHELL_DIALECT_ID, default)?;
    if root_array != "_0=1\n_1='x'" {
        return Err(format!("shell root array prefix drifted: {root_array:?}"));
    }
    // D6: an empty string emits `key=`; nothing after the `=`.
    let empty = object(&[("k", string(""))]);
    if encode(&empty, SHELL_DIALECT_ID, default)? != "k=" {
        return Err("shell empty string must emit `k=`".into());
    }
    // D7: the separator is a typed option, default `_`.
    let dash = RenderEncodeOptions {
        shell_separator: "-",
        ..RenderEncodeOptions::default()
    };
    let dashed = encode(&object(&[("a", object(&[("b", num(1))]))]), SHELL_DIALECT_ID, dash)?;
    if dashed != "a-b=1" {
        return Err(format!("shell separator option drifted: {dashed:?}"));
    }
    Ok(())
}

/// Pins the shell dialect's two terminal refusals: a key that is not a valid
/// shell name (D9) and two paths that flatten to the same variable (D8), both
/// with prose naming the path(s) and publishing zero bytes.
fn shell_refusals() -> Result<(), String> {
    let default = RenderEncodeOptions::default();
    let collision = object(&[("a", object(&[("b", num(1))])), ("a_b", num(2))]);
    let error = encode_full(&collision, SHELL_DIALECT_ID, default).expect_err("collision must refuse");
    if error.kind() != jqf_codec_core::CodecFailureKind::UnsupportedRepresentation {
        return Err("shell collision must be UnsupportedRepresentation".into());
    }
    let message = error
        .diagnostic()
        .map(|diagnostic| diagnostic.message().to_owned())
        .unwrap_or_default();
    if !message.contains("\"a_b\"") || !message.contains(".a.b") || !message.contains(".a_b") {
        return Err(format!(
            "shell collision prose must name both paths and the variable: {message:?}"
        ));
    }
    for (key, fragment) in [("1x", ".1x"), ("a-b", ".a-b"), ("a b", ".a b"), ("", ".")] {
        let error =
            encode_full(&object(&[(key, num(1))]), SHELL_DIALECT_ID, default).expect_err("invalid key must refuse");
        let message = error
            .diagnostic()
            .map(|diagnostic| diagnostic.message().to_owned())
            .unwrap_or_default();
        if !message.contains(fragment) || !message.contains("[A-Za-z_][A-Za-z0-9_]*") {
            return Err(format!(
                "shell key refusal prose must name the path and the rule: {message:?}"
            ));
        }
    }
    Ok(())
}

/// The cross-path quoting law: the shell renderer's word quoting is
/// byte-identical to the `@sh` builtin's `push_word`. The same value runs
/// through BOTH paths — the codec registration and the engine builtin — and
/// the published bytes must agree (`value=` + the `@sh` word).
fn shell_quoting_matches_at_sh() -> Result<(), String> {
    let default = RenderEncodeOptions::default();
    // The string half (D6): every quoting-sensitive spelling. The EMPTY
    // string is deliberately absent: D10 renders it bare (`value=`), the one
    // documented divergence from `@sh`'s `''` — pinned below instead.
    let strings = [
        "simple",
        "a b",
        "O'Hara!",
        "'",
        "''",
        "$HOME `ls` \\ *",
        "tab\tnewline\nreturn\r",
        "\u{00e9}\u{4e00}\u{1f600}",
        "a\"b",
    ];
    for text in strings {
        let payload = serde_json::to_string(text).map_err(|error| format!("json: {error}"))?;
        let codec = encode(&string(text), SHELL_DIALECT_ID, default)
            .map_err(|error| format!("shell codec failed for {text:?}: {error}"))?;
        let at_sh = at_sh(&payload).map_err(|error| format!("@sh failed for {text:?}: {error}"))?;
        let expected = format!("value={at_sh}");
        if codec != expected {
            return Err(format!(
                "shell quoting diverges from @sh for {text:?}: codec {codec:?}, @sh {at_sh:?}"
            ));
        }
    }
    // The one documented D10 divergence: an empty string renders bare
    // (`value=`) where `@sh` writes `''`.
    let codec = encode(&string(""), SHELL_DIALECT_ID, default)?;
    if codec != "value=" {
        return Err(format!("shell empty string must render bare `value=`: {codec:?}"));
    }
    let at_sh_empty = at_sh("\"\"").map_err(|error| format!("@sh failed for empty string: {error}"))?;
    if at_sh_empty != "''" {
        return Err(format!("@sh empty-string spelling drifted: {at_sh_empty:?}"));
    }
    // The scalar half (D10): numbers, booleans, and null render their JSON
    // text exactly as `@sh` spells them.
    let scalars = ["null", "true", "false", "1", "-7", "1.5", "1e3", "0.1"];
    for payload in scalars {
        let value = json_scalar(payload);
        let codec = encode(&value, SHELL_DIALECT_ID, default)
            .map_err(|error| format!("shell codec failed for {payload:?}: {error}"))?;
        let at_sh = at_sh(payload).map_err(|error| format!("@sh failed for {payload:?}: {error}"))?;
        let expected = format!("value={at_sh}");
        if codec != expected {
            return Err(format!(
                "shell scalar {payload:?} diverges from @sh: codec {codec:?}, @sh {at_sh:?}"
            ));
        }
    }
    Ok(())
}

/// Builds one scalar value from its JSON literal spelling.
fn json_scalar(payload: &str) -> Value {
    match payload {
        "null" => Value::Null,
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        number => Value::Number(Number::try_json_literal(number).expect("number")),
    }
}

/// Drives one JSON-encoded value through the `@sh` builtin via the real SDK
/// pipeline and returns its exact bytes.
fn at_sh(payload: &str) -> Result<String, String> {
    let mut resources = resources();
    let json = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let registrations = [&json];
    let catalog = jqf_sdk::CodecCatalog::new(&registrations);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| error.to_string())?;
    let dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| error.to_string())?;
    let output_format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| error.to_string())?;
    let output_dialect = DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| error.to_string())?;
    // The decode request borrows its dialect; a separate instance keeps the
    // moved input/output identities apart (mirrors the json_seq smoke).
    let request_dialect: &'static DialectId = Box::leak(Box::new(
        DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
    ));
    let policy = jqf_engine::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let program =
        jqf_engine::try_compile_program("@sh", policy, &resources).map_err(|error| format!("compile @sh: {error}"))?;
    let requirement = program
        .try_requirement(&resources)
        .map_err(|error| format!("requirement: {:?}", error.kind()))?;
    let mut sink = ByteSink { bytes: Vec::new() };
    let raw = jqf_codec_json::JsonEncodeOptions {
        indent: jqf_codec_json::JsonIndent::Compact,
        raw_strings: true,
        ..jqf_codec_json::JsonEncodeOptions::default()
    };
    let request = jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(payload.as_bytes()))
        .with_catalog(catalog)
        .with_source(crate::drive::source(payload.as_bytes()))
        .with_format(format, dialect)
        .with_output_format(output_format, output_dialect)
        .with_policy(jqf_sdk::PipelinePolicy {
            decode: jqf_codec_core::DecodeRequest {
                validation: jqf_codec_core::ValidationMode::Strict,
                diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
                dialect: request_dialect,
                options: None,
                allow_adjacent_values: true,
                value_separator: &[],
            },
            encode_diagnostics: jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::None,
            encode_options: Some(&raw as &(dyn core::any::Any + Send + Sync)),
            cooperative_credits: 4_096,
            split: None,

            max_iterations: None,
        })
        .with_framing(jqf_sdk::FacadeFraming::item_suffix(b""))
        .with_resources(&mut resources)
        .with_requirement(&requirement);
    jqf_sdk::execute(request, &mut sink).map_err(|error| format!("@sh run: {:?}", error.pipeline_failure()))?;
    String::from_utf8(sink.bytes).map_err(|error| format!("@sh output not UTF-8: {error}"))
}

/// A sink that collects every published byte, exactly.
struct ByteSink {
    bytes: Vec<u8>,
}

impl jqf_sdk::ItemSink for ByteSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn finish_item(&mut self, _index: u64, _report: jqf_sdk::EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Pins the composition-law rejections: GFM without a present header, and the
/// shape/layout cap failures.
fn composition_rejects() -> Result<(), String> {
    let no_header = RenderEncodeOptions {
        header: HeaderPolicy::Absent,
        ..RenderEncodeOptions::default()
    };
    expect_reject(&array(&[object(&[("a", num(1))])]), GFM_TABLE_DIALECT_ID, no_header)?;
    let too_many_rows = RenderEncodeOptions {
        sample_rows: 1,
        ..RenderEncodeOptions::default()
    };
    expect_reject(
        &array(&[object(&[("a", num(1))]), object(&[("a", num(2))])]),
        GRID_TABLE_DIALECT_ID,
        too_many_rows,
    )?;
    let narrow = RenderEncodeOptions {
        max_width: 1,
        ..RenderEncodeOptions::default()
    };
    expect_reject(
        &array(&[object(&[("a", string("\u{4e00}"))])]),
        GRID_TABLE_DIALECT_ID,
        narrow,
    )?;
    let plain_shape = RenderEncodeOptions {
        terminal_shape: TerminalShape::Plain,
        ..RenderEncodeOptions::default()
    };
    expect_reject(&object(&[("a", num(1))]), TERMINAL_DIALECT_ID, plain_shape)?;
    expect_reject(&num(1), GFM_TABLE_DIALECT_ID, RenderEncodeOptions::default())?;
    Ok(())
}

pub fn run() -> Result<(), String> {
    let results = [
        ("registration surface", registration_surface()),
        ("physical route", physical_route()),
        ("table byte laws", table_byte_laws()),
        ("plain and tree byte laws", plain_and_tree_byte_laws()),
        ("terminal byte laws", terminal_byte_laws()),
        ("shell byte laws", shell_byte_laws()),
        ("shell refusals", shell_refusals()),
        ("shell quoting matches @sh", shell_quoting_matches_at_sh()),
        ("composition rejects", composition_rejects()),
    ];
    let mut failures = 0;
    for (label, result) in results {
        match result {
            Ok(()) => println!("render-smoke: {label}: ok"),
            Err(error) => {
                failures += 1;
                println!("render-smoke: {label}: FAIL: {error}");
            }
        }
    }
    if failures != 0 {
        return Err(format!("{failures} receipt(s) failed"));
    }
    println!("render-smoke: all receipts pass");
    Ok(())
}
