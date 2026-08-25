//! The one public entry point's request surface.
//!
//! Everything an embedder needs to say about a run travels in a
//! [`Request`]: the compiled program, the input shape, the format/dialect
//! selection for both sides, the pipeline policy and framing, the resource
//! context, the diagnostic stream, and the input-model flags the drives are
//! selected by. Request fields travel the way dialects and limits already
//! travel — a request is a value, not a parameter list.
//!
//! The SDK's routing is deliberately invisible here: `execute` picks the
//! drive from `Input` + the flags + the compiled program's own facts, and an
//! embedder never names a route. This is the surface 122 W4's exit gate
//! pins: exactly one public `execute` in the crate, and the route-named
//! entry points are `pub(crate)`.

use std::error::Error as StdError;
use std::fmt;
use std::sync::OnceLock;

use jqf_codec_core::{
    AccessRequirement, DecodeRequest, DiagnosticPolicy, ErasedRecordStreamProvider, PreservationRequest, RouteSlot,
    ValidationMode,
};
use jqf_data::{DialectId, FormatId};
use jqf_engine::CompiledProgram;
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceFileRange, SourceId, SourceKind, SourceRef};

/// The default cooperative credit quantum, matching the CLI's. Zero is an
/// invalid quantum and the maximum is 4096, so a defaulted request must not
/// carry zero.
pub const DEFAULT_COOPERATIVE_CREDITS: u32 = 4_096;

/// The SDK's process-lifetime default dialect, allocated exactly ONCE.
///
/// `Request::new`'s default policy must borrow a `&'static DialectId`, and the
/// builder's own `input_dialect` field cannot be borrowed by the returned
/// `Self`. The historical per-call `Box::leak` charged every morsel worker's
/// child ledger with a 55-byte `Working` leak and broke detach quiescence,
/// yielding every record-parallel request to serial; the static is initialized
/// lazily and [`warm_default_dialect`] lets the morsel relay run that one
/// allocation on the coordinator thread, before any worker child ledger exists
/// to be charged.
static DEFAULT_DIALECT: OnceLock<DialectId> = OnceLock::new();

/// Allocates the process-lifetime default dialect NOW, on the calling thread.
///
/// The record and adjacent-value morsel relays call this on the coordinator
/// thread before spawning workers, so the SDK's one per-process dialect
/// allocation never lands on a worker child ledger whose detach demands
/// quiescence. Embedders with no worker lane never need it: the first
/// [`Request::new`] initializes the static lazily.
pub fn warm_default_dialect() {
    let _ = default_dialect();
}

fn default_dialect() -> &'static DialectId {
    DEFAULT_DIALECT.get_or_init(|| DialectId::try_new("rfc8259").expect("built-in json dialect"))
}

use crate::diagnostics::Diagnostics;
use crate::drive::{CodecCatalog, FacadeFraming, ItemSink, PipelineError, PipelinePolicy, PublicationStatus};

/// The host read callback failed while a streaming drive pulled input.
///
/// The message is host-rendered at the callback site; the SDK never invents
/// read failures.
#[derive(Debug)]
pub struct ReadFailure {
    message: String,
}

impl ReadFailure {
    /// Builds a read failure with the host's own message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The host-rendered message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ReadFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// What `execute` should run over.
pub enum Input<'a> {
    /// One retained buffer: a whole document, or the adjacent-value stream it
    /// holds.
    Whole(&'a [u8]),
    /// A pull read callback; the SDK never opens a file or a socket. The
    /// callback returns the number of bytes written, or an error; `Ok(0)` is
    /// EOF. The box is owned because the streaming cursors are stored through
    /// the engine's `Box<dyn Any>` host-extension seam, which demands a
    /// `'static` source.
    #[expect(
        clippy::type_complexity,
        reason = "the pull callback's boxed shape is the ABI of the streaming input seam"
    )]
    Streaming(Box<dyn FnMut(&mut [u8]) -> Result<usize, ReadFailure> + 'static>),
    /// Physically framed records over one retained source. The SDK is
    /// codec-agnostic, so the framing provider is opened by the embedder
    /// (through the framing codec's `create_record_provider`) and travels
    /// here beside the slot it serves.
    Records {
        /// The retained source bytes the records and payloads borrow.
        source: &'a [u8],
        /// The opened framing provider.
        records: ErasedRecordStreamProvider<'a>,
        /// The record route slot the provider serves.
        slot: RouteSlot,
    },
}

/// The request-construction error channel.
///
/// A request that cannot be honoured must be refused, never silently
/// degraded — the same rejection law `--max-memory-bytes` holds at the CLI.
#[derive(Debug)]
pub enum RequestError {
    /// A field the selected drive requires was not set on the request.
    MissingField(&'static str),
    /// Streaming `--stream` cannot honour `-n` or `-s`. Those flags need
    /// the whole-input event drive; silently dropping them would run
    /// per-event over the source.
    IncompatibleStreamEventsOption(&'static str),
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestError::MissingField(field) => {
                write!(formatter, "request is missing required field {field}")
            }
            RequestError::IncompatibleStreamEventsOption(option) => {
                write!(formatter, "streaming --stream cannot honour {option}")
            }
        }
    }
}

impl StdError for RequestError {}

/// The one public failure of the one public entry point.
///
/// `execute` returns this on every failure path; it is `std::error::Error`
/// so an embedder can `?` it into `anyhow`/`eyre` without a wrapper.
#[derive(Debug)]
pub enum Failure {
    /// The host read callback failed while a streaming drive pulled input.
    Read(ReadFailure),
    /// A `--stream` parse refusal: jq's message text, at the parse class
    /// (exit 5), with every earlier event's bytes already published.
    ParseRefused(String),
    /// The request itself was invalid or incomplete.
    Request(RequestError),
    /// The pipeline failed; the host sink's own error is erased to its
    /// Display text so `Failure` stays non-generic and `Send + Sync`.
    Pipeline(PipelineError<String>),
}

impl Failure {
    /// The exact pipeline failure, when this is a pipeline failure.
    #[must_use]
    pub fn pipeline_failure(&self) -> Option<&crate::drive::PipelineFailure<String>> {
        match self {
            Failure::Pipeline(error) => Some(error.failure()),
            _ => None,
        }
    }

    /// Publication state at the failure boundary, when this is a pipeline
    /// failure (the pipeline's own partial-publication status).
    #[must_use]
    pub fn publication(&self) -> Option<PublicationStatus> {
        match self {
            Failure::Pipeline(error) => Some(error.publication()),
            _ => None,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::Read(read) => write!(formatter, "read failed: {read}"),
            Failure::ParseRefused(message) => write!(formatter, "{message}"),
            Failure::Request(request) => write!(formatter, "{request}"),
            Failure::Pipeline(error) => fmt::Display::fmt(error, formatter),
        }
    }
}

impl StdError for Failure {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Failure::Request(request) => Some(request),
            // The pipeline failure is not a `std::error::Error` source here:
            // its host sink error is erased to a `String`, which is not
            // itself an error. `Display` carries the message either way.
            _ => None,
        }
    }
}

/// What one `execute` call produced.
#[derive(Debug)]
pub enum Outcome {
    /// The drive served the request; the report describes the publication.
    Served(Report),
    /// A decline-able drive declined: nothing reached the sink, and the
    /// caller is free to fall through to the next lane.
    Declined,
}

/// One run request: what to run, over what input, under what options.
///
/// The three lifetimes separate the borrow classes the drives consume:
/// `'request` is the program, catalog, requirement, policy, and label borrows;
/// `'context` is the resource context's inner borrow (and the diagnostic
/// stream, which rides the same region); `'input` is the input itself — the
/// retained buffer, the opened record provider, or the read callback, the
/// shortest-lived thing an embedder hands over. Keeping them apart is what
/// lets a caller reuse one program and one resource context across requests
/// of different inputs.
#[expect(
    clippy::struct_excessive_bools,
    reason = "the Request carries one bool per input-model option;               bundling them would hide the option surface from the dispatcher"
)]
pub struct Request<'request, 'context, 'input> {
    pub(crate) catalog: CodecCatalog<'request, 'request>,
    pub(crate) source: ResolvedSource<'input>,
    pub(crate) files: Option<&'request [SourceFileRange<'request>]>,
    pub(crate) program: &'request CompiledProgram,
    pub(crate) input: Input<'input>,
    pub(crate) requirement: Option<&'request AccessRequirement>,
    pub(crate) input_format: FormatId,
    pub(crate) input_dialect: DialectId,
    pub(crate) output_format: FormatId,
    pub(crate) output_dialect: DialectId,
    pub(crate) policy: PipelinePolicy<'request>,
    pub(crate) framing: FacadeFraming<'request>,
    pub(crate) resources: Option<&'request mut ResourceContext<'context>>,
    pub(crate) diagnostics: Option<&'context Diagnostics>,
    pub(crate) label: &'request str,
    pub(crate) null_input: bool,
    pub(crate) slurped: bool,
    pub(crate) stream_events: bool,
    pub(crate) stream_errors: bool,
    pub(crate) editing: bool,
    pub(crate) input_family: bool,
    pub(crate) roundtrip: bool,
    pub(crate) range_locate: bool,
}

impl<'request, 'context, 'input> Request<'request, 'context, 'input> {
    /// Begins a request for `program` over `input`, with every option at its
    /// default (whole-input adjacent values, JSON in and out, the default
    /// policy and framing, no resource context, no diagnostic stream).
    ///
    /// The returned request is not yet executable: the required fields
    /// ([`Request::with_catalog`], [`Request::with_source`],
    /// [`Request::with_resources`], and the requirement where the selected
    /// drive needs one) must be set before `execute`.
    ///
    /// # Panics
    ///
    /// Panics only if the built-in `json`/`rfc8259` identities stop validating,
    /// which is an internal contract violation, never caller input.
    #[must_use]
    pub fn new(program: &'request CompiledProgram, input: Input<'input>) -> Self {
        // The source is derived from the input bytes so a plain embedder
        // needs nothing but `new` plus the resource context; a caller with
        // its own label/identity overrides it with `with_source`.
        let (bytes, identity) = match &input {
            Input::Whole(bytes) => (*bytes, SourceKind::Input),
            Input::Records { source, .. } => (*source, SourceKind::Input),
            // The streaming drives never read a retained source; the empty
            // placeholder is never consulted.
            Input::Streaming(_) => (&[][..], SourceKind::Input),
        };
        // The decode policy's request borrows a `'static` dialect (123 X5
        // carries the dialect on the request). The builder's own
        // `input_dialect` field cannot be borrowed by the returned `Self`, so
        // the request borrows the SDK's process-lifetime default dialect — one
        // allocation per process, never one per call (a per-call leak charged
        // every morsel worker's child ledger and broke detach quiescence).
        let default_dialect: &'static DialectId = default_dialect();
        let source = ResolvedSource::new(SourceRef::new(SourceId::new(0), identity), "<input>", bytes, 0);
        Self {
            catalog: CodecCatalog::new(&[]),
            source,
            files: None,
            program,
            input,
            requirement: None,
            input_format: FormatId::try_new("json").expect("built-in json format"),
            // The canonical RFC 8259 dialect is the SDK's default input and
            // output dialect; the codec crates register it under this id.
            input_dialect: DialectId::try_new("rfc8259").expect("built-in json dialect"),
            output_format: FormatId::try_new("json").expect("built-in json format"),
            output_dialect: DialectId::try_new("rfc8259").expect("built-in json dialect"),
            policy: PipelinePolicy {
                decode: DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: default_dialect,
                    options: None,
                    allow_adjacent_values: true,
                    value_separator: jqf_codec_json::VALUE_SEPARATORS,
                },
                encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
                preservation: PreservationRequest::None,
                encode_options: None,
                cooperative_credits: DEFAULT_COOPERATIVE_CREDITS,
                split: None,
                max_iterations: None,
            },
            framing: FacadeFraming::item_suffix(b"\n"),
            resources: None,
            diagnostics: None,
            label: "<input>",
            null_input: false,
            slurped: false,
            stream_events: false,
            stream_errors: false,
            editing: false,
            input_family: false,
            roundtrip: false,
            range_locate: false,
        }
    }

    /// Replaces the compiled program the request runs (the CLI's slurp-map
    /// rewrite compiles a transformed program and re-requests with it).
    #[must_use]
    pub fn with_program(mut self, program: &'request CompiledProgram) -> Self {
        self.program = program;
        self
    }

    /// Sets the codec catalog the drives select registrations from.
    #[must_use]
    pub fn with_catalog(mut self, catalog: CodecCatalog<'request, 'request>) -> Self {
        self.catalog = catalog;
        self
    }

    /// Sets the resolved input source (the retained bytes plus their
    /// identity and label), overriding the one `new` derived from the input.
    #[must_use]
    pub fn with_source(mut self, source: ResolvedSource<'input>) -> Self {
        self.source = source;
        self
    }

    /// Sets the per-file byte ranges of a multi-file concatenation, when
    /// files were named. Single-file and stdin requests leave this unset.
    #[must_use]
    pub fn with_files(mut self, files: &'request [SourceFileRange<'request>]) -> Self {
        self.files = Some(files);
        self
    }

    /// Sets the access requirement the codec binds, lowered by the caller
    /// from the program and its policy (the caller owns the lowering law:
    /// `try_requirement` vs `try_whole_document_requirement` differ per
    /// input model).
    #[must_use]
    pub fn with_requirement(mut self, requirement: &'request AccessRequirement) -> Self {
        self.requirement = Some(requirement);
        self
    }

    /// Sets the input format and dialect the input bytes are decoded as.
    #[must_use]
    pub fn with_format(mut self, format: FormatId, dialect: DialectId) -> Self {
        self.input_format = format;
        self.input_dialect = dialect;
        self
    }

    /// Sets the output format and dialect the results are encoded as.
    #[must_use]
    pub fn with_output_format(mut self, format: FormatId, dialect: DialectId) -> Self {
        self.output_format = format;
        self.output_dialect = dialect;
        self
    }

    /// Sets the pipeline policy (decode/encode options, diagnostics policy).
    #[must_use]
    pub fn with_policy(mut self, policy: PipelinePolicy<'request>) -> Self {
        self.policy = policy;
        self
    }

    /// Sets the facade framing (the item separator the host owns).
    #[must_use]
    pub fn with_framing(mut self, framing: FacadeFraming<'request>) -> Self {
        self.framing = framing;
        self
    }

    /// Sets the per-request resource context the run charges and cancels
    /// through.
    #[must_use]
    pub fn with_resources(mut self, resources: &'request mut ResourceContext<'context>) -> Self {
        self.resources = Some(resources);
        self
    }

    /// Attaches the retained diagnostic stream; `execute` records its
    /// route and failures into it. `None` (the default) is the `Off` policy.
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: Option<&'context Diagnostics>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Sets the source label the CURSOR drives report (`input`/
    /// `inputs` facts and `--stream` error lines read it). The other lanes
    /// take their label from the request's source (`with_source`), so a
    /// custom label reaches only the cursor drives.
    #[must_use]
    pub fn with_label(mut self, label: &'request str) -> Self {
        self.label = label;
        self
    }

    /// The reference's `-n`: the filter runs once over `null` with the input
    /// family served from the eager or streaming cursor.
    #[must_use]
    pub fn with_null_input(mut self) -> Self {
        self.null_input = true;
        self
    }

    /// The reference's `-s`: the filter runs once over the array of every
    /// decoded input.
    #[must_use]
    pub fn slurped(mut self) -> Self {
        self.slurped = true;
        self
    }

    /// The reference's `--stream`: the input is served as `[path, leaf]`
    /// events. `stream_errors` selects the `--stream-errors` spelling that
    /// turns parse refusals into `[message, path]` events instead of
    /// aborting.
    #[must_use]
    pub fn as_stream_events(mut self, stream_errors: bool) -> Self {
        self.stream_events = true;
        self.stream_errors = stream_errors;
        self
    }

    /// The reference's `--edit`: the whole document is the output subject
    /// and the source-preserving edit lane owns the run.
    #[must_use]
    pub fn editing(mut self) -> Self {
        self.editing = true;
        self
    }

    /// Serves the input family from the shared input-sequence cursor: `input`/
    /// `inputs`/`input_filename`/`input_line_number` read the decoded sequence
    /// through the engine's shared cursor, with each value marked current
    /// before its run — the reference's shared-stream model. Without this
    /// option the input family sees no attached source (the reference's `-n`
    /// detached state: `input` raises `break`, the position builtins answer
    /// their `-n` forms).
    #[must_use]
    pub fn with_input_family(mut self) -> Self {
        self.input_family = true;
        self
    }

    /// Asks `execute` to try the canonical-identity round-trip lane first:
    /// identity over one canonical document publishes the retained source
    /// verbatim, and declines (publishing nothing) to the ordinary drive.
    ///
    /// Deliberate SDK-public PROBE surface: production reaches the lane
    /// through the default serial ladder (which tries it first anyway), so
    /// naming the rung exists only so a probe can observe `Declined` instead
    /// of a silent floor — the shape the SDK's own round-trip tests use. A
    /// non-lenient mismatch policy refuses the named rung: both rungs answer
    /// from the codec's pushed-down prefix, which cannot fire a warn/strict
    /// mismatch cell.
    #[must_use]
    pub fn roundtrip(mut self) -> Self {
        self.roundtrip = true;
        self
    }

    /// Asks `execute` to try the bare-slice range-locate lane when the
    /// program is range-locate eligible, declining to the ordinary drive.
    ///
    /// Deliberate SDK-public PROBE surface: production reaches the lane
    /// through the default serial ladder; naming the rung exists so a probe
    /// can observe `Declined` instead of a silent floor — the shape the
    /// sdk-smoke receipt and the SDK's own range-locate tests use. A
    /// non-lenient mismatch policy refuses the named rung: both rungs answer
    /// from the codec's pushed-down prefix, which cannot fire a warn/strict
    /// mismatch cell.
    #[must_use]
    pub fn range_locate(mut self) -> Self {
        self.range_locate = true;
        self
    }
}

/// What a served drive published, in the drive's own report shape.
#[derive(Debug)]
pub enum Report {
    /// A value/sequence drive's summary (the sequence, null-first, slurped,
    /// input-family, and edit drives).
    Sequence(crate::drive::SequenceReport),
    /// A single-document drive's summary (the unified value drive, the
    /// round-trip lane, the range-locate lane).
    Pipeline(crate::drive::PipelineReport),
    /// A record drive's summary.
    Record(crate::drive::RecordSequenceReport),
    /// A `--stream` event drive's summary.
    EventStream(crate::drive::EventStreamReport),
}

/// Runs one request through the drive its input and options select, publishing
/// every item through `sink`.
///
/// This is the SDK's ONE entry point: route selection lives here, driven by
/// the request's input shape and option flags plus the compiled program's own
/// pushdown facts, and an embedder never names a route. The route-named
/// drives are `pub(crate)` and reachable only through this function.
///
/// # Errors
///
/// Returns [`Failure::Request`] when the request is missing a field the
/// selected drive requires; [`Failure::Read`] when a streaming read callback
/// fails; [`Failure::ParseRefused`] for a `--stream` parse refusal;
/// [`Failure::Pipeline`] for every pipeline, codec, registry, or sink failure.
#[expect(
    clippy::too_many_lines,
    reason = "the one-entry dispatcher routes to the crate's drives by input shape and option flags;               the per-drive bodies stay inline so the routing law reads as one obligation"
)]
pub fn execute<S: ItemSink>(request: Request<'_, '_, '_>, sink: &mut S) -> Result<Outcome, Failure>
where
    S::Error: fmt::Display,
{
    let Request {
        catalog,
        source,
        files,
        program,
        input,
        requirement,
        input_format,
        input_dialect,
        output_format,
        output_dialect,
        policy,
        framing,
        resources,
        diagnostics,
        label,
        null_input,
        slurped,
        stream_events,
        stream_errors,
        editing,
        input_family,
        roundtrip,
        range_locate,
    } = request;
    // One source of truth per route family: a DOCUMENT request's
    // `with_format` dialect and `policy.decode.dialect` must agree (a
    // mismatch means providers built under a dialect the request no longer
    // names — for `Whole` inputs they are one field by construction).
    // STREAMING and RECORD requests intentionally differ: `with_format`
    // names the CLI-facing registration (ndjson/json-seq/csv tails, or the
    // adjacent-value default) while the decode policy names the PAYLOAD
    // grammar the framer/cursor hands over (strict JSON).
    if matches!(input, Input::Whole(_)) {
        debug_assert_eq!(
            policy.decode.dialect, &input_dialect,
            "with_format and policy.decode.dialect must name the same input dialect",
        );
    }
    let resources = resources.ok_or_else(|| Failure::Request(RequestError::MissingField("resources")))?;
    let need_requirement = || -> Result<&'_ AccessRequirement, Failure> {
        requirement.ok_or_else(|| Failure::Request(RequestError::MissingField("requirement")))
    };

    // The source-preserving edit lane: the whole document is the output
    // subject. It declines (publishing nothing) when the lane cannot serve
    // the program, so the caller falls through to its ordinary route. A
    // RECORD input is edited per record: each framed payload decodes to a
    // source-backed document (the delimited spans), the program runs once
    // per record, and the patched payload is published with the record's OWN
    // authored terminator bytes.
    if editing {
        if let Input::Records { records, slot, .. } = input {
            let report = crate::drive::execute_record_source_edit(
                catalog,
                records,
                slot,
                source,
                &input_format,
                &input_dialect,
                program,
                &output_format,
                &output_dialect,
                policy,
                framing,
                resources,
                sink,
            )
            .map_err(map_pipeline)?;
            return Ok(Outcome::Served(Report::Record(report)));
        }
        let run = crate::drive::execute_source_edit(
            catalog,
            source,
            &input_format,
            &input_dialect,
            program,
            &output_format,
            &output_dialect,
            policy,
            framing,
            resources,
            sink,
        )
        .map_err(map_pipeline)?;
        return Ok(match run {
            crate::drive::EditRun::Completed(report) => Outcome::Served(Report::Sequence(report)),
            crate::drive::EditRun::Declined => Outcome::Declined,
        });
    }

    // The bounded `--stream` event route: the events ARE the input model,
    // served whole or streamed from the source itself.
    if stream_events {
        return match input {
            Input::Streaming(mut read) => {
                if null_input {
                    return Err(Failure::Request(RequestError::IncompatibleStreamEventsOption(
                        "null_input",
                    )));
                }
                if slurped {
                    return Err(Failure::Request(RequestError::IncompatibleStreamEventsOption(
                        "slurped",
                    )));
                }
                let report = crate::drive::execute_stream_events_streaming(
                    catalog,
                    stream_errors,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                    &mut read,
                )
                .map_err(map_streaming_event)?;
                Ok(Outcome::Served(Report::EventStream(report)))
            }
            Input::Whole(bytes) | Input::Records { source: bytes, .. } => {
                let report = if null_input {
                    crate::drive::execute_stream_events_null_first(
                        catalog,
                        bytes,
                        stream_errors,
                        slurped,
                        program,
                        &output_format,
                        &output_dialect,
                        policy,
                        framing,
                        resources,
                        sink,
                        label,
                    )
                } else {
                    crate::drive::execute_stream_events(
                        catalog,
                        bytes,
                        stream_errors,
                        slurped,
                        program,
                        &output_format,
                        &output_dialect,
                        policy,
                        framing,
                        resources,
                        sink,
                        label,
                    )
                }
                .map_err(map_event)?;
                Ok(Outcome::Served(Report::EventStream(report)))
            }
        };
    }

    match input {
        // Physically framed records: the record drive the input model names.
        Input::Records {
            source: _bytes,
            records,
            slot,
        } => {
            let requirement = need_requirement()?;
            let report = if null_input {
                crate::drive::execute_null_first_record_sequence(
                    catalog,
                    records,
                    slot,
                    source,
                    files,
                    &input_format,
                    &input_dialect,
                    requirement,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                )
            } else if slurped {
                crate::drive::execute_slurped_record_sequence(
                    catalog,
                    records,
                    slot,
                    source,
                    files,
                    &input_format,
                    &input_dialect,
                    requirement,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                )
            } else {
                crate::drive::execute_record_sequence(
                    catalog,
                    records,
                    slot,
                    source,
                    files,
                    &input_format,
                    &input_dialect,
                    requirement,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                )
            }
            .map_err(map_pipeline)?;
            Ok(Outcome::Served(Report::Record(report)))
        }

        // A retained buffer: the single-run models and the value ladder.
        Input::Whole(_) => {
            if null_input {
                let requirement = need_requirement()?;
                let report = crate::drive::execute_null_first_sequence(
                    catalog,
                    source,
                    files,
                    &input_format,
                    &input_dialect,
                    requirement,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                )
                .map_err(map_pipeline)?;
                return Ok(Outcome::Served(Report::Sequence(report)));
            }
            if slurped {
                let requirement = need_requirement()?;
                let report = crate::drive::execute_slurped_sequence(
                    catalog,
                    source,
                    files,
                    &input_format,
                    &input_dialect,
                    requirement,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                )
                .map_err(map_pipeline)?;
                return Ok(Outcome::Served(Report::Sequence(report)));
            }
            // Exclusive rungs: a caller that named one rung (the SDK tests)
            // still gets Declined rather than a silent floor, so a probe can
            // see the decline. The default ladder below is what the CLI
            // serial tail and the FFI take — one execute, fall-through inside.
            // These probes SKIP the mismatch gate the default ladder applies:
            // both rungs answer from the codec's pushed-down prefix, which
            // cannot fire a warn/strict mismatch cell, so a non-lenient
            // caller must not name them.
            if roundtrip {
                let run = crate::drive::execute_source_roundtrip(
                    catalog,
                    source,
                    &input_format,
                    &input_dialect,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                    false,
                )
                .map_err(map_pipeline)?;
                return Ok(match run {
                    crate::drive::RoundtripRun::Published(report) => Outcome::Served(Report::Pipeline(report)),
                    crate::drive::RoundtripRun::Encoded(_) => {
                        unreachable!("exclusive roundtrip never encodes the floor")
                    }
                    crate::drive::RoundtripRun::Declined => Outcome::Declined,
                });
            }
            if range_locate {
                let requirement = need_requirement()?;
                let run = crate::drive::execute_range_locate(
                    catalog,
                    source,
                    &input_format,
                    &input_dialect,
                    requirement,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                )
                .map_err(map_pipeline)?;
                return Ok(match run {
                    crate::drive::RangeLocateRun::Completed(report) => Outcome::Served(Report::Pipeline(report)),
                    crate::drive::RangeLocateRun::NotSingleDocument | crate::drive::RangeLocateRun::NotApplicable => {
                        Outcome::Declined
                    }
                });
            }
            // Default serial ladder: roundtrip, range-locate, then the floor.
            // A declined roundtrip that already decoded a single document
            // encodes it (RoundtripRun::Encoded) so the floor does not decode
            // twice. Non-lenient mismatch skips the echo/range rungs — the
            // codec's pushed-down prefix cannot report a cell.
            if resources.mismatch_policy() == jqf_resource::policy::MismatchPolicy::Lenient {
                let run = crate::drive::execute_source_roundtrip(
                    catalog,
                    source,
                    &input_format,
                    &input_dialect,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                    true,
                )
                .map_err(map_pipeline)?;
                match run {
                    crate::drive::RoundtripRun::Published(report) => {
                        if let Some(diagnostics) = diagnostics {
                            diagnostics.record_route_named("roundtrip");
                        }
                        return Ok(Outcome::Served(Report::Pipeline(report)));
                    }
                    crate::drive::RoundtripRun::Encoded(report) => {
                        if let Some(diagnostics) = diagnostics {
                            diagnostics.record_route_named("sequence");
                        }
                        return Ok(Outcome::Served(Report::Pipeline(report)));
                    }
                    crate::drive::RoundtripRun::Declined => {}
                }
                if program.range_locate_eligible()
                    && let Ok(requirement) = program.try_range_locate_requirement(resources)
                {
                    let run = crate::drive::execute_range_locate(
                        catalog,
                        source,
                        &input_format,
                        &input_dialect,
                        &requirement,
                        &output_format,
                        &output_dialect,
                        policy,
                        framing,
                        resources,
                        sink,
                    )
                    .map_err(map_pipeline)?;
                    if let crate::drive::RangeLocateRun::Completed(report) = run {
                        if let Some(diagnostics) = diagnostics {
                            diagnostics.record_route_named("range-locate");
                        }
                        return Ok(Outcome::Served(Report::Pipeline(report)));
                    }
                }
            }
            // The floor: the single-document drive for a non-adjacent
            // input (a format without the adjacent-value capability, or a
            // decode policy that refused them), the shared-cursor
            // input-sequence drive for an input-family program, and the
            // adjacent-value sequence drive otherwise. The input-model fact
            // comes from the codec's own route capabilities declaration plus
            // the policy's own opt-in, never a hand-written list.
            let adjacent = policy.decode.allow_adjacent_values
                && catalog
                    .route_capabilities(&input_format, &input_dialect)
                    .is_ok_and(|caps| caps.contains(&jqf_codec_core::RouteCapability::AdjacentValues));
            let requirement = need_requirement()?;
            if !adjacent {
                let report = crate::drive::execute_value_document(
                    catalog,
                    source,
                    &input_format,
                    &input_dialect,
                    requirement,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    diagnostics,
                    sink,
                )
                .map_err(map_pipeline)?;
                Ok(Outcome::Served(Report::Pipeline(report)))
            } else if input_family {
                let report = crate::drive::execute_input_sequence(
                    catalog,
                    source,
                    files,
                    &input_format,
                    &input_dialect,
                    requirement,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                )
                .map_err(map_pipeline)?;
                if let Some(diagnostics) = diagnostics {
                    diagnostics.record_route_named("sequence");
                }
                Ok(Outcome::Served(Report::Sequence(report)))
            } else {
                let report = crate::drive::execute_sequence(
                    catalog,
                    source,
                    files,
                    &input_format,
                    &input_dialect,
                    requirement,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                )
                .map_err(map_pipeline)?;
                if let Some(diagnostics) = diagnostics {
                    diagnostics.record_route_named("sequence");
                }
                Ok(Outcome::Served(Report::Sequence(report)))
            }
        }

        // A live read source: the streaming drives, selected by the same
        // program facts as the whole-input floor.
        Input::Streaming(mut read) => {
            if null_input {
                let report = crate::drive::execute_null_first_sequence_streaming(
                    catalog,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                    label,
                    move |buffer| read(buffer).map_err(|failure| failure.message),
                )
                .map_err(map_pipeline)?;
                return Ok(Outcome::Served(Report::Sequence(report)));
            }
            if input_family {
                let report = crate::drive::execute_input_sequence_streaming(
                    catalog,
                    program,
                    &output_format,
                    &output_dialect,
                    policy,
                    framing,
                    resources,
                    sink,
                    label,
                    move |buffer| read(buffer).map_err(|failure| failure.message),
                )
                .map_err(map_pipeline)?;
                return Ok(Outcome::Served(Report::Sequence(report)));
            }
            let report = crate::drive::execute_sequence_streaming(
                catalog,
                &input_format,
                &input_dialect,
                // The streaming adjacent-value drive needs the requirement
                // exactly like the whole-input sequence drive does.
                need_requirement()?,
                program,
                &output_format,
                &output_dialect,
                policy,
                framing,
                resources,
                sink,
                &mut read,
            )
            .map_err(map_streaming)?;
            Ok(Outcome::Served(Report::Sequence(report)))
        }
    }
}

/// Maps a pipeline error, erasing the host sink error to its Display text so
/// [`Failure`] stays non-generic.
fn map_pipeline<SinkError: fmt::Display>(error: crate::drive::PipelineError<SinkError>) -> Failure {
    Failure::Pipeline(crate::drive::erase_sink(error, |error| error.to_string()))
}

/// Maps a streaming adjacent-value error.
fn map_streaming<SinkError: fmt::Display>(
    error: crate::drive::StreamingSequenceError<SinkError, ReadFailure>,
) -> Failure {
    match error {
        crate::drive::StreamingSequenceError::Read(read) => Failure::Read(read),
        crate::drive::StreamingSequenceError::Pipeline(error) => map_pipeline(error),
    }
}

/// Maps a whole-input `--stream` event error.
fn map_event<SinkError: fmt::Display>(error: crate::drive::EventStreamError<SinkError>) -> Failure {
    match error {
        crate::drive::EventStreamError::Pipeline(error) => map_pipeline(error),
        crate::drive::EventStreamError::ParseRefused(message) => Failure::ParseRefused(message),
    }
}

/// Maps a streaming `--stream` event error.
fn map_streaming_event<SinkError: fmt::Display>(
    error: crate::drive::StreamingEventStreamError<SinkError, ReadFailure>,
) -> Failure {
    match error {
        crate::drive::StreamingEventStreamError::Pipeline(error) => map_pipeline(error),
        crate::drive::StreamingEventStreamError::Read(read) => Failure::Read(read),
        crate::drive::StreamingEventStreamError::ParseRefused(message) => Failure::ParseRefused(message),
    }
}
