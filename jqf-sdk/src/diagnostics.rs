//! The SDK's diagnostic record stream: retention, policy, and rendering.
//!
//! The engine PRODUCES records (raise sites, precision boundaries); this
//! module RETAINS them and renders them. It is the first-class consumer of
//! the design: bindings read the same records the CLI prints, and the stderr
//! line is one renderer of them.
//!
//! Laws (see .docs-intenal/diagnostics-sdk-design.md):
//! - The retained stream is bounded and never charged to the request ledger,
//!   so the ledger is identical under every policy.
//! - `render` is a PURE function of a record: template(code) + kind/operand.
//!   No engine state survives in a record, so FFI bindings can render
//!   statelessly.
//! - A record's operand is the BOUNDED rendering the message law already
//!   produces (capped, so a truncated element cannot leak past the bound).

use std::format;
use std::string::String;
use std::vec::Vec;

use jqf_codec_core::DiagnosticPolicy;
use jqf_resource::diag::{
    DiagnosticBuffer, DiagnosticRecord, DiagnosticSink, OwnedDiagnosticRecord, RecordClass, Severity, codes,
};

/// The SDK-owned record stream for one request.
///
/// The buffer is the sink the engine writes into (interior mutability, like
/// the stderr seam) and the report reads back from. `Off` is the absence of
/// this type; `ErrorsOnly` retains error-severity records only; `All`
/// retains the full capped stream.
#[derive(Debug)]
pub struct Diagnostics {
    buffer: DiagnosticBuffer,
    policy: DiagnosticPolicy,
}

impl Diagnostics {
    /// Creates the stream for one policy; `None` for `Off` (no sink, no cost).
    #[must_use]
    pub fn new(policy: DiagnosticPolicy) -> Option<Self> {
        match policy {
            DiagnosticPolicy::Off => None,
            policy => Some(Self {
                buffer: DiagnosticBuffer::with_cap(4096),
                policy,
            }),
        }
    }

    /// The retained records, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<OwnedDiagnosticRecord> {
        self.buffer.records()
    }

    /// The number of retained records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether no records are retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Calls `inspect` with one retained record, without cloning it.
    pub fn with_record<R>(&self, index: usize, inspect: impl FnOnce(&OwnedDiagnosticRecord) -> R) -> Option<R> {
        self.buffer.with_record(index, inspect)
    }

    /// The terminal failure record, when one was retained.
    #[must_use]
    pub fn failure(&self) -> Option<OwnedDiagnosticRecord> {
        self.buffer.failure()
    }

    /// How many records overflowed the cap.
    #[must_use]
    pub fn dropped(&self) -> usize {
        self.buffer.dropped()
    }

    /// Discards every retained record, so a caller that starts a new run on
    /// this same stream begins with nothing left over from the last one (see
    /// [`DiagnosticBuffer::clear`]).
    pub fn clear(&self) {
        self.buffer.clear();
    }

    /// Records the route that served the request, named (the CLI's rung
    /// ladder names each lane; the record's operand carries the name).
    pub fn record_route_named(&self, route: &str) {
        let mut record = DiagnosticRecord::new(codes::ROUTE_SELECTED);
        record.operand = Some(route);
        // Through the policy match, like every other producer: an
        // ErrorsOnly/Off request retains no informational route row.
        DiagnosticSink::record(self, record);
    }

    /// Records the request's cost snapshot (the SDK's own producer).
    ///
    /// `spill` is absent by design: the logical spill counter has no writer
    /// anywhere in the workspace, so the field could only ever render a
    /// constant zero. The spill work a request actually does is its run-file
    /// footprint, which `spill_disk` carries.
    pub fn record_cost(&self, snapshot: &jqf_resource::UsageSnapshot) {
        let mut record = DiagnosticRecord::new(codes::COST_SNAPSHOT);
        let operand = format!(
            "peak={} input={} output={} spill_disk={}",
            snapshot.memory_peak_bytes(),
            snapshot.input_bytes(),
            snapshot.output_bytes(),
            snapshot.spill_disk_bytes(),
        );
        record.operand = Some(&operand);
        // Through the policy match, like every other producer: an
        // ErrorsOnly/Off request retains no informational cost row.
        DiagnosticSink::record(self, record);
    }

    /// Records the run's TERMINAL failure and marks it terminal (it survives
    /// any later overflow). The raised channel carries the catchable text —
    /// rendered at this boundary, where it already exists.
    #[expect(
        clippy::match_same_arms,
        reason = "the failure vocabulary is one row per variant, in variant order; `EditOutputCount` shares `Registry`'s code only because it has no dedicated code YET, so merging would hide a row that is expected to diverge"
    )]
    pub fn record_failure<E>(&self, failure: &crate::PipelineFailure<E>) {
        use crate::PipelineFailure;
        let code = match failure {
            PipelineFailure::Registry(_) => codes::MACHINE_INTERNAL_CONTRACT,
            PipelineFailure::AccessBind(_) => codes::MACHINE_REQUIREMENT,
            PipelineFailure::Codec(codec) => codec.kind().diagnostic_code(),
            PipelineFailure::Sink(_) | PipelineFailure::SinkContract => codes::MACHINE_SINK,
            PipelineFailure::InvalidCooperativeCredits => codes::MACHINE_CREDITS,
            PipelineFailure::TypeMismatch { .. } => codes::RAISE_INDEX,
            PipelineFailure::IterateMismatch { .. } => codes::RAISE_ITERATE,
            PipelineFailure::ObjectKeyMismatch { .. } => codes::RAISE_OBJECT_KEY,
            PipelineFailure::NoLength { .. } => codes::RAISE_NO_LENGTH,
            PipelineFailure::NoKeys { .. } => codes::RAISE_NO_KEYS,
            PipelineFailure::ArithmeticError(failure) => match failure {
                jqf_engine::ArithFailure::TypeMismatch { .. } => codes::RAISE_ARITHMETIC,
                jqf_engine::ArithFailure::DivideByZero => codes::RAISE_DIVIDE_BY_ZERO,
                jqf_engine::ArithFailure::RemainderByZero => codes::RAISE_REMAINDER_BY_ZERO,
                jqf_engine::ArithFailure::NumericRange => codes::RAISE_NUMERIC_RANGE,
            },
            PipelineFailure::SliceIndices => codes::RAISE_SLICE_INDICES,
            PipelineFailure::MismatchRaised { cell } => {
                let _ = cell;
                codes::MISMATCH_STRICT
            }
            PipelineFailure::EngineCardinality { .. } => codes::RAISE_ENGINE_CARDINALITY,
            PipelineFailure::Raised(_) => codes::RAISE_PROGRAM,
            PipelineFailure::Halt { .. } => codes::RAISE_HALT,
            // The edit-route publication-count failure is a machine-class
            // contract of the source-edit vertical; no dedicated code yet.
            PipelineFailure::EditOutputCount { .. } => codes::MACHINE_INTERNAL_CONTRACT,
            // The split-destination refusal is a usage problem surfaced at
            // publication time; it shares the internal-contract code until it
            // earns a dedicated row.
            PipelineFailure::SplitName { .. } => codes::MACHINE_INTERNAL_CONTRACT,
            PipelineFailure::SplitCollision { .. } => codes::MACHINE_INTERNAL_CONTRACT,
        };
        let mut record = DiagnosticRecord::new(code);
        // The raised-value / halt-message payloads are OWNED renderings
        // (`raised_body` allocates), while the record's payload field is a
        // borrow — so the owned text is built in a local that outlives the
        // `buffer.record(record)` call below, and the record borrows it.
        let mut owned_payload: Option<std::string::String> = None;
        // The typed family's failing STEP is already in the failure
        // variant; carry it onto the terminal record so the FFI's
        // `jqf_diag_get` locator is readable on the failure row itself
        // , not only on the engine's earlier raise-site record.
        let typed_step_index = match failure {
            PipelineFailure::TypeMismatch { step_index, .. } | PipelineFailure::IterateMismatch { step_index, .. } => {
                Some(*step_index)
            }
            _ => None,
        };
        if let PipelineFailure::TypeMismatch { actual_type, .. }
        | PipelineFailure::IterateMismatch { actual_type, .. }
        | PipelineFailure::ObjectKeyMismatch { actual_type, .. }
        | PipelineFailure::NoLength { actual_type, .. }
        | PipelineFailure::NoKeys { actual_type, .. } = failure
        {
            record.kind = Some(kind_name(*actual_type));
        } else if let PipelineFailure::MismatchRaised { cell } = failure {
            // The strict-dial raise's payload is the cell name (`052` W3): the
            // registry says one code for all eleven cells, so the name rides
            // the record's payload field.
            record.payload = jqf_resource::policy::MISMATCH_CELL_NAMES
                .get(usize::from(*cell))
                .copied();
        } else if let PipelineFailure::Raised(raised) = failure {
            // The raised value's OWN rendering is the payload, whatever its
            // kind: a string as-is (the reference's uncaught-raise law) and any other
            // value as its compact JSON — so `error({"code":42})` arrives
            // with the object recoverable , not as a payload-less
            // RAISE_PROGRAM. `raised_body` is the engine's own bounded
            // renderer (the same text the reference prints after the frame).
            if let Ok(body) = jqf_engine::raised_body(raised.value()) {
                owned_payload = Some(body);
            }
        } else if let PipelineFailure::Halt { status, message, .. } = failure {
            // Plan 109: the program's own halt status rides the RAISE_HALT
            // record, dial-exempt at every strictness level. `halt_error`'s
            // message value is the payload, rendered under the reference's
            // own halt law (the value written RAW to stderr — a string
            // as-is, any other value compact); `halt` carries no message and
            // keeps a payload-less record.
            record.halt_status = Some(*status);
            if let Some(message) = message
                && let Ok(body) = jqf_engine::raised_body(message)
            {
                owned_payload = Some(body);
            }
        }
        if let Some(step) = typed_step_index
            && let Ok(step) = u32::try_from(step)
        {
            record.step_index = Some(step);
        }
        if let Some(body) = owned_payload.as_deref() {
            record.payload = Some(body);
        }
        self.buffer.record(record);
        self.buffer.mark_terminal();
    }

    /// Records the run's TERMINAL failure for everything [`record_failure`]
    /// cannot see: a run that never reached the pipeline at all — an invalid
    /// program, codec, format, dialect, or requirement, before any input was
    /// read.
    ///
    /// A host binding that returns a bare failure sentinel with an EMPTY
    /// diagnostic stream cannot tell that apart from a transport-level bug;
    /// this record is the ABI's only account of what actually went wrong, so
    /// every consumer of the C ABI needs it, not just one binding's own
    /// fallback logic.
    ///
    /// [`record_failure`]: Self::record_failure
    pub fn record_setup_failure(&self, message: &str) {
        let mut record = DiagnosticRecord::new(codes::MACHINE_SETUP);
        record.payload = Some(message);
        self.buffer.record(record);
        self.buffer.mark_terminal();
    }
}

impl DiagnosticSink for Diagnostics {
    fn record(&self, record: DiagnosticRecord<'_>) {
        match self.policy {
            DiagnosticPolicy::All => self.buffer.record(record),
            // ErrorsOnly retains exactly the failure class: the terminal
            // failure record is the policy's whole payload.
            DiagnosticPolicy::ErrorsOnly if record.severity == Severity::Error => {
                self.buffer.record(record);
            }
            DiagnosticPolicy::ErrorsOnly | DiagnosticPolicy::Off => {}
        }
    }
}

/// One code's reference-shaped template, with the record's kind/operand filled in.
/// Unknown codes and template-less families (arithmetic in v1) render their
/// registry name so a binding always gets SOMETHING readable.
#[must_use]
pub fn render_record(record: &OwnedDiagnosticRecord) -> String {
    let kind = record.kind().unwrap_or("value");
    let operand = record.operand().unwrap_or("");
    // Templates degrade gracefully when a v1 record lacks the operand: the
    // parenthesized tail is dropped, never rendered empty.
    let parenthesized = if operand.is_empty() {
        String::new()
    } else {
        format!(" ({operand})")
    };
    match record.code {
        codes::RAISE_ITERATE => format!("Cannot iterate over {kind}{parenthesized}"),
        codes::RAISE_INDEX if operand.is_empty() => format!("Cannot index {kind}"),
        codes::RAISE_INDEX => format!("Cannot index {kind} with {operand}"),
        codes::RAISE_OBJECT_KEY => format!("Cannot use {kind}{parenthesized} as object key"),
        codes::RAISE_NO_LENGTH => format!("{kind}{parenthesized} has no length"),
        codes::RAISE_NO_KEYS => format!("{kind}{parenthesized} has no keys"),
        codes::RAISE_SLICE_INDICES => "Array/string slice indices must be integers".into(),
        codes::RAISE_DIVIDE_BY_ZERO => "division by zero".into(),
        codes::RAISE_REMAINDER_BY_ZERO => "remainder by zero".into(),
        codes::RAISE_NONTERMINATING => "non-terminating decimal division".into(),
        codes::RAISE_NUMERIC_RANGE => "numeric range overflow".into(),
        codes::RAISE_ARITHMETIC if operand.is_empty() => "arithmetic failure".into(),
        codes::RAISE_ARITHMETIC => format!("{kind} and {operand} cannot be combined"),
        // `halt_error`'s message rides the payload exactly as RAISE_PROGRAM's
        // raised value does; a bare `halt` keeps a payload-less record and
        // renders the plain word.
        codes::RAISE_HALT => record.payload().map_or_else(|| "halt".into(), str::to_owned),
        codes::MACHINE_INPUT => "input violates the selected format or dialect".into(),
        codes::MACHINE_REPRESENTATION => "value cannot be represented by the target".into(),
        codes::MACHINE_REQUIREMENT => "no physical route satisfies the requirement".into(),
        codes::MACHINE_ROUTE_MISMATCH => "provider or route identity mismatch".into(),
        codes::MACHINE_INVALID_TAG => "tag identity invalid for the target codec".into(),
        codes::MACHINE_COLLIDING_TAGS => "distinct tag identities collide".into(),
        codes::MACHINE_RESOURCE => "request accounting rejected the operation".into(),
        codes::MACHINE_CANCELLED => "request cancelled".into(),
        codes::MACHINE_DEADLINE => "request deadline expired".into(),
        codes::MACHINE_MEMORY => "physical memory ceiling exceeded".into(),
        codes::MACHINE_OVERFLOW => "checked arithmetic overflowed".into(),
        codes::MACHINE_ALLOCATION => "allocation failed".into(),
        codes::MACHINE_INTERNAL_CONTRACT => "internal contract violation".into(),
        codes::MACHINE_SINK => "host output sink failed".into(),
        codes::RAISE_PROGRAM => record.payload().map_or_else(|| "error".into(), str::to_owned),
        codes::ROUTE_SELECTED => format!("route: {operand}"),
        codes::COST_SNAPSHOT => format!("cost: {operand}"),
        codes::PRECISION_BOUNDARY => "exact-to-binary64 contagion".into(),
        codes::MISMATCH_STRICT => format!(
            "mismatch under strict policy: {}",
            record.payload().unwrap_or("<unknown-cell>")
        ),
        codes::MISMATCH_WARN => format!("mismatch report: {operand}"),
        _ => format!("{} (no template in v1)", record.code_name()),
    }
}

/// One record as a JSON object: the `--diagnostics json` line shape.
#[must_use]
pub fn record_json(record: &OwnedDiagnosticRecord) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("{");
    let _ = write!(
        out,
        "\"code\":{},\"name\":\"{}\",\"revision\":{},\"class\":\"{}\",\"severity\":\"{}\",\"catchable\":{}",
        record.code,
        record.code_name(),
        record.revision,
        class_name(record.class),
        severity_name(record.severity),
        record.catchable
    );
    let _ = write!(
        out,
        ",\"caught\":{},\"step\":{},\"input\":{},\"byte_offset\":{},\"halt_status\":{},\"kind\":{},\"operand\":{},\"payload\":{}",
        option_json(record.caught.map(|d| format!("{d}"))),
        option_json(record.step_index.map(|s| format!("{s}"))),
        option_json(record.halt_status.map(|h| format!("{h}"))),
        option_json(record.input_ordinal.map(|o| format!("{o}"))),
        option_json(record.byte_offset.map(|b| format!("{b}"))),
        option_json(record.kind().map(str::to_owned)),
        option_json(record.operand().map(str::to_owned)),
        option_json(record.payload().map(str::to_owned)),
    );
    out.push('}');
    out
}

/// The payload-free kind name for one semantic category (the message law's
/// own spelling, mirrored here so records carry it without engine state).
fn kind_name(kind: jqf_data::ValueKind) -> &'static str {
    match kind {
        jqf_data::ValueKind::Null => "null",
        jqf_data::ValueKind::Bool => "boolean",
        jqf_data::ValueKind::Number => "number",
        jqf_data::ValueKind::String => "string",
        jqf_data::ValueKind::Array => "array",
        jqf_data::ValueKind::Object => "object",
        jqf_data::ValueKind::Bytes => "bytes",
        jqf_data::ValueKind::LocalDate => "date",
        jqf_data::ValueKind::LocalTime => "time",
        // the reference's type vocabulary has no offset/local distinction.
        jqf_data::ValueKind::LocalDateTime | jqf_data::ValueKind::OffsetDateTime => "datetime",
    }
}

fn class_name(class: RecordClass) -> &'static str {
    match class {
        RecordClass::Semantic => "Semantic",
        RecordClass::Machine => "Machine",
        RecordClass::ProgramRaised => "ProgramRaised",
        RecordClass::Informational => "Informational",
    }
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "Error",
        Severity::Warning => "Warning",
        Severity::Info => "Info",
        Severity::Trace => "Trace",
    }
}

fn option_json(value: Option<String>) -> String {
    match value {
        Some(text) => {
            // The escape law is the JSON codec's `push_json_escaped`:
            // the diag NDJSON stream must be valid JSON, so a control
            // byte in a record field is escaped, never raw.
            let mut out = Vec::with_capacity(text.len() + 2);
            out.push(b'"');
            jqf_codec_json::push_json_escaped(&mut out, text.as_bytes());
            out.push(b'"');
            // The escaper only appends ASCII escapes and passes valid UTF-8
            // through, so the result is valid UTF-8.
            String::from_utf8(out).expect("escaped JSON string is valid UTF-8")
        }
        None => "null".into(),
    }
}
