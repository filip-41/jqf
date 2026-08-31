//! The WebAssembly binding for jqf: run programs in a browser over the same SDK the CLI drives.
//!
//! The exports are `wasm-bindgen` functions. `bindings/wasm/jqf.js` owns option defaults, text encoding, and envelope
//! parsing so a page calls `loadJqf()` then `run`.
//!
//! # Session model
//!
//! `jqf_run` lazily builds ONE session — codec registrations, catalog, diagnostics buffer, resource context — leaked
//! for the instance's lifetime, exactly like the FFI handle. Every later call runs against it. The instance is
//! single-threaded; the session lives in a thread-local.
//!
//! # The run entry point
//!
//! `jqf_run` takes the program, the input bytes, input/output format names, an indent width (`-1` = tabs, `0` =
//! compact, `1..=7` = spaces per level; anything else is refused), a flags bitmask, and a slurp flag, and returns the
//! result ENVELOPE as a JSON string — never fails across the boundary:
//!
//! ```text
//! {"ok":true,"output":"...","value_errors":[],"records":[...]}
//! {"ok":false,"output":"partial...","error":"...","records":[...]}
//! ```
//!
//! `output` carries the published bytes as a JSON string, or `"binary":true` plus `output_base64` when they were not
//! valid UTF-8 (`output` is then null, never a lossy string). Records come from the SDK's diagnostic stream
//! (`record_json`), so the JS side never re-renders a failure class itself.
//!
//! # Scope (deliberate)
//!
//! - One-shot runs: compile + run per call. A browser demo's per-keystroke cost is dominated by the UI, not the
//!   compile.
//! - The whole-document/input-sequence routes plus the record drive: no feed, edit, diff, or follow. Those are
//!   CLI/host-drive surfaces.
//! - No deadline: `Instant::now()` is unavailable on wasm32-unknown-unknown, so nothing on this path may consult
//!   wall-clock time. Runaway programs are bounded instead by the ledger's memory/output ceilings and the engine's own
//!   depth guards.
use std::cell::RefCell;

use jqf_codec_core::{
    AccessRequirement, CodecRegistration, DecodeRequest, DiagnosticPolicy, ItemByteOwner, PreservationRequest,
    RouteCapability, ValidationMode,
};
use jqf_codec_json::{JsonEncodeOptions, JsonIndent};
use jqf_data::{DialectId, FormatId};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, try_compile_program};
use jqf_resource::{ContinueControl, EnvironmentSnapshot, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_sdk::{
    CodecCatalog, Diagnostics, FacadeFraming, Input, ItemSink, PipelineFailure, PipelinePolicy, Request, record_json,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};
use wasm_bindgen::prelude::*;

/// The ABI version. The JS glue checks this at load and refuses a mismatch.
pub const ABI_VERSION: u32 = 1;

/// Ledger ceilings sized for a browser tab: generous enough that no honest request trips them, tight enough that a
/// runaway program fails with a ledger refusal instead of OOM-trapping the instance.
const MAX_MEMORY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SPILL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_NESTING_DEPTH: u32 = 1_000;

/// The compile policy every run uses (the FFI's law): strict validation, errors-only diagnostics, no explicit
/// container-span frontier.
const POLICY: CodecRequirementPolicy =
    CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

struct Session {
    resources: ResourceContext<'static>,
    diagnostics: &'static Diagnostics,
    catalog: CodecCatalog<'static, 'static>,
    /// The six-registration record catalog the record drives serve (json, ndjson, json-seq, csv, tsv, render) —
    /// installed once, exactly like the FFI handle's.
    record_catalog: CodecCatalog<'static, 'static>,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// Leaks one registration forever. Called once per format at init; the FFI handle leaks the same way (a registration
/// must outlive its borrowed catalog, and the session lives as long as the instance).
fn leak(
    registration: Result<CodecRegistration<'static>, jqf_codec_core::RegistrationError>,
) -> &'static CodecRegistration<'static> {
    let registration = registration.unwrap_or_else(|error| panic!("invalid built-in registration: {error:?}"));
    Box::leak(Box::new(registration))
}

fn build_session() -> Session {
    // One line per codec, exactly like the CLI's loader: which formats a build understands is a dependency edge, not a
    // feature flag.
    let json = leak(jqf_codec_json::registration());
    let jsonc = leak(jqf_codec_json::jsonc::registration());
    let five = leak(jqf_codec_json::json5::registration());
    let ndjson = leak(jqf_codec_json::ndjson::registration());
    let json_seq = leak(jqf_codec_json::seq::registration());
    let toml_10 = leak(jqf_codec_toml::registration_1_0());
    let toml_11 = leak(jqf_codec_toml::registration_1_1());
    let csv = leak(jqf_codec_delimited::registration());
    let tsv = leak(jqf_codec_delimited::registration_tsv());
    let cbor = leak(jqf_codec_cbor::registration());
    let yaml = leak(jqf_codec_yaml::registration());
    let xml = leak(jqf_codec_xml::registration());
    let html = leak(jqf_codec_html::registration());
    let html_fragment = leak(jqf_codec_html::registration_fragment());
    let properties = leak(jqf_codec_ini::registration());
    let ini = leak(jqf_codec_ini::registration_ini());
    let dotenv = leak(jqf_codec_ini::registration_dotenv());
    let messagepack = leak(jqf_codec_messagepack::registration());

    let registrations: &'static [&'static CodecRegistration<'static>] = Box::leak(Box::new(vec![
        json,
        jsonc,
        five,
        ndjson,
        json_seq,
        toml_10,
        toml_11,
        csv,
        tsv,
        cbor,
        yaml,
        xml,
        html,
        html_fragment,
        properties,
        ini,
        dotenv,
        messagepack,
    ]));
    let catalog = CodecCatalog::new(registrations);

    let account = RequestAccount::try_new(ResourceLimits::new(
        // No input ceiling: the retained input is what every route borrows.
        u64::MAX,
        MAX_OUTPUT_BYTES,
        MAX_MEMORY_BYTES,
        MAX_SPILL_BYTES,
        MAX_NESTING_DEPTH,
    ))
    .expect("the wasm ledger ceilings are constructible");
    let work = WorkMeter::try_new_v1(64).expect("the wasm work meter is valid");
    let control: &'static ContinueControl = Box::leak(Box::new(ContinueControl));
    let resources = ResourceContext::new(account, control, work).expect("the wasm resource context is constructible");
    let diagnostics = Box::leak(Box::new(
        Diagnostics::new(DiagnosticPolicy::All).expect("the wasm diagnostics buffer is valid"),
    ));
    let resources = resources.with_diagnostics(diagnostics);
    let resources = resources.with_environment(EnvironmentSnapshot::new(
        Vec::new(),
        Some(String::from("/")),
        Vec::new(),
        None,
    ));
    Session {
        resources,
        diagnostics,
        catalog,
        record_catalog: jqf_runtime::records::install_record_catalog(
            jqf_codec_json::registration().expect("the strict-JSON registration is static"),
            jqf_codec_json::ndjson::registration().expect("the NDJSON registration is static"),
            jqf_codec_json::seq::registration().expect("the json-seq registration is static"),
            jqf_codec_delimited::registration().expect("the CSV registration is static"),
            jqf_codec_delimited::registration_tsv().expect("the TSV registration is static"),
            jqf_codec_render::registration().expect("the render registration is static"),
            jqf_codec_yaml::registration().expect("the YAML registration is static"),
            jqf_codec_xml::registration().expect("the XML registration is static"),
            jqf_codec_html::registration().expect("the HTML registration is static"),
        ),
    }
}

/// Runs `body` with the session, building it first if needed.
fn with_session<R>(body: impl FnOnce(&mut Session) -> R) -> R {
    SESSION.with(|session| {
        let mut slot = session.borrow_mut();
        if slot.is_none() {
            *slot = Some(build_session());
        }
        body(slot.as_mut().expect("session just built"))
    })
}

// ---------------------------------------------------------------------------
// Format table: friendly name -> format + default dialects
// ---------------------------------------------------------------------------

struct FormatEntry {
    name: &'static str,
    format: &'static str,
    /// The default INPUT dialect (what `--input-format <name>` selects).
    input_dialect: &'static str,
    /// The default OUTPUT dialect (what `--output-format <name>` selects).
    output_dialect: &'static str,
    /// Whether this family is physically framed into records: its input side goes through the record drive, never the
    /// value ladder.
    record_input: bool,
    /// Whether the format can DECODE at all (`false` for output-only formats).
    decodable: bool,
}

impl FormatEntry {
    const fn doc(name: &'static str, input_dialect: &'static str, output_dialect: &'static str) -> Self {
        Self {
            name,
            format: name,
            input_dialect,
            output_dialect,
            record_input: false,
            decodable: true,
        }
    }

    /// A record-framed family: decodable only through the record drive.
    const fn framed(
        name: &'static str,
        format: &'static str,
        input_dialect: &'static str,
        output_dialect: &'static str,
    ) -> Self {
        Self {
            name,
            format,
            input_dialect,
            output_dialect,
            record_input: true,
            decodable: true,
        }
    }
}

const FORMATS: &[FormatEntry] = &[
    FormatEntry::doc("json", "rfc8259", "rfc8259"),
    FormatEntry::doc("jsonc", "jsonc.trailing@1", "jsonc.trailing-jqf@1"),
    FormatEntry::doc("json5", "json5.document@1", "json5.jqf@1"),
    FormatEntry::framed("ndjson", "ndjson", "ndjson.strict@1", "ndjson.strict@1"),
    FormatEntry::framed("json-seq", "json-seq", "json-seq.strict@1", "json-seq.jqf@1"),
    FormatEntry::doc("toml", "toml-1.0", "toml.jqf-1.0@1"),
    FormatEntry::framed("csv", "csv", "csv.utf8@1", "csv.jqf-utf8@1"),
    FormatEntry::framed("tsv", "tsv", "tsv.utf8@1", "tsv.jqf-lf@1"),
    FormatEntry::framed("csv-header", "csv", "csv.utf8-header@1", "csv.jqf-utf8-header@1"),
    FormatEntry::framed("tsv-header", "tsv", "tsv.utf8-header@1", "tsv.jqf-lf-header@1"),
    FormatEntry::doc("cbor", "cbor.rfc8949-generic@1", "cbor.preferred@1"),
    FormatEntry::doc("yaml", "yaml.core@1", "yaml.block@1"),
    FormatEntry::doc("xml", "xml.document@1", "xml.source@1"),
    FormatEntry::doc("html", "html.document@1", "html.document-serialize@1"),
    FormatEntry::doc("properties", "properties.jdk@1", "properties.jqf-1.0@1"),
    FormatEntry::doc("ini", "ini.jqf-strict@1", "ini.jqf-1.0@1"),
    FormatEntry::doc("dotenv", "dotenv.jqf-strict@1", "dotenv.jqf-1.0@1"),
    FormatEntry::doc("messagepack", "messagepack.utf8@1", "messagepack.deterministic@1"),
];

fn find_format(name: &str) -> Option<&'static FormatEntry> {
    FORMATS.iter().find(|entry| entry.name == name)
}

/// Whether the input dialect is an adjacent-values stream or exactly one document. Mirrors the CLI's law: the fact
/// comes from the catalog's route capabilities, never a per-format list.
fn adjacent_values_input(session: &Session, entry: &FormatEntry) -> bool {
    let Ok(format_id) = FormatId::try_new(entry.format) else {
        return false;
    };
    let Ok(dialect_id) = DialectId::try_new(entry.input_dialect) else {
        return false;
    };
    session
        .catalog
        .route_capabilities(&format_id, &dialect_id)
        .is_ok_and(|caps| caps.contains(&RouteCapability::AdjacentValues))
}

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// Publish string values without quotes (the reference's `-r`).
pub const FLAG_RAW_STRINGS: u32 = 1;
/// Sort object keys (`-S`).
pub const FLAG_SORT_KEYS: u32 = 2;
/// Escape non-ASCII (`-a`).
pub const FLAG_ASCII: u32 = 4;
/// Do not read the input; run the program once over `null` (`-n`). Beats slurp, exactly as in the CLI (the reference's
/// own precedence).
pub const FLAG_NULL_INPUT: u32 = 8;

// ---------------------------------------------------------------------------
// Envelope building
// ---------------------------------------------------------------------------

/// Escapes one string as a JSON string literal body (no surrounding quotes). The law is
/// [`jqf_codec_json::push_json_escaped`]: quote, backslash, C0, and DEL `0x7F`.
fn json_escape(body: &str) -> String {
    let mut out = Vec::with_capacity(body.len());
    jqf_codec_json::push_json_escaped(&mut out, body.as_bytes());
    String::from_utf8(out).expect("JSON string escape is UTF-8")
}

/// Standard base64, for binary published output.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The sink every run publishes into: an unbounded-but-ledger-accounted `Vec`. The ledger's output ceiling refuses
/// before capacity growth can become a tab problem, so the Vec stays inside the committed bound.
struct VecSink {
    out: Vec<u8>,
    errors: Vec<String>,
}

impl ItemSink for VecSink {
    type Error = &'static str;
    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        self.out.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn finish_item(&mut self, _index: u64, _report: jqf_sdk::EncodedItemReport) -> Result<(), Self::Error> {
        Ok(())
    }
    fn report_value_error(&mut self, error: jqf_sdk::SequenceValueError) -> Result<(), Self::Error> {
        self.errors.push(error.message().to_owned());
        Ok(())
    }
}

/// A minimal failure envelope (setup failures: no run happened).
fn setup_envelope(message: &str) -> String {
    format!(
        "{{\"ok\":false,\"output\":\"\",\"error\":\"{}\",\"value_errors\":[],\"records\":[]}}",
        json_escape(message),
    )
}

/// Appends `"output":"..."` / the base64 twin for the published bytes.
fn push_output_field(envelope: &mut String, out: &[u8]) {
    if let Ok(text) = std::str::from_utf8(out) {
        envelope.push_str(",\"output\":\"");
        envelope.push_str(&json_escape(text));
        envelope.push('"');
    } else {
        envelope.push_str(",\"output\":null,\"output_base64\":\"");
        envelope.push_str(&base64(out));
        envelope.push_str("\",\"binary\":true");
    }
}

/// Appends the per-value error list body (no brackets).
fn push_value_errors(envelope: &mut String, errors: &[String]) {
    for (i, error) in errors.iter().enumerate() {
        if i > 0 {
            envelope.push(',');
        }
        envelope.push('"');
        envelope.push_str(&json_escape(error));
        envelope.push('"');
    }
}

/// Appends `"records":[...]` from the diagnostic stream's records.
fn push_records(envelope: &mut String, diagnostics: &Diagnostics) {
    envelope.push_str(",\"records\":[");
    for (i, record) in diagnostics.records().iter().map(record_json).enumerate() {
        if i > 0 {
            envelope.push(',');
        }
        envelope.push_str(&record);
    }
    envelope.push(']');
}

/// Parses the indent argument under jq's own law (`--indent`): negative is the tab spelling, 0 is compact, 1..=7 is
/// spaces per level. Out of range is REFUSED with the CLI's message — never silently clamped, which would publish bytes
/// the caller did not ask for.
fn parse_indent(indent: i32) -> Result<JsonIndent, String> {
    match indent {
        -1 => Ok(JsonIndent::Tabs),
        0 => Ok(JsonIndent::Compact),
        width @ 1..=7 => Ok(JsonIndent::Spaces(u8::try_from(width).expect("1..=7 fits u8"))),
        _ => Err(String::from("--indent takes a number between -1 and 7")),
    }
}

// ---------------------------------------------------------------------------
// The run itself
// ---------------------------------------------------------------------------

/// The record drive's per-record ceiling for browser runs: no honest request approaches it, and a corrupt stream cannot
/// balloon one record past a bounded refusal.
const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;

/// Runs one program over a RECORD-FRAMED input (ndjson/json-seq/csv/tsv) through the SDK's own record drive, planned
/// SERIAL — a browser tab has no worker pool to offer, and serial is byte-identical to every width.
#[allow(
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "one linear record path; every argument is a distinct facet of one request"
)]
fn run_record(
    session: &mut Session,
    compiled: &jqf_engine::CompiledProgram,
    input_bytes: &[u8],
    input_entry: &FormatEntry,
    output_entry: &FormatEntry,
    indent: JsonIndent,
    flags: u32,
    slurp: bool,
) -> String {
    use jqf_codec_json::ndjson::NdjsonProfile;
    use jqf_codec_json::seq::JsonSeqProfile;
    use jqf_runtime::parallel::{ParallelPlan, PlanDecision, WorkerRequest};
    use jqf_runtime::records::{
        OutputTarget, RecordDriveSpec, RecordInputKind, RecordOutputSpec, RecordRunModel, execute_record_request,
    };

    let json_options = JsonEncodeOptions {
        indent,
        raw_strings: flags & FLAG_RAW_STRINGS != 0,
        sort_keys: flags & FLAG_SORT_KEYS != 0,
        ascii_output: flags & FLAG_ASCII != 0,
        raw_output_nul: false,
    };

    // Output targets the record drive serves. Anything else is a setup refusal naming the supported set, never a silent
    // JSON fallback.
    let target = match output_entry.name {
        "json" => OutputTarget::Json,
        "ndjson" => OutputTarget::Ndjson,
        "json-seq" => OutputTarget::JsonSeq,
        "csv" => OutputTarget::Csv {
            header: false,
            utf8: true,
        },
        "csv-header" => OutputTarget::Csv {
            header: true,
            utf8: true,
        },
        "tsv" => OutputTarget::Tsv { header: false },
        "tsv-header" => OutputTarget::Tsv { header: true },
        // YAML output: the block profile (the binding's registered yaml out dialect). The drive plans serial —
        // is_stateful() fences it — so the block profile's factory-carried `---` separators are correct.
        "yaml" => OutputTarget::Yaml {
            dialect: jqf_codec_yaml::YAML_BLOCK_DIALECT_ID,
        },
        "xml" => OutputTarget::Markup {
            format: jqf_codec_xml::FORMAT_ID,
            // The DETERMINISTIC profile, exactly the CLI's default xml output dialect: xml.source@1 echoes an unchanged
            // document's retained source spans, which for a JSON/CSV record is the payload itself.
            dialect: jqf_codec_xml::XML_DETERMINISTIC_DIALECT_ID,
        },
        "html" => OutputTarget::Markup {
            format: jqf_codec_html::FORMAT_ID,
            dialect: jqf_codec_html::HTML_DOCUMENT_SERIALIZE_DIALECT_ID,
        },
        other => {
            let message = format!(
                "record inputs publish only json/ndjson/json-seq/csv/tsv/yaml/xml/html outputs; requested {other:?}"
            );
            session.diagnostics.record_setup_failure(&message);
            return setup_envelope(&message);
        }
    };

    // The CSV spec fields follow the dialect table: comma for the csv entries, TAB for every tsv entry (the delimiter
    // travels on the spec regardless of header). textdata stays false: the utf8 dialects are what this binding
    // registers, not the RFC 4180 ASCII opt-ins.
    let (input_kind, profile, seq_profile, csv_delimiter, csv_textdata) = match input_entry.name {
        "ndjson" => (
            RecordInputKind::Ndjson,
            NdjsonProfile::Strict,
            JsonSeqProfile::Strict,
            None,
            false,
        ),
        "json-seq" => (
            RecordInputKind::JsonSeq,
            NdjsonProfile::Strict,
            JsonSeqProfile::Strict,
            None,
            false,
        ),
        "csv" | "csv-header" => (
            RecordInputKind::Csv {
                header: input_entry.name.ends_with("header"),
                tsv: false,
            },
            NdjsonProfile::Strict,
            JsonSeqProfile::Strict,
            None,
            false,
        ),
        "tsv" | "tsv-header" => (
            RecordInputKind::Csv {
                header: input_entry.name.ends_with("header"),
                tsv: true,
            },
            NdjsonProfile::Strict,
            JsonSeqProfile::Strict,
            Some(b'\t'),
            false,
        ),
        other => {
            let message = format!("{other:?} is not a record-framed input");
            session.diagnostics.record_setup_failure(&message);
            return setup_envelope(&message);
        }
    };

    // The reference's own precedence over a record stream, unchanged from the CLI's plan: `-n` beats `-s`.
    let model = if flags & FLAG_NULL_INPUT != 0 {
        RecordRunModel::NullFirst
    } else if slurp {
        RecordRunModel::Slurped
    } else {
        RecordRunModel::PerRecord
    };

    let spec = RecordDriveSpec {
        input: input_bytes,
        source_name: "<wasm>",
        files: None,
        kind: input_kind,
        profile,
        json_seq_profile: seq_profile,
        csv_delimiter,
        csv_textdata,
        max_record_bytes: MAX_RECORD_BYTES,
        max_iterations: None,
        catalog: session.record_catalog,
        output: RecordOutputSpec {
            target,
            terminator: jqf_codec_json::ndjson::NdjsonTerminator::Lf,
            json: json_options,
            no_newline: false,
        },
        model,
        edit: false,
        cooperative_credits: 64,
    };
    let plan = ParallelPlan::serial(
        WorkerRequest::Auto,
        PlanDecision::SingleRunModel,
        input_bytes.len() as u64,
    );

    let mut sink = VecSink {
        out: Vec::new(),
        errors: Vec::new(),
    };
    let outcome = execute_record_request(spec, plan, compiled, &mut session.resources, &mut sink, None);

    let report_summary;
    match outcome {
        Ok(report) => {
            report_summary = format!(",\"records\":{},\"issues\":{}", report.records(), report.issues());
            session.diagnostics.record_route_named("wasm-record-run");
        }
        Err(error) => {
            let message = match &error {
                jqf_runtime::records::RecordDriveError::Setup { step, error } => {
                    format!("record drive setup at {step}: {:?}", error.kind())
                }
                jqf_runtime::records::RecordDriveError::Pipeline(failure) => {
                    if let Some(pipeline) = failure.pipeline_failure() {
                        session.diagnostics.record_failure(pipeline);
                    }
                    failure.to_string()
                }
                jqf_runtime::records::RecordDriveError::Sink(_) => "sink refused".to_owned(),
                jqf_runtime::records::RecordDriveError::Resource(error) => {
                    format!("resource limit: {error}")
                }
                jqf_runtime::records::RecordDriveError::Control(error) => {
                    format!("control: {error:?}")
                }
            };
            session.diagnostics.record_setup_failure(&message);
            let mut envelope = String::with_capacity(128 + sink.out.len() / 2);
            envelope.push_str("{\"ok\":false");
            push_output_field(&mut envelope, &sink.out);
            envelope.push_str(",\"error\":\"");
            envelope.push_str(&json_escape(&message));
            envelope.push_str("\",\"value_errors\":[");
            push_value_errors(&mut envelope, &sink.errors);
            envelope.push(']');
            push_records(&mut envelope, session.diagnostics);
            envelope.push('}');
            return envelope;
        }
    }

    let mut envelope = String::with_capacity(sink.out.len() / 2 + 512);
    envelope.push_str("{\"ok\":true");
    push_output_field(&mut envelope, &sink.out);
    envelope.push_str(&report_summary);
    envelope.push_str(",\"value_errors\":[");
    push_value_errors(&mut envelope, &sink.errors);
    envelope.push(']');
    push_records(&mut envelope, session.diagnostics);
    envelope.push('}');
    envelope
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear run path: compile -> requirement -> policy -> execute -> envelope"
)]
fn run(
    program: &str,
    input_bytes: &[u8],
    input_entry: &'static FormatEntry,
    output_entry: &'static FormatEntry,
    indent: i32,
    flags: u32,
    slurp: bool,
) -> String {
    with_session(|session| {
        session.diagnostics.clear();

        // Compile. $ARGS binds empty, like the FFI's plain jqf_compile.
        let args: Vec<(String, jqf_data::Value)> = Vec::new();
        let compiled = match try_compile_program(
            program,
            POLICY,
            CompileOptions {
                cli_vars: &args,
                split_exp: false,
                ..Default::default()
            },
            &session.resources,
        ) {
            Ok(compiled) => compiled,
            Err(error) => {
                let message = format!("{error}");
                session.diagnostics.record_setup_failure(&message);
                return setup_envelope(&message);
            }
        };

        // The indent law is checked BEFORE any decode work: an out-of-range width is a usage error about the arguments,
        // not a run failure.
        let indent = match parse_indent(indent) {
            Ok(indent) => indent,
            Err(message) => {
                session.diagnostics.record_setup_failure(&message);
                return setup_envelope(&message);
            }
        };

        // The record-framed families (ndjson/json-seq/csv/tsv) decode ONLY through the record drive — the never-infer
        // law's explicit cousin: serving "ndjson" through the adjacent-value ladder would silently repair framing and
        // make the dialect unanswerable.
        debug_assert!(input_entry.decodable || input_entry.record_input);
        if input_entry.record_input {
            return run_record(
                session,
                &compiled,
                input_bytes,
                input_entry,
                output_entry,
                indent,
                flags,
                slurp,
            );
        }

        // Requirement: the program's own lowered demand plus the fact intent the CLI's serial tail computes (preserve
        // when the program reads facts).
        let requirement = match compiled.try_requirement(&session.resources) {
            Ok(requirement) => requirement,
            Err(error) => {
                let message = format!("cannot lower program requirement: {}", error.kind());
                session.diagnostics.record_setup_failure(&message);
                return setup_envelope(&message);
            }
        };
        let requirement: AccessRequirement = requirement.with_fact_intent(if compiled.accesses_facts() {
            jqf_codec_core::FactIntent::Preserve
        } else {
            jqf_codec_core::FactIntent::None
        });

        let single_document_input = !adjacent_values_input(session, input_entry);

        // Encode options (the CLI's typed-options channel): every arm hands the codec its own concrete options struct
        // as `&dyn Any`; formats with no v1 options channel carry none.
        let json_options = JsonEncodeOptions {
            indent,
            raw_strings: flags & FLAG_RAW_STRINGS != 0,
            sort_keys: flags & FLAG_SORT_KEYS != 0,
            ascii_output: flags & FLAG_ASCII != 0,
            raw_output_nul: false,
        };
        let trailing_style = jqf_codec_json::jsonc::JsoncEncodeOptions {
            style: json_options,
            profile: jqf_codec_json::jsonc::JsoncEncodeProfile::Trailing,
        };
        let seq_options =
            jqf_codec_json::seq::JsonSeqEncodeOptions::new(json_options, jqf_codec_json::seq::JsonSeqSuffix::Lf);
        let ndjson_options =
            jqf_codec_json::ndjson::NdjsonEncodeOptions::new(jqf_codec_json::ndjson::NdjsonTerminator::Lf);
        let csv_options = jqf_codec_delimited::CsvEncodeOptions::try_new(None).ok();
        let tsv_options = jqf_codec_delimited::CsvEncodeOptions::try_new_tsv().ok();

        let options: Option<&(dyn core::any::Any + Send + Sync)> = match output_entry.name {
            "json" | "json5" => Some(&json_options),
            "jsonc" => Some(&trailing_style),
            "json-seq" => Some(&seq_options),
            "ndjson" => Some(&ndjson_options),
            "csv" => csv_options
                .as_ref()
                .map(|options| options as &(dyn core::any::Any + Send + Sync)),
            "tsv" => tsv_options
                .as_ref()
                .map(|options| options as &(dyn core::any::Any + Send + Sync)),
            "yaml" => Some(&jqf_codec_yaml::YamlTargetSchema::Core),
            _ => None,
        };

        let (Ok(input_format), Ok(input_dialect), Ok(output_format), Ok(output_dialect)) = (
            FormatId::try_new(input_entry.format),
            DialectId::try_new(input_entry.input_dialect),
            FormatId::try_new(output_entry.format),
            DialectId::try_new(output_entry.output_dialect),
        ) else {
            let message = String::from("invalid built-in format identity");
            session.diagnostics.record_setup_failure(&message);
            return setup_envelope(&message);
        };

        // Facade framing: JSON keeps the facade newline; every other format asks the catalog who owns the inter-item
        // byte.
        let framing = if output_entry.format == "json" {
            FacadeFraming::item_suffix(b"\n")
        } else {
            match session.catalog.item_byte_owner(&output_format, &output_dialect) {
                Ok(ItemByteOwner::Facade) => FacadeFraming::item_suffix(b"\n"),
                Ok(ItemByteOwner::Codec) => FacadeFraming::item_suffix(b""),
                Err(error) => {
                    let message = format!("cannot resolve item byte owner: {error:?}");
                    session.diagnostics.record_setup_failure(&message);
                    return setup_envelope(&message);
                }
            }
        };

        // The policy borrows the input dialect while the request consumes its own owned copy; DialectId is
        // Clone-not-Copy.
        let policy_dialect = input_dialect.clone();
        let policy = PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &policy_dialect,
                options: None,
                allow_adjacent_values: !single_document_input,
                value_separator: match session.catalog.value_separators(&input_format, &input_dialect) {
                    Ok(separators) => separators,
                    Err(error) => {
                        let message = format!("cannot resolve value separators: {error:?}");
                        session.diagnostics.record_setup_failure(&message);
                        return setup_envelope(&message);
                    }
                },
            },
            encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::Report,
            encode_options: options,
            cooperative_credits: 64,
            split: None,
            max_iterations: None,
        };

        let source = SourceRef::new(SourceId::new(1), SourceKind::Input);
        let resolved_source = ResolvedSource::new(source, "<wasm>", input_bytes, 0);

        let null_input = flags & FLAG_NULL_INPUT != 0;
        let mut request = Request::new(&compiled, Input::Whole(input_bytes))
            .with_catalog(session.catalog)
            .with_source(resolved_source)
            .with_format(input_format, input_dialect)
            .with_output_format(output_format, output_dialect)
            .with_policy(policy)
            .with_framing(framing);
        // The CLI's exclusivity law: `-n` beats `-s`; an input-family program attaches the cursor only when it reads
        // values (never under `-n`, where `input` raises break by definition).
        if null_input {
            request = request.with_null_input();
        } else if slurp {
            request = request.slurped();
        } else if compiled.uses_input_family() && !single_document_input {
            request = request.with_input_family();
        }
        let request = request
            .with_resources(&mut session.resources)
            .with_requirement(&requirement);

        let mut sink = VecSink {
            out: Vec::new(),
            errors: Vec::new(),
        };
        let outcome = jqf_sdk::execute(request, &mut sink);

        // Halt status extraction (the FFI's law): the RAISE_HALT terminal failure carries the process exit status the
        // host reports.
        let mut halt_status: Option<u32> = None;
        if let Err(failure) = &outcome
            && let Some(pipeline) = failure.pipeline_failure()
        {
            if let PipelineFailure::Halt { status, .. } = pipeline {
                halt_status = Some(*status);
            }
            session.diagnostics.record_failure(pipeline);
        }

        let mut envelope = String::with_capacity(sink.out.len() / 2 + 512);
        envelope.push_str("{\"ok\":");
        envelope.push_str(if outcome.is_ok() { "true" } else { "false" });

        push_output_field(&mut envelope, &sink.out);

        if let Err(failure) = &outcome {
            envelope.push_str(",\"error\":\"");
            envelope.push_str(&json_escape(&failure.to_string()));
            envelope.push('"');
        }
        if let Some(status) = halt_status {
            envelope.push_str(",\"halt_status\":");
            envelope.push_str(&status.to_string());
        }

        envelope.push_str(",\"value_errors\":[");
        for (i, error) in sink.errors.iter().enumerate() {
            if i > 0 {
                envelope.push(',');
            }
            envelope.push('"');
            envelope.push_str(&json_escape(error));
            envelope.push('"');
        }
        envelope.push(']');
        push_records(&mut envelope, session.diagnostics);
        envelope.push('}');

        // Route/cost receipts ride the diagnostic stream on success too.
        if outcome.is_ok() {
            session.diagnostics.record_route_named("wasm-run");
        }
        envelope
    })
}

// ---------------------------------------------------------------------------
// wasm-bindgen exports
// ---------------------------------------------------------------------------

/// Runs `program` over `input` and returns the result ENVELOPE as a JSON string (never fails across the boundary —
/// every failure lands inside the envelope as `ok:false` with the diagnostic records).
///
/// `indent` follows jq's `--indent` law: -1 = tabs, 0 = compact, 1..=7 = spaces per level; anything else lands in the
/// envelope as `ok:false`. `flags` is a bitmask of the `FLAG_*` constants; `slurp` is `-s`.
///
/// # Envelope shape
///
/// ```text
/// {"ok":true,"output":"...","value_errors":[],"records":[...]}
/// {"ok":false,"output":"partial","error":"...","records":[...],"halt_status":5}
/// ```
///
/// Binary output (CBOR/MessagePack targets) arrives base64 under `output_base64` with `"binary":true`.
///
/// # Panics
///
/// Only if the built-in format table itself names an unknown format — impossible for the committed table; user input
/// never reaches a panic (every caller error lands inside the envelope).
#[wasm_bindgen]
#[must_use]
pub fn jqf_run(
    program: &str,
    input: &[u8],
    input_format: &str,
    output_format: &str,
    indent: i32,
    flags: u32,
    slurp: bool,
) -> String {
    let unknown = find_format(input_format).map_or_else(
        || format!("unknown input format {input_format:?}"),
        |_| {
            find_format(output_format)
                .map_or_else(|| format!("unknown output format {output_format:?}"), |_| String::new())
        },
    );
    if !unknown.is_empty() {
        with_session(|session| {
            session.diagnostics.clear();
            session.diagnostics.record_setup_failure(&unknown);
        });
        return setup_envelope(&unknown);
    }
    let input_entry = find_format(input_format).expect("checked above");
    let output_entry = find_format(output_format).expect("checked above");
    run(program, input, input_entry, output_entry, indent, flags, slurp)
}

/// The supported formats and their default dialects, as a JSON array of `{"name","format","in","out"}` rows.
#[wasm_bindgen]
#[must_use]
pub fn jqf_formats() -> String {
    use std::fmt::Write as _;
    let mut out = String::from("[");
    for (i, entry) in FORMATS.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"name\":\"{}\",\"format\":\"{}\",\"in\":\"{}\",\"out\":\"{}\"}}",
            entry.name,
            entry.format,
            json_escape(entry.input_dialect),
            json_escape(entry.output_dialect),
        );
    }
    out.push(']');
    out
}

/// The ABI version.
#[wasm_bindgen]
#[must_use]
pub fn jqf_version() -> String {
    String::from(concat!("jqf-wasm ", env!("CARGO_PKG_VERSION")))
}

/// The ABI version as a number, for the JS wrapper's load-time check.
#[wasm_bindgen]
#[must_use]
pub fn jqf_abi_version() -> u32 {
    ABI_VERSION
}

/// Compiles the README examples as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

#[cfg(test)]
mod tests {
    use super::*;

    fn run_json(program: &str, input: &str) -> String {
        run(
            program,
            input.as_bytes(),
            find_format("json").unwrap(),
            find_format("json").unwrap(),
            0,
            0,
            false,
        )
    }

    /// Envelope string bodies use the JSON encoder law, including DEL `0x7F`.
    #[test]
    fn tab_indent_is_the_indent_argument_not_a_flag() {
        let rust_name = concat!("FLAG_TAB", "_INDENT");
        let js_name = concat!("TAB", "_INDENT");
        let rust = include_str!("lib.rs");
        let js = include_str!("../jqf.js");
        assert!(
            !rust.contains(rust_name),
            "tab indent is the indent i32, not a flags bit"
        );
        assert!(!js.contains(js_name), "jqf.js FLAGS must not export a tab-indent bit");
    }

    #[test]
    fn json_escape_matches_encoder_law_on_quote_c0_and_del() {
        let input = std::str::from_utf8(b"\"\\\x00\x08\x0c\n\r\t\x1f\x7fok").expect("utf-8");
        let mut expected = Vec::new();
        jqf_codec_json::push_json_escaped(&mut expected, input.as_bytes());
        assert_eq!(json_escape(input).as_bytes(), expected.as_slice());
        assert!(json_escape(input).contains("\\u007f"), "DEL must be \\u007f");
    }

    #[test]
    fn basic_json_query() {
        let envelope = run_json(".user.name", "{\"user\":{\"name\":\"Filip\"}}");
        assert!(envelope.contains("\"ok\":true"), "{envelope}");
        // The output field is a JSON string, so the inner quotes are escaped.
        assert!(envelope.contains("\\\"Filip\\\""), "{envelope}");
    }

    #[test]
    fn multi_output() {
        let envelope = run_json(".[] | . * 2", "[1,2,3]");
        assert!(envelope.contains("2\\n4\\n6"), "{envelope}");
    }

    #[test]
    fn yaml_input() {
        let envelope = run(
            ".name",
            b"name: app\nport: 8080\n",
            find_format("yaml").unwrap(),
            find_format("json").unwrap(),
            0,
            0,
            false,
        );
        assert!(envelope.contains("\"ok\":true"), "{envelope}");
    }

    #[test]
    fn compile_error_is_enveloped() {
        let envelope = run_json(".a ++", "{}");
        assert!(envelope.contains("\"ok\":false"), "{envelope}");
    }

    #[test]
    fn indent_out_of_range_is_refused_not_clamped() {
        // The old binding clamped 12 -> 7 spaces and published anyway; jq rejects the same width, so the envelope must
        // refuse.
        let envelope = run_json(".", "{}");
        let clamped = run(
            ".",
            b"{}",
            find_format("json").unwrap(),
            find_format("json").unwrap(),
            12,
            0,
            false,
        );
        assert!(clamped.contains("\"ok\":false"), "{clamped}");
        assert!(!clamped.contains("\"output\":\"{"), "{clamped}");
        drop(envelope);
    }

    #[test]
    fn slurp_collects_the_sequence_into_one_array() {
        let envelope = run(
            ".",
            b"1\n2\n3\n",
            find_format("json").unwrap(),
            find_format("json").unwrap(),
            0,
            0,
            true,
        );
        assert!(
            envelope.contains("[1,2,3]") && envelope.contains("\"ok\":true"),
            "slurp must wrap the sequence: {envelope}"
        );
    }

    #[test]
    fn null_input_beats_slurp_like_the_cli() {
        // The reference's precedence, pinned against the live binary: `-n` reads nothing, so `-n -s 'type'` answers
        // "null", never "array".
        let envelope = run(
            "type",
            b"1 2 3",
            find_format("json").unwrap(),
            find_format("json").unwrap(),
            0,
            FLAG_NULL_INPUT,
            true,
        );
        assert!(
            envelope.contains("\"output\":\"\\\"null\\\""),
            "-n beats -s: {envelope}"
        );
    }

    #[test]
    fn tsv_input_splits_on_tabs() {
        // Per-record drive: the program runs once per ROW, so `.[1]` is the row's second FIELD — which only exists when
        // the split byte is the tab (on a comma-split row it would stay the whole line).
        let envelope = run(
            ".[1]",
            b"name\tscore\na\t42\n",
            find_format("tsv").unwrap(),
            find_format("json").unwrap(),
            0,
            0,
            false,
        );
        assert!(
            envelope.contains("\"output\":\"\\\"score\\\"\\n\\\"42\\\"\\n\""),
            "TSV rows split on the tab, not the comma: {envelope}"
        );
    }

    #[test]
    fn record_input_slurp_runs_once_over_the_array() {
        let envelope = run(
            "length",
            b"1\n2\n3\n",
            find_format("ndjson").unwrap(),
            find_format("json").unwrap(),
            0,
            0,
            true,
        );
        assert!(
            envelope.contains("\"ok\":true") && envelope.contains("\"output\":\"3"),
            "record slurp answers length 3: {envelope}"
        );
    }
}
