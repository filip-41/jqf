//! The record-parallel drive: ordered morsels over one record stream.
//!
//! The ordering law, the clean-morsel law, and the yield-to-serial recovery this route rests on live in
//! [`crate::parallel::relay`]; what lives here is the record route's own drive — the framer, the payload ladder, and
//! the record-aligned partition — plus the report it publishes.
//!
//! Three input kinds drive today: NDJSON, json-seq, and CSV/TSV. The framer owns physical boundaries; the payload codec
//! owns every byte inside a record.
//!
//! [`crate::parallel::relay`]: crate::parallel

mod plan;

use core::fmt;

use jqf_codec_core::{CodecError, ErasedRecordStreamProvider, RecordProviderOpen};
use jqf_codec_delimited::CsvDecodeOptions;
use jqf_codec_json::{
    JsonEncodeOptions,
    ndjson::{NdjsonDecodeOptions, NdjsonProfile, NdjsonTerminator},
    seq::{JsonSeqDecodeOptions, JsonSeqProfile},
};
use jqf_data::FormatId;
use jqf_engine::CompiledProgram;
use jqf_resource::{ControlError, ResourceContext, ResourceError};
use jqf_sdk::{CodecCatalog, Input, ItemSink, Outcome, PipelinePolicy, RecordSequenceReport, Report};
use jqf_source::ResolvedSource;

pub use plan::plan_request;
pub(crate) use plan::{partition_csv_morsels, partition_json_seq_morsels, partition_morsels};

pub use crate::parallel::{ParallelPlan, PlanDecision, WORKER_HARD_CAP, WorkerRequest};

pub use crate::output::OutputTarget;
use crate::output::{build_request, built_in_dialect, built_in_format};
use crate::parallel::{MorselDrive, MorselSink, MorselUnits, Relay, SkippingSink, SliceMorsels, relay_morsels};
use crate::workers::MorselFallbackCause;

/// Which record format a record drive frames.
///
/// Kept as plain `Copy` data so a worker can rebuild the drive from its own numeric grant, exactly like every other
/// field of [`RecordDriveSpec`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordInputKind {
    /// NDJSON: newline-delimited strict-JSON records.
    Ndjson,
    /// JSON Text Sequences (RFC 7464): RS-delimited strict-JSON records.
    JsonSeq,
    /// CSV: RFC 4180 records whose payloads are also CSV rows.
    Csv {
        /// Whether the FIRST record is a header rather than data. Under the headered dialect the framer consumes it and
        /// every later record publishes as an object keyed by its names; under the array dialect every record publishes
        /// as an array of its fields.
        header: bool,
        /// Whether this is the TSV grammar: tab delimiter, no quote. Under the CSV grammar `--csv-delimiter` may name
        /// any accepted byte; under the TSV grammar the delimiter is bound and the dial is a usage error.
        tsv: bool,
    },
}

impl RecordInputKind {
    /// Whether this kind's meaning depends on records OUTSIDE any one morsel.
    ///
    /// The headered dialects do: record zero defines every later record's member names, so a worker handed a middle
    /// byte range would read its own first row as a header and drop it. Such a kind is fenced off the morsel lane,
    /// which is what keeps a worker's bytes equal to serial's bytes.
    #[must_use]
    pub const fn is_stream_stateful(self) -> bool {
        matches!(self, Self::Csv { header: true, .. })
    }
}

/// Installs the process-lifetime catalog of the record formats the drives serve.
///
/// The record and value drives name the record formats' registrations only through this catalog — never the codec
/// crates that implement them — and a worker rebuilds its drive from the spec's copy, so the registrations must outlive
/// every thread. The nine `CodecRegistration` values are `'static` by construction (the CLI and the FFI build identical
/// ones from the same codec crates), so the first installation leaks them deliberately, exactly as the CLI's own full
/// catalog already is; later calls return the same catalog.
///
/// The JSON registration rides along because the drives decode every record payload and encode every item through the
/// same catalog: the payload is always strict JSON for NDJSON/json-seq and the record codecs' own rows for CSV/TSV.
#[allow(
    clippy::too_many_arguments,
    reason = "one line per served codec, exactly like the facades that build these registrations"
)]
pub fn install_record_catalog(
    json: jqf_codec_core::CodecRegistration<'static>,
    ndjson: jqf_codec_core::CodecRegistration<'static>,
    json_seq: jqf_codec_core::CodecRegistration<'static>,
    csv: jqf_codec_core::CodecRegistration<'static>,
    tsv: jqf_codec_core::CodecRegistration<'static>,
    render: jqf_codec_core::CodecRegistration<'static>,
    yaml: jqf_codec_core::CodecRegistration<'static>,
    xml: jqf_codec_core::CodecRegistration<'static>,
    html: jqf_codec_core::CodecRegistration<'static>,
) -> CodecCatalog<'static, 'static> {
    use std::sync::OnceLock;
    static RECORD_CATALOG: OnceLock<CodecCatalog<'static, 'static>> = OnceLock::new();
    *RECORD_CATALOG.get_or_init(|| {
        let registrations: [&'static jqf_codec_core::CodecRegistration<'static>; 9] = [
            Box::leak(Box::new(json)),
            Box::leak(Box::new(ndjson)),
            Box::leak(Box::new(json_seq)),
            Box::leak(Box::new(csv)),
            Box::leak(Box::new(tsv)),
            Box::leak(Box::new(render)),
            Box::leak(Box::new(yaml)),
            Box::leak(Box::new(xml)),
            Box::leak(Box::new(html)),
        ];
        let registrations: &'static [&'static jqf_codec_core::CodecRegistration<'static>] =
            Box::leak(Box::new(registrations));
        let index = Box::leak(Box::new(jqf_sdk::CatalogIndex::build(registrations)));
        CodecCatalog::new(registrations).with_index(index)
    })
}

/// Which INPUT MODEL one record drive runs under.
///
/// Three input models read a record stream the same way they read adjacent values — the framer decides where a record
/// ends, and the model decides how many times the program runs over how many of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordRunModel {
    /// One program run per record, in ordinal order.
    PerRecord,
    /// `-s`: every record is collected into one array and the program runs once over it. No input cursor is attached
    /// afterwards, exactly as after an adjacent-value slurp.
    Slurped,
    /// `-n`: the program runs once over `null` with the record stream attached as the shared `input`/`inputs` cursor.
    NullFirst,
}

impl RecordRunModel {
    /// Whether this model runs the program exactly once over the whole stream.
    #[must_use]
    pub const fn is_single_run(self) -> bool {
        matches!(self, Self::Slurped | Self::NullFirst)
    }
}

/// How a request's output is framed.
///
/// Kept as plain `Copy` fields rather than as built codec options so a worker can rebuild the options inside its own
/// child ledger.
#[derive(Clone, Copy, Debug)]
pub struct RecordOutputSpec {
    /// Which family of bytes this drive publishes.
    pub target: OutputTarget,
    /// The NDJSON terminator, meaningful only under [`OutputTarget::Ndjson`].
    pub terminator: NdjsonTerminator,
    /// The JSON render style (indentation, `-r` raw strings, `-S` sort keys, `-a` ascii output), meaningful under JSON
    /// and json-seq targets.
    ///
    /// A worker rebuilds the whole drive from this spec, so a style left out here is a style the worker cannot know
    /// about: the parallel route would publish the codec's compact default while the serial route published what the
    /// caller asked for, and the two would disagree byte for byte.
    pub json: JsonEncodeOptions,
    /// See [`crate::JsonItemSuffix`].
    pub no_newline: bool,
}

/// Everything one record drive needs, in a form that crosses a thread.
///
/// Every field is `Copy` and thread-safe by construction: borrowed immutable bytes and plain values. No account, no
/// document, no provider, no encoder, and no sink appears here, which is what lets a worker rebuild the whole drive
/// from its own numeric grant.
#[derive(Clone, Copy, Debug)]
pub struct RecordDriveSpec<'request> {
    /// The whole retained input.
    pub input: &'request [u8],
    /// The source name diagnostics render.
    pub source_name: &'request str,
    /// Per-file byte ranges of a multi-file concatenation, when files were named. The record drive attributes a
    /// per-value error to the file holding its last byte. Stdin, follow, serve, and the feed leave this unset.
    pub files: Option<&'request [jqf_source::SourceFileRange<'request>]>,
    /// Which record format this drive frames.
    pub kind: RecordInputKind,
    /// The framing profile the request selected (NDJSON only).
    pub profile: NdjsonProfile,
    /// The json-seq framing profile the request selected (`--seq`'s flag- scoped recovering route, or strict for an
    /// explicit json-seq selection).
    pub json_seq_profile: JsonSeqProfile,
    /// The CSV field delimiter : `None` is RFC 4180's comma; `Some` selects the TSV dialect and friends. Meaningful
    /// only when `kind` is `Csv`.
    pub csv_delimiter: Option<u8>,
    /// The CSV alphabet freeze: `true` decodes every record under the RFC 4180 TEXTDATA law (`csv.rfc4180@1`), `false`
    /// under the Unicode-capable `csv.utf8@1` family. Meaningful only when `kind` is `Csv`; TSV ignores it.
    pub csv_textdata: bool,
    /// The record ceiling the request selected.
    pub max_record_bytes: u64,
    /// The process-lifetime catalog of the record formats the drive serves catalog, installed once through
    /// [`install_record_catalog`]. Every field of this spec crosses a thread; the catalog is `'static` so the worker
    /// path can rebuild the drive from its own numeric grant.
    pub catalog: CodecCatalog<'static, 'static>,
    /// Output framing.
    pub output: RecordOutputSpec,
    /// Which input model the program runs under.
    pub model: RecordRunModel,
    /// Whether this is the source-preserving EDIT lane: every record is patched in place rather than re-encoded, and
    /// the drive is serial by construction (a morsel worker cannot splice).
    pub edit: bool,
    /// Credits installed on every cooperative resume.
    pub cooperative_credits: u32,
    /// The frame-transition ceiling (`--max-iterations`): a crossing is a machine resource refusal, exactly as on the
    /// serial tail. `None` is the uncapped default; a host surface that cannot carry the dial passes `None`.
    pub max_iterations: Option<u64>,
}

/// A record drive's failure, at the boundary that raised it.
#[derive(Debug)]
pub enum RecordDriveError<SinkError> {
    /// Construction before the pipeline ran: requirement lowering, the record ceiling, or the record provider. The
    /// static name says which.
    Setup {
        /// The construction step that failed.
        step: &'static str,
        /// The codec's own failure.
        error: CodecError,
    },
    /// The record pipeline's own failure, unchanged.
    Pipeline(jqf_sdk::Failure),
    /// The caller's sink refused a relayed morsel's bytes.
    Sink(SinkError),
    /// A worker envelope or the parent ledger refused the request.
    Resource(ResourceError),
    /// The host cancelled the request, the deadline expired, or the physical memory ceiling was exceeded.
    Control(ControlError),
}

/// Completion summary of one record request, serial or parallel.
///
/// The production reader is the [`fmt::Display`] impl below (the CLI's relay line); the plan/relay facts stay private
/// fields beside it.
#[derive(Clone, Copy, Debug)]
pub struct RecordRequestReport {
    records: u64,
    items: u64,
    issues: u64,
    error_issues: u64,
    /// How many per-record CODEC failures (`RawNulByte`) the drive continued past — promotable warnings under
    /// `--strictness strict`.
    codec_error_count: u64,
    morsels_published: u64,
    fell_back: Option<MorselFallbackCause>,
    degraded: bool,
    cancelled_worker_panics: u32,
}

impl RecordRequestReport {
    /// Records whose payload was decoded and run.
    #[must_use]
    pub const fn records(self) -> u64 {
        self.records
    }

    /// Ordered items published across every record.
    #[must_use]
    pub const fn items(self) -> u64 {
        self.items
    }

    /// Ordinals that produced an issue instead of a value.
    #[must_use]
    pub const fn issues(self) -> u64 {
        self.issues
    }

    /// Issues whose severity FORCES the request's failure class.
    #[must_use]
    pub const fn error_issues(self) -> u64 {
        self.error_issues
    }

    /// How many per-record codec failures the drive continued past.
    #[must_use]
    pub const fn codec_value_errors(self) -> u64 {
        self.codec_error_count
    }

    fn from_sequence(
        sequence: RecordSequenceReport,
        morsels_published: u64,
        fell_back: Option<MorselFallbackCause>,
        cancelled_worker_panics: u32,
    ) -> Self {
        Self {
            records: sequence.records(),
            items: sequence.items(),
            issues: sequence.issues(),
            error_issues: sequence.error_issues(),
            codec_error_count: sequence.codec_value_errors(),
            morsels_published,
            fell_back,
            degraded: fell_back.is_some(),
            cancelled_worker_panics,
        }
    }
}

/// The relay half of a request's diagnostics.
///
/// The PLAN is printable before the drive runs, and the relay only afterwards, so they are two lines rather than one: a
/// request that fails still gets to say what it planned, which is what lets a differential harness confirm the lane
/// actually engaged on an input it also expects to fail.
impl fmt::Display for RecordRequestReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fallback = match self.fell_back {
            None => "none",
            Some(MorselFallbackCause::Diagnostics) => "diagnostics",
            Some(MorselFallbackCause::DriveFailed) => "drive-failed",
            Some(MorselFallbackCause::WorkerUnavailable) => "worker-unavailable",
        };
        write!(
            formatter,
            "relay: morsels_published={} fallback={fallback} records={} items={} degraded={} worker_panics={}",
            self.morsels_published, self.records, self.items, self.degraded, self.cancelled_worker_panics
        )?;
        Ok(())
    }
}

/// Whether the request's framing profile can serve a morsel worker.
///
/// Only a STRICT profile makes an unclean morsel unambiguous: its framing faults are terminal, so a morsel either
/// publishes exactly what serial would for its byte range or reports that it did not. A RECOVERING framer turns faults
/// into ordered issues carrying absolute offsets — diagnostics a worker holding one morsel's bytes cannot render — so
/// recovering NDJSON and the `--seq` flag's recovering json-seq drive serial. CSV/TSV have no recovering profile.
/// Checked HERE as well as in every caller's planner so the law is structural rather than a property of one planner;
/// the CLI declines the same requests with the printable `input-ineligible` decision.
fn framing_is_morsel_eligible(spec: &RecordDriveSpec<'_>) -> bool {
    match spec.kind {
        RecordInputKind::Ndjson => spec.profile == NdjsonProfile::Strict,
        RecordInputKind::JsonSeq => spec.json_seq_profile == JsonSeqProfile::Strict,
        RecordInputKind::Csv { .. } => true,
    }
}

/// Runs one record request (NDJSON, json-seq, or CSV/TSV), in parallel when the plan says so.
///
/// A serial plan drives exactly the code path the switch-off drives; there is no one-worker coordinator hiding behind
/// it.
///
/// # Errors
///
/// Returns the record drive's own failure. The parallel lane raises no error of its own: every coordinator, grant, or
/// worker problem yields to serial instead, and only the serial redrive's failure can surface.
pub fn execute_record_request<Sink: ItemSink>(
    spec: RecordDriveSpec<'_>,
    plan: ParallelPlan,
    program: &CompiledProgram,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    split: Option<&jqf_engine::CompiledProgram>,
) -> Result<RecordRequestReport, RecordDriveError<Sink::Error>>
where
    Sink::Error: core::fmt::Display,
{
    // A single-run model, a stateful stream, a NON-STRICT framing profile, a stateful output dialect, a split
    // destination, or an edit request is planned serial by every caller; re-checking it here makes the clean-morsel law
    // structural rather than a property of one planner. A worker cutting a headered stream would read its own first row
    // as a header, a headered output would repeat its header once per morsel, a single-run model has no per-record
    // morsel to relay, and a split destination names items the relay never sees — none of those is serial's bytes. A
    // recovering framer's issues are ordered diagnostics carrying absolute offsets, which a worker seeing one byte
    // range cannot render, so only a strict framing profile is morsel-eligible.
    if !plan.is_parallel()
        || spec.model.is_single_run()
        || spec.kind.is_stream_stateful()
        || !framing_is_morsel_eligible(&spec)
        || spec.output.target.is_stateful()
        || spec.edit
        || split.is_some()
    {
        let sequence = drive_record_range(spec, spec.input, program, resources, sink, split)?;
        return Ok(RecordRequestReport::from_sequence(sequence, 0, None, 0));
    }
    let morsels = match spec.kind {
        RecordInputKind::JsonSeq => partition_json_seq_morsels(spec.input, plan.morsel_bytes()),
        // TSV has no quote state, so a cut at a line feed is exact exactly as under NDJSON.
        RecordInputKind::Ndjson | RecordInputKind::Csv { tsv: true, .. } => {
            partition_morsels(spec.input, plan.morsel_bytes())
        }
        RecordInputKind::Csv { tsv: false, .. } => partition_csv_morsels(spec.input, plan.morsel_bytes()),
    };
    match relay_morsels(spec, plan, SliceMorsels::new(&morsels), program, resources, sink) {
        Relay::Complete {
            units: records,
            items,
            morsels_published,
            degraded,
            cancelled_worker_panics,
        } => {
            // The relay charges no input here. The request's input bytes belong to whoever READ them — the host, once —
            // and a drive that charged the slice it borrows would count the same read again on every lane the request
            // takes.
            Ok(RecordRequestReport {
                records,
                items,
                issues: 0,
                error_issues: 0,
                codec_error_count: 0,
                morsels_published,
                fell_back: None,
                degraded,
                cancelled_worker_panics,
            })
        }
        Relay::SinkFailed(error) => Err(RecordDriveError::Sink(error)),
        Relay::Refused(error) => Err(RecordDriveError::Resource(error)),
        Relay::Control(error) => Err(RecordDriveError::Control(error)),
        Relay::YieldToSerial {
            published_bytes,
            morsels_published,
            cause,
            cancelled_worker_panics,
        } => {
            // Serial's own drive, over the whole input, minus the bytes the clean prefix already wrote. What follows is
            // serial verbatim.
            let mut skipping = SkippingSink::new(sink, published_bytes);
            let sequence = drive_record_range(spec, spec.input, program, resources, &mut skipping, split)?;
            Ok(RecordRequestReport::from_sequence(
                sequence,
                morsels_published,
                Some(cause),
                cancelled_worker_panics,
            ))
        }
    }
}

/// Runs the serial record drive over `bytes`, publishing through `sink`.
///
/// This is the ONE record drive in the process: the serial path calls it over the whole input, a worker calls it over
/// its morsel, and the fallback calls it again over the whole input. There is no second decode ladder — every caller
/// shares this one, so their bytes cannot diverge.
///
/// # Errors
///
/// Returns the construction or pipeline failure the record drive raised.
#[expect(
    clippy::too_many_lines,
    reason = "the ONE record drive: registration, framing, per-record decode and the report are a single sequence, and splitting out a second ladder would let the callers' bytes diverge"
)]
pub(crate) fn drive_record_range<Sink: ItemSink>(
    spec: RecordDriveSpec<'_>,
    bytes: &[u8],
    program: &CompiledProgram,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    split: Option<&jqf_engine::CompiledProgram>,
) -> Result<RecordSequenceReport, RecordDriveError<Sink::Error>>
where
    Sink::Error: core::fmt::Display,
{
    let model = spec.model;
    // The record drive lowers the per-record requirement as the program's own path class names it, exactly as the CLI's
    // non-record lanes do; Slurped materializes every record into an owned value before the one run, so each record's
    // decode must produce a complete document whatever the program's own path class is. NullFirst decodes each pulled
    // record on demand under the pulled-record prune hint. The EDIT lane lowers the eager whole document, exactly as
    // the non-record edit drive does — the codec must never push down a prefix the diff walk then cannot read.
    let requirement = match model {
        RecordRunModel::NullFirst => {
            program
                .try_pulled_record_requirement(resources)
                .map_err(|error| RecordDriveError::Setup {
                    step: "requirement",
                    error,
                })?
        }
        RecordRunModel::Slurped => {
            program
                .try_whole_document_requirement(resources)
                .map_err(|error| RecordDriveError::Setup {
                    step: "requirement",
                    error,
                })?
        }
        RecordRunModel::PerRecord if spec.edit => {
            program
                .try_whole_document_requirement(resources)
                .map_err(|error| RecordDriveError::Setup {
                    step: "requirement",
                    error,
                })?
        }
        RecordRunModel::PerRecord => program
            .try_requirement(resources)
            .map_err(|error| RecordDriveError::Setup {
                step: "requirement",
                error,
            })?,
    };
    build_request(
        spec.output.target,
        spec.output.terminator,
        spec.output.json,
        spec.output.no_newline,
        spec.csv_delimiter,
        spec.catalog,
        spec.source_name,
        bytes,
        // One record holds exactly one complete text.
        false,
        spec.cooperative_credits,
        spec.max_iterations,
        split,
        |request| match spec.kind {
            RecordInputKind::Ndjson => {
                let options = NdjsonDecodeOptions::try_new(None, spec.max_record_bytes).map_err(|error| {
                    RecordDriveError::Setup {
                        step: "record-ceiling",
                        error,
                    }
                })?;
                let records = open_record_provider(
                    request.catalog,
                    request.source,
                    RecordProviderOpen::Ndjson {
                        recovering: matches!(spec.profile, NdjsonProfile::Recovering),
                        max_record_bytes: options.max_record_bytes(),
                    },
                    resources,
                )
                .map_err(|error| RecordDriveError::Setup {
                    step: "record-provider",
                    error,
                })?;
                drive_record_model(
                    model,
                    spec.edit,
                    request.catalog,
                    records,
                    jqf_codec_core::RECORD_ROUTE_SLOT,
                    request.source,
                    &request.format,
                    &request.dialect,
                    &requirement,
                    program,
                    &request.output_format,
                    &request.output_dialect,
                    request.policy,
                    request.framing,
                    resources,
                    sink,
                    spec.files,
                )
            }
            RecordInputKind::JsonSeq => {
                let options = JsonSeqDecodeOptions::try_new(None, spec.max_record_bytes).map_err(|error| {
                    RecordDriveError::Setup {
                        step: "record-ceiling",
                        error,
                    }
                })?;
                let records = open_record_provider(
                    request.catalog,
                    request.source,
                    RecordProviderOpen::JsonSeq {
                        recovering: matches!(spec.json_seq_profile, JsonSeqProfile::Recovering),
                        max_record_bytes: options.max_record_bytes(),
                    },
                    resources,
                )
                .map_err(|error| RecordDriveError::Setup {
                    step: "record-provider",
                    error,
                })?;
                drive_record_model(
                    model,
                    spec.edit,
                    request.catalog,
                    records,
                    jqf_codec_core::RECORD_ROUTE_SLOT,
                    request.source,
                    &request.format,
                    &request.dialect,
                    &requirement,
                    program,
                    &request.output_format,
                    &request.output_dialect,
                    request.policy,
                    request.framing,
                    resources,
                    sink,
                    spec.files,
                )
            }
            RecordInputKind::Csv { header, tsv } => {
                // The accepted-alphabet law picks the constructor: the RFC-named dialects enforce TEXTDATA, the default
                // is the Unicode-capable utf8 family.
                let options = if tsv {
                    CsvDecodeOptions::try_new_tsv(Some(spec.max_record_bytes), spec.max_record_bytes, header)
                } else if spec.csv_textdata {
                    CsvDecodeOptions::try_new_rfc4180(
                        spec.csv_delimiter,
                        Some(spec.max_record_bytes),
                        spec.max_record_bytes,
                        header,
                    )
                } else {
                    CsvDecodeOptions::try_new(
                        spec.csv_delimiter,
                        Some(spec.max_record_bytes),
                        spec.max_record_bytes,
                        header,
                    )
                }
                .map_err(|error| RecordDriveError::Setup {
                    step: "record-ceiling",
                    error,
                })?;
                let records = open_record_provider(
                    request.catalog,
                    request.source,
                    RecordProviderOpen::Delimited {
                        delimiter: options.delimiter(),
                        header: options.header(),
                        quote: options.quote(),
                        max_record_bytes: options.max_record_bytes(),
                    },
                    resources,
                )
                .map_err(|error| RecordDriveError::Setup {
                    step: "record-provider",
                    error,
                })?;
                // The payload format is the GRAMMAR's own: the TSV grammar decodes records as `tsv`, the CSV grammar as
                // `csv`, never a hard-coded format id.
                let format = built_in_format(options.format_id());
                let dialect = built_in_dialect(options.dialect_id());
                // The payload provider learns header mode from the SAME normalized options the framer was opened with,
                // so the two can never disagree on the row shape.
                let mut policy = request.policy;
                policy.decode.dialect = &dialect;
                policy.decode.options = Some(&options as &(dyn core::any::Any + Send + Sync));
                drive_record_model(
                    model,
                    spec.edit,
                    request.catalog,
                    records,
                    jqf_codec_core::RECORD_ROUTE_SLOT,
                    request.source,
                    &format,
                    &dialect,
                    &requirement,
                    program,
                    &request.output_format,
                    &request.output_dialect,
                    policy,
                    request.framing,
                    resources,
                    sink,
                    spec.files,
                )
            }
        },
    )
}

/// Opens one record provider through the request catalog's registered record-provider factory: the runtime names the
/// FORMAT, never the codec crate that implements it. Returns the codec's own failure; the caller wraps it in its
/// `Setup` arm.
///
/// The format identity is derived from the open envelope's variant, so the registered factory and the payload policy
/// can never disagree on which format was requested.
fn open_record_provider<'source>(
    catalog: CodecCatalog<'_, '_>,
    source: ResolvedSource<'source>,
    open: RecordProviderOpen,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedRecordStreamProvider<'source>, CodecError> {
    let format_text = match open {
        RecordProviderOpen::Ndjson { .. } => jqf_codec_core::record_options::NDJSON_FORMAT_ID,
        RecordProviderOpen::JsonSeq { .. } => jqf_codec_core::record_options::JSON_SEQ_FORMAT_ID,
        RecordProviderOpen::Delimited { quote, .. } => {
            if quote.is_some() {
                jqf_codec_core::record_options::CSV_FORMAT_ID
            } else {
                jqf_codec_core::record_options::TSV_FORMAT_ID
            }
        }
    };
    let format = FormatId::try_new(format_text).map_err(|_| {
        CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
            contract: "built-in record format identity",
        })
    })?;
    let factory = catalog.record_provider(&format).map_err(|_| {
        CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
            contract: "record-provider registration",
        })
    })?;
    factory.create_provider(source, open, resources)
}

/// Dispatches one opened record stream to the SDK drive its input model names.
///
/// The three drives take the same inventory on purpose: the model is the ONLY thing that differs between them, so
/// selecting here keeps every record format's construction identical across all three.
#[allow(
    clippy::too_many_arguments,
    reason = "the record drive's own boundary inventory, forwarded unchanged to the SDK"
)]
fn drive_record_model<'source, Sink: ItemSink>(
    model: RecordRunModel,
    edit: bool,
    catalog: CodecCatalog<'_, '_>,
    records: jqf_codec_core::ErasedRecordStreamProvider<'source>,
    record_slot: jqf_codec_core::RouteSlot,
    source: ResolvedSource<'source>,
    input_format: &jqf_data::FormatId,
    input_dialect: &jqf_data::DialectId,
    requirement: &jqf_codec_core::AccessRequirement,
    program: &CompiledProgram,
    output_format: &jqf_data::FormatId,
    output_dialect: &jqf_data::DialectId,
    policy: PipelinePolicy<'_>,
    framing: jqf_sdk::FacadeFraming<'_>,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    files: Option<&[jqf_source::SourceFileRange<'_>]>,
) -> Result<RecordSequenceReport, RecordDriveError<Sink::Error>>
where
    Sink::Error: core::fmt::Display,
{
    let mut request = jqf_sdk::Request::new(
        program,
        Input::Records {
            source: source.bytes(),
            records,
            slot: record_slot,
        },
    )
    .with_catalog(catalog)
    .with_source(source)
    .with_format(input_format.clone(), input_dialect.clone())
    .with_output_format(output_format.clone(), output_dialect.clone())
    .with_policy(policy)
    .with_framing(framing)
    .with_resources(resources)
    .with_requirement(requirement);
    match model {
        RecordRunModel::PerRecord => {}
        RecordRunModel::Slurped => request = request.slurped(),
        RecordRunModel::NullFirst => request = request.with_null_input(),
    }
    if edit {
        request = request.editing();
    }
    if let Some(files) = files {
        request = request.with_files(files);
    }
    let outcome = jqf_sdk::execute(request, sink).map_err(RecordDriveError::Pipeline)?;
    // A record request never declines and the record drive always serves a record report; anything else is an internal
    // contract violation.
    match outcome {
        Outcome::Served(Report::Record(report)) => Ok(report),
        _ => Err(RecordDriveError::Setup {
            step: "record-report",
            error: jqf_codec_core::CodecError::new(jqf_codec_core::CodecFailureKind::InternalContractViolation {
                contract: "record drive served a non-record outcome",
            }),
        }),
    }
}

/// The record route's worker-side drive.
///
/// Every payload operation — grammar, validation, document construction, publication — is delegated to
/// `drive_record_range`, the ONE record drive in the process. A worker adds no second decode ladder.
impl<'request> MorselDrive<'request> for RecordDriveSpec<'request> {
    fn input(self) -> &'request [u8] {
        self.input
    }

    fn cooperative_credits(self) -> u32 {
        self.cooperative_credits
    }

    fn drive_morsel(
        self,
        bytes: &[u8],
        program: &CompiledProgram,
        resources: &mut ResourceContext<'_>,
        sink: &mut MorselSink<'_>,
    ) -> Result<MorselUnits, MorselFallbackCause> {
        let report = drive_record_range(self, bytes, program, resources, sink, None)
            .map_err(|_| MorselFallbackCause::DriveFailed)?;
        // A record ISSUE is an ordered diagnostic carrying an absolute offset, which a worker seeing one byte range
        // cannot render; it makes the morsel unclean exactly as a reported value error does.
        if report.issues() != 0 || report.error_issues() != 0 {
            return Err(MorselFallbackCause::Diagnostics);
        }
        Ok(MorselUnits {
            units: report.records(),
            items: report.items(),
        })
    }
}
