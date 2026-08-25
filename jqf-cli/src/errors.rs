//! The CLI failure model: exit classes, the reactor message, and the renderers.
//!
//! Cohesive unit: what the process exits with and how each failure renders. Parsing, route execution, and I/O all
//! report through [`CliFailure`], but the type and its rendering live here alone, so a new exit path needs no other
//! module change.

use std::fmt::{self, Write as _};

use jqf_engine::EngineCompileError;
use jqf_sdk::PipelineFailure;

/// Process exit classes.
///
/// jq documents 2 for "usage problem or system error", 3 for a compile error, and 5 for a runtime error; a malformed
/// INPUT was measured at 5 as well, not the documented 2.
#[derive(Clone, Copy)]
pub(crate) enum ExitClass {
    /// A usage problem or a host/system failure.
    Usage,
    /// The program was rejected before it ran.
    Compile,
    /// The program ran and a value failed, or the input would not parse.
    Runtime,
}

impl ExitClass {
    const fn code(self) -> u8 {
        match self {
            Self::Usage => 2,
            Self::Compile => 3,
            Self::Runtime => 5,
        }
    }
}

pub(crate) enum CliFailure {
    Message {
        class: ExitClass,
        message: String,
    },
    Codec {
        kind: jqf_codec_core::CodecFailureKind,
        diagnostic: Option<String>,
    },
    /// A per-value runtime error already streamed to stderr by [`StdoutSink:report_value_error`] as the sequence
    /// continued past it. The process must exit with the runtime class (the last value's), but the message is not
    /// reprinted — jq reports each such error exactly once.
    Reported,
    /// `halt`/`halt_error` terminated the run: exit with `status`, printing the already-rendered message (if any) RAW
    /// to stderr first — jq's `halt_error` writes the value compact and exits, without the `jq: error` frame.
    Halt {
        /// The exit status (already masked to the process's byte).
        status: u8,
        /// The rendered message value, when the terminating call was `halt_error`.
        message: Option<String>,
    },
}

impl CliFailure {
    /// A program the compiler rejected: the adopted exit-3 class.
    pub(crate) fn compile(message: String) -> Self {
        Self::Message {
            class: ExitClass::Compile,
            message,
        }
    }

    /// A request the ROUTE planner rejects before a byte is read: jq's exit-2 usage class (the
    /// `--workers`-with-`--no-parallel` law's class).
    pub(crate) fn usage(message: &str) -> Self {
        Self::Message {
            class: ExitClass::Usage,
            message: String::from(message),
        }
    }

    /// The process exit class this failure carries.
    pub(crate) const fn class(&self) -> ExitClass {
        match self {
            Self::Message { class, .. } => *class,
            // A malformed input and an uncaught per-value error are both the runtime class; jqf keeps its own richer
            // parse diagnostic (a recorded intentional difference) but adopts the adopted exit code. A `halt` exit is
            // the caller's own code; `class` is only the fallback inside [`Self:exit_code`], which never consults it.
            Self::Codec { .. } | Self::Reported | Self::Halt { .. } => ExitClass::Runtime,
        }
    }

    /// The process exit code this failure carries, including a `halt`'s own.
    pub(crate) const fn exit_code(&self) -> u8 {
        match self {
            Self::Halt { status, .. } => *status,
            _ => self.class().code(),
        }
    }
}

impl From<String> for CliFailure {
    /// A host/system failure: the arguments, stdin, the request ledger, or the built-in registrations. Program
    /// rejections use [`CliFailure:compile`].
    fn from(message: String) -> Self {
        Self::Message {
            class: ExitClass::Usage,
            message,
        }
    }
}

impl From<&'static str> for CliFailure {
    fn from(message: &'static str) -> Self {
        Self::from(message.to_owned())
    }
}

impl fmt::Display for CliFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message { message, .. } => formatter.write_str(message),
            // The exit path prints a `halt` message RAW and never displays this frame, exactly like `Reported`'s
            // already-printed stderr line; the arms are merged because their bodies are identical.
            Self::Halt { .. } | Self::Reported => Ok(()),
            // The resource/limit family is the ONE codec failure a user can provoke by POLICY rather than by a
            // malformed document, so it gets the house error frame and prose numbers. It precedes the diagnostic arm
            // deliberately: the structural dump below is itself `Debug`-formatted, so routing a limit failure through
            // it would reintroduce exactly the type syntax this arm exists to remove. `--max-memory-bytes` is a CLI
            // concept, so the flag hint is added here; the dimensions and the numbers are `jqf-resource`'s own
            // rendering.
            Self::Codec {
                kind: jqf_codec_core::CodecFailureKind::Resource(error),
                ..
            } => write!(formatter, "error: {}", resource_note(*error)),
            // The physical governor's refusal: the same house frame as the accounted rejection, with the actionable
            // body the governor recorded — measured RSS, the ceiling, its provenance, the retained-input split, and the
            // override flag. Distinct diag code (MACHINE_MEMORY), distinct text, distinct knob.
            Self::Codec {
                kind: jqf_codec_core::CodecFailureKind::Control(jqf_resource::ControlError::MemoryExceeded),
                ..
            } => write!(formatter, "error: {}", crate::rss::refusal_message()),
            // the adopted wording for this exact case (the token-span pass's `--raw-output0` guard), so a script
            // grepping stderr for it keeps working unchanged. Matched before the generic diagnostic arm below: this
            // failure carries no [`CodecDiagnostic`], only the kind, so the generic `Debug`-dump arm would otherwise
            // catch it.
            Self::Codec {
                kind: jqf_codec_core::CodecFailureKind::RawNulByte,
                ..
            } => formatter.write_str("Cannot dump a string containing NUL with --raw-output0 option"),
            Self::Codec {
                diagnostic: Some(diagnostic),
                ..
            } => formatter.write_str(diagnostic),
            Self::Codec { kind, diagnostic: None } => write!(formatter, "codec failed: {}", render_codec_kind(*kind)),
        }
    }
}

pub(crate) struct BoundedText {
    value: String,
    max_len: usize,
}

impl BoundedText {
    fn try_new(max_len: usize) -> Result<Self, ()> {
        let mut value = String::new();
        value.try_reserve_exact(256.min(max_len)).map_err(|_| ())?;
        Ok(Self { value, max_len })
    }
}

impl fmt::Write for BoundedText {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let final_len = self.value.len().checked_add(value.len()).ok_or(fmt::Error)?;
        if final_len > self.max_len {
            return Err(fmt::Error);
        }
        self.value.try_reserve_exact(value.len()).map_err(|_| fmt::Error)?;
        self.value.push_str(value);
        Ok(())
    }
}

pub(crate) fn resource_note(error: jqf_resource::ResourceError) -> String {
    let hint = match error {
        jqf_resource::ResourceError::LimitExceeded {
            limit_kind: jqf_resource::ResourceLimit::MemoryBytes,
            ..
        } => " (raise the ceiling with --max-memory-bytes)",
        jqf_resource::ResourceError::LimitExceeded {
            limit_kind: jqf_resource::ResourceLimit::SpillDiskBytes,
            ..
        } => " (raise the ceiling with --max-spill-disk-bytes)",
        _ => "",
    };
    format!("{error}{hint}")
}

pub(crate) fn render_codec_diagnostic(error: &jqf_codec_core::CodecError) -> Option<String> {
    const MAX_DIAGNOSTIC_BYTES: usize = 4_096;
    let diagnostic = error.diagnostic()?;
    let mut output = BoundedText::try_new(MAX_DIAGNOSTIC_BYTES).ok()?;
    write!(
        output,
        "{}: {}: {}",
        diagnostic.code(),
        diagnostic.message(),
        render_codec_kind(error.kind())
    )
    .ok()?;
    for source in diagnostic.sources() {
        write!(
            output,
            "\n  source {} base={} {}",
            source.source(),
            source.base_offset(),
            source.label()
        )
        .ok()?;
    }
    for label in diagnostic.labels() {
        let span = label.span();
        // `label.style` stays Debug on purpose: `LabelStyle` is a closed two-variant enum with no string payload.
        write!(
            output,
            "\n  {:?} {} {}..{} {}",
            label.style(),
            label.source(),
            span.start(),
            span.end(),
            label.message()
        )
        .ok()?;
    }
    Some(output.value)
}

/// Classifies one compile-time rejection: everything the compiler REJECTS is the adopted exit-3 class, while a ledger
/// rejection while charging the compiled arena is a host resource failure, not a statement about the program. A
/// rejection that carries a span gets a caret excerpt appended — the message keeps its byte offsets, the excerpt shows
/// the line and points at the column, so the user reads the mistake instead of counting bytes.
pub(crate) fn compile_failure(error: &EngineCompileError, source: &str) -> CliFailure {
    let mut message = format!("{error}");
    // Module-not-found is classified as an unsupported construct, whose Display appends the whole supported-surface
    // catalog. Keep the one line that names the miss; the catalog is engine-owned and not this failure's message.
    if message.contains("module not found")
        && let Some(cut) = message.find(" is outside the supported surface")
    {
        message.truncate(cut);
    }
    if let Some(span) = error.span()
        && let Some(excerpt) = caret_excerpt(source, span.start(), span.end())
    {
        message.push('\n');
        message.push_str(&excerpt);
    }
    match error {
        // A ledger rejection while charging the compiled arena is a host resource failure in the LIMIT class (exit 5),
        // not a usage or compile rejection: the program is fine, the ceiling refused it — the same class the run-phase
        // refusals carry, and what the limiterr corpus rows pin. The default `From<String>` maps to Usage (2), which is
        // why this arm exists.
        EngineCompileError::Resource(_) => CliFailure::Message {
            class: ExitClass::Runtime,
            message,
        },
        _ => CliFailure::compile(message),
    }
}

/// The caret block under a compile rejection: the source line the span starts on, then a caret line under the offending
/// columns. Byte-column alignment — exact for the ASCII programs people type; a multibyte or tab column can land the
/// caret a cell off, never crash. A line longer than the window is cut around the caret with `…` markers.
fn caret_excerpt(source: &str, start: u32, end: u32) -> Option<String> {
    const WINDOW: usize = 120;
    let start = usize::try_from(start).ok()?;
    let end = usize::try_from(end).ok()?;
    // `start == len` is the everyday case (`.a |` rejects AT end of input); anything past that means the span is not
    // about this source.
    if start > source.len() {
        return None;
    }
    let bytes = source.as_bytes();
    let line_start = bytes[..start]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |position| position + 1);
    let line_end = bytes[start..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(source.len(), |position| start + position);
    let column = start - line_start;
    let caret_width = end.clamp(start + 1, line_end.max(start + 1)) - start;
    // Cut the shown line to the window, keeping the caret inside it.
    let (shown_start, prefix) = if column > WINDOW / 2 {
        (start - WINDOW / 2, "…")
    } else {
        (line_start, "")
    };
    let (shown_end, suffix) = if line_end - shown_start > WINDOW {
        (shown_start + WINDOW, "…")
    } else {
        (line_end, "")
    };
    let line = String::from_utf8_lossy(&bytes[shown_start..shown_end]);
    let padding = " ".repeat(prefix.chars().count() + (start - shown_start));
    let carets = "^".repeat(caret_width.min(shown_end.max(start) - start).max(1));
    Some(format!("  {prefix}{line}{suffix}\n  {padding}{carets}"))
}

/// Renders one codec failure kind as a user-facing sentence. The kind alone is a class name
/// (`UnsupportedRepresentation`), not a message — this is the 049 item 3 / 043 W4 removal of `Debug` reaching users: a
/// failure with no structured diagnostic attached still tells the user what happened in words, and a structured
/// diagnostic's own message names the specific shape problem (which value, which key, why the format cannot hold it).
fn render_codec_kind(kind: jqf_codec_core::CodecFailureKind) -> String {
    use jqf_codec_core::CodecFailureKind;
    match kind {
        // The two host-flavored arms stay CLI-side: the memory refusal names the governor and the resource note appends
        // the ceiling hint. Every other kind renders through the kind's own Display — the wording law lives in
        // jqf-codec-core beside the type, so the FFI's `MACHINE_SETUP` payload and this function cannot drift apart.
        CodecFailureKind::Control(jqf_resource::ControlError::MemoryExceeded) => crate::rss::refusal_message(),
        CodecFailureKind::Resource(error) => resource_note(error),
        other => format!("{other}"),
    }
}

/// Renders one pipeline failure with the class its boundary owns. The read and parse-refusal classes render the host's
/// own message; a request failure is a usage problem; the pipeline failure renders through its own renderer.
pub(crate) fn render_failure(error: &jqf_sdk::Failure) -> CliFailure {
    match error {
        jqf_sdk::Failure::Read(read) => CliFailure::from(read.message().to_owned()),
        jqf_sdk::Failure::ParseRefused(message) => CliFailure::Message {
            class: ExitClass::Runtime,
            message: message.clone(),
        },
        jqf_sdk::Failure::Request(request) => CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("invalid request: {request}"),
        },
        jqf_sdk::Failure::Pipeline(pipeline) => render_pipeline_failure(pipeline),
    }
}

pub(crate) fn render_pipeline_failure<SinkError: fmt::Display>(
    error: &jqf_sdk::PipelineError<SinkError>,
) -> CliFailure {
    match error.failure() {
        PipelineFailure::Registry(failure) => CliFailure::from(format!("codec selection failed: {failure}")),
        PipelineFailure::AccessBind(failure) => CliFailure::from(format!("codec route bind failed: {failure}")),
        PipelineFailure::Codec(failure) => CliFailure::Codec {
            kind: failure.kind(),
            diagnostic: render_codec_diagnostic(failure),
        },
        PipelineFailure::Sink(failure) => CliFailure::from(format!("stdout failed: {failure}")),
        PipelineFailure::SinkContract => CliFailure::from("stdout violated the write contract"),
        PipelineFailure::InvalidCooperativeCredits => CliFailure::from("invalid cooperative work quantum"),
        PipelineFailure::EditOutputCount { observed } => CliFailure::Message {
            // The program ran and its output count failed the edit law: the runtime class, not a usage problem.
            class: ExitClass::Runtime,
            message: format!("edit mode requires exactly one output per document; the program produced {observed}"),
        },
        // The split destination's name refusal : the expression produced no output or a non-string for one item. A
        // usage problem — the request's expression contract failed, not the program's — so the usage class, with the
        // item index and the produced kind named.
        PipelineFailure::SplitName { index, detail } => CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("--split-exp: item {index}: {detail}"),
        },
        PipelineFailure::SplitCollision {
            name,
            first_index,
            second_index,
        } => CliFailure::Message {
            class: ExitClass::Usage,
            message: format!("--split-exp: destination {name} written by item {first_index} and item {second_index}"),
        },
        // Per-value runtime mismatches (index, the distinct iterate class, the object-key class) and uncaught
        // program-raised values are the sequence's LAST value's class; each was already streamed to stderr by the sink
        // as the sequence continued (every CLI drive publishes each value as it lands), so the final failure only sets
        // the exit class and is not reprinted.
        PipelineFailure::TypeMismatch { .. }
        | PipelineFailure::IterateMismatch { .. }
        | PipelineFailure::ObjectKeyMismatch { .. }
        | PipelineFailure::NoLength { .. }
        | PipelineFailure::NoKeys { .. }
        | PipelineFailure::ArithmeticError(_)
        | PipelineFailure::SliceIndices
        | PipelineFailure::MismatchRaised { .. }
        | PipelineFailure::EngineCardinality { .. }
        | PipelineFailure::Raised(_) => CliFailure::Reported,
        // `halt`/`halt_error`: render the message now (a string prints as its raw text, a non-null non-string prints
        // compact plus a newline, and `null` prints nothing); the exit path prints it raw and exits with the code. The
        // byte masking itself is the engine's `halt_status` law; this arm only narrows to the process exit byte. If the
        // full body's render fails (allocation), the engine's BOUNDED compact dump stands in — a halt must never exit
        // with nothing printed.
        PipelineFailure::Halt { status, message } => CliFailure::Halt {
            status: u8::try_from(*status).unwrap_or(0),
            message: message.as_ref().and_then(|value| match value {
                jqf_data::Value::Null => None,
                jqf_data::Value::String(text) => Some(text.as_str().to_owned()),
                other => Some(match jqf_engine::raised_body(other) {
                    Ok(body) => format!("{body}\n"),
                    Err(_) => format!(
                        "<{}: {}>",
                        jqf_engine::kind_name(other.kind()),
                        jqf_engine::dump_trunc_owned(other)
                            .unwrap_or_else(|_| "halt_error body could not be rendered".to_owned())
                    ),
                }),
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::render_codec_diagnostic;
    use jqf_codec_core::{CodecError, CodecFailureKind};
    use jqf_source::{Diagnostic, DiagnosticSource, Label, Namespace, Severity, SourceId, SourceKind, SourceRef, Span};

    /// Message payloads render unquoted (finding 3): the 043-W4 law removed Debug enum names, and the message strings
    /// must not keep Debug quoting.
    #[test]
    fn diagnostic_messages_render_unquoted() {
        let input = SourceRef::new(SourceId::new(0), SourceKind::Input);
        let diagnostic = Diagnostic::new(
            Namespace::new("toml").code("expected-value"),
            Severity::Error,
            "expected a value",
        )
        .with_source(DiagnosticSource::new(input, "input#0", 0))
        .with_label(Label::primary(input, Span::new(0, 1), "an \"unquoted\" label"));
        let error = CodecError::new(CodecFailureKind::InvalidInput).with_diagnostic(diagnostic);

        let rendered = render_codec_diagnostic(&error).expect("render");
        assert!(
            rendered.contains("toml.expected-value: expected a value"),
            "message Debug-quoted: {rendered}"
        );
        assert!(
            rendered.contains("source input#0 base=0 input#0"),
            "source label Debug-quoted: {rendered}"
        );
        assert!(
            rendered.contains("an \"unquoted\" label"),
            "label message Debug-quoted: {rendered}"
        );
        // `label.style` stays Debug: a closed enum with no string payload.
        assert!(rendered.contains("Primary"), "style lost Debug: {rendered}");
    }
}
