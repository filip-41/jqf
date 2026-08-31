//! Shared smoke drive: sinks, resource contexts, and the one pipeline request builder.
//!
//! Every battery module borrows these types. `run` formats a pipeline failure
//! as `String`; `execute_root` returns the typed [`jqf_sdk::Failure`] the
//! adversarial-sink receipts match on. Oracle helpers (`oracle_run_over`,
//! [`OracleOutcome`]) are the designated-vs-floor driver the corpus and
//! equivalence batteries share. Sibling batteries live next to this file.

use jqf_codec_core::{
    AccessAdapter, AccessResultKind, DecodeRequest, DiagnosticPolicy, PreservationRequest, ValidationMode,
};
use jqf_data::{DialectId, FormatId, Value};
use jqf_engine::{CodecRequirementPolicy, CompileOptions, CompiledProgram, try_compile_program};
use jqf_resource::{
    ContinueControl, Control, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits, WorkMeter,
};
use jqf_sdk::{
    CodecCatalog, EncodedItemReport, FacadeFraming, ItemSink, OrderedResultPoll, OrderedResultProducer, PipelinePolicy,
};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use std::sync::OnceLock;

/// The built-in JSON dialect the smoke battery's requests borrow.
pub(crate) fn json_dialect() -> &'static DialectId {
    static DIALECT: OnceLock<DialectId> = OnceLock::new();
    DIALECT.get_or_init(|| DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"))
}

pub(crate) static CONTROL: ContinueControl = ContinueControl;

pub(crate) struct ToggleControl(pub(crate) core::sync::atomic::AtomicBool);

impl Control for ToggleControl {
    fn check(&self) -> ControlOutcome {
        if self.0.load(core::sync::atomic::Ordering::Relaxed) {
            ControlOutcome::Cancelled
        } else {
            ControlOutcome::Continue
        }
    }
}

pub(crate) struct PartialSink {
    pub(crate) bytes: Vec<u8>,
    pub(crate) boundaries: Vec<(bool, u64)>,
    pub(crate) reports: Vec<EncodedItemReport>,
}

pub(crate) struct FailingSink {
    pub(crate) bytes: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(crate) enum FaultMode<'a> {
    Zero,
    Oversized,
    Begin,
    Finish,
    CancelAfterWrite(&'a ToggleControl),
    CancelAfterFraming(&'a ToggleControl, usize),
}

pub(crate) struct FaultSink<'a> {
    pub(crate) mode: FaultMode<'a>,
    pub(crate) bytes: Vec<u8>,
}

impl ItemSink for FaultSink<'_> {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        if matches!(self.mode, FaultMode::Begin) {
            Err("begin failure")
        } else {
            Ok(())
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        match self.mode {
            FaultMode::Zero => Ok(0),
            FaultMode::Oversized => Ok(bytes.len() + 1),
            FaultMode::CancelAfterWrite(control) => {
                let Some(first) = bytes.first() else {
                    return Ok(0);
                };
                self.bytes.push(*first);
                control.0.store(true, core::sync::atomic::Ordering::Relaxed);
                Ok(1)
            }
            FaultMode::CancelAfterFraming(control, codec_bytes) => {
                self.bytes.extend_from_slice(bytes);
                if self.bytes.len() > codec_bytes {
                    control.0.store(true, core::sync::atomic::Ordering::Relaxed);
                }
                Ok(bytes.len())
            }
            FaultMode::Begin | FaultMode::Finish => {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
        }
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        if matches!(self.mode, FaultMode::Finish) {
            Err("finish failure")
        } else {
            Ok(())
        }
    }
}

pub(crate) struct ManyProducer {
    pub(crate) items: std::vec::IntoIter<Value>,
    pub(crate) pending: bool,
}

impl OrderedResultProducer<'static> for ManyProducer {
    fn poll_next(
        &mut self,
        _context: &mut jqf_codec_core::CodecRunContext<'_, '_>,
    ) -> Result<OrderedResultPoll<'static>, jqf_codec_core::CodecError> {
        if self.pending {
            self.pending = false;
            return Ok(OrderedResultPoll::Pending);
        }
        Ok(self
            .items
            .next()
            .map(jqf_engine::EngineResult::owned)
            .map_or(OrderedResultPoll::Complete, OrderedResultPoll::Item))
    }
}

impl ItemSink for FailingSink {
    type Error = &'static str;

    fn begin_item(&mut self, _index: u64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        if self.bytes.len() == 4 {
            return Err("injected sink failure");
        }
        let accepted = bytes.len().min(4 - self.bytes.len());
        self.bytes.extend_from_slice(&bytes[..accepted]);
        Ok(accepted)
    }

    fn finish_item(&mut self, _index: u64, _report: EncodedItemReport) -> Result<(), Self::Error> {
        Err("finish after injected failure")
    }
}

impl ItemSink for PartialSink {
    type Error = &'static str;

    fn begin_item(&mut self, index: u64) -> Result<(), Self::Error> {
        self.boundaries.push((true, index));
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<usize, Self::Error> {
        let accepted = bytes.len().min(3);
        self.bytes.extend_from_slice(&bytes[..accepted]);
        Ok(accepted)
    }

    fn finish_item(&mut self, index: u64, report: EncodedItemReport) -> Result<(), Self::Error> {
        self.boundaries.push((false, index));
        self.reports.push(report);
        Ok(())
    }
}

pub(crate) fn resources() -> ResourceContext<'static> {
    resources_with(&CONTROL, u64::MAX, 7)
}

pub(crate) fn resources_with(control: &dyn Control, max_output_bytes: u64, credits: u32) -> ResourceContext<'_> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, max_output_bytes, 64 << 20, 0, 128)).expect("account"),
        control,
        WorkMeter::try_new_v1(credits).expect("work meter"),
    )
    .expect("resources")
}

/// An exact-path `Located` requirement whose demand fits the scoped route's
/// ceiling Direct-binds that route (scoped physical identity, slot 1,
/// `adapter = None`) instead of falling back to the whole route + the generic
/// `CompleteDocumentExact` adapter.
pub(crate) fn is_scoped_exact_report(report: jqf_sdk::PipelineReport) -> bool {
    report.access_route().route() == jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID
        && report.access_route().slot().get() == 1
        && report.access_report().adapter() == AccessAdapter::None
}

/// Which route the projection-vs-floor oracle drives one pair through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OracleRoute {
    /// Whatever route the compiled filter takes TODAY: its own
    /// located / whole-document requirement.
    Designated,
    /// The floor: the same filter behind a `[.][0] |` construction barrier. The
    /// barrier drives the pushdown split to whole-document (a constructor on the
    /// upstream spine pushes nothing down) and materializes the input into an
    /// owned value before the filter navigates it, so NOTHING is projected.
    Floor,
}

/// What one oracle run observed: the published bytes, whether the run completed,
/// and the route receipt proving which physical route actually fired.
pub(crate) struct OracleOutcome {
    pub(crate) bytes: Vec<u8>,
    pub(crate) completed: bool,
    pub(crate) result: AccessResultKind,
    /// Whether the RANGE-LOCATE rung served this run.
    ///
    /// [`Self::result`] cannot say so on its own: the bare-slice publish and the
    /// ordinary located route report the same [`AccessResultKind::Located`], so a
    /// rung that declined (container dispatch) is indistinguishable
    /// from one that fired. The force-route lane's `forced` counter exists
    /// precisely to prove the comparison is not floor ≡ floor in disguise, so it
    /// reads this flag rather than guessing from the result kind.
    pub(crate) range_located: bool,
    /// The failure CLASS a failed run ended in (`None` on success). The
    /// force-route differential compares bytes and completion on every row,
    /// but a projected route that raises the WRONG class at zero bytes (e.g.
    /// an `InternalContractViolation` where the floor raised a type error)
    /// publishes zero bytes on both sides and would pass a byte-only compare.
    /// The class is the payload-free soundness net for exactly that case.
    pub(crate) failure_class: Option<String>,
}

/// The payload-free failure CLASS of an oracle run, for the differential's
/// error arms: the codec KIND for a codec failure, else the semantic
/// variant's class name. Payloads are deliberately excluded — the class is
/// what must agree between the designated route and the forced floor.
pub(crate) fn failure_class<SinkError: std::fmt::Debug>(failure: &jqf_sdk::PipelineFailure<SinkError>) -> String {
    let class: std::borrow::Cow<'_, str> = match failure {
        jqf_sdk::PipelineFailure::Codec(error) => {
            // The payload-free VARIANT name, never the full Debug: a
            // `Resource`/`InternalContractViolation` payload (ceiling
            // numbers, current bytes, contract text) legitimately differs
            // between the designated route and the floor — the projected
            // route allocates less — and must not make the net false-flag a
            // same-class raise. This is the payload-free net the oracle
            // doc promises.
            std::borrow::Cow::Owned(format!("codec:{}", codec_kind_class(&error.kind())))
        }
        jqf_sdk::PipelineFailure::TypeMismatch { .. } => std::borrow::Cow::Borrowed("type-mismatch"),
        jqf_sdk::PipelineFailure::IterateMismatch { .. } => std::borrow::Cow::Borrowed("iterate-mismatch"),
        jqf_sdk::PipelineFailure::ObjectKeyMismatch { .. } => std::borrow::Cow::Borrowed("object-key-mismatch"),
        jqf_sdk::PipelineFailure::NoLength { .. } => std::borrow::Cow::Borrowed("no-length"),
        jqf_sdk::PipelineFailure::NoKeys { .. } => std::borrow::Cow::Borrowed("no-keys"),
        jqf_sdk::PipelineFailure::ArithmeticError(_) => std::borrow::Cow::Borrowed("arithmetic"),
        jqf_sdk::PipelineFailure::SliceIndices => std::borrow::Cow::Borrowed("slice-indices"),
        jqf_sdk::PipelineFailure::MismatchRaised { .. } => std::borrow::Cow::Borrowed("mismatch-raised"),
        jqf_sdk::PipelineFailure::EngineCardinality { .. } => std::borrow::Cow::Borrowed("engine-cardinality"),
        jqf_sdk::PipelineFailure::Raised(_) => std::borrow::Cow::Borrowed("raised"),
        jqf_sdk::PipelineFailure::Registry(_) => std::borrow::Cow::Borrowed("registry"),
        jqf_sdk::PipelineFailure::AccessBind(_) => std::borrow::Cow::Borrowed("access-bind"),
        jqf_sdk::PipelineFailure::Sink(_) => std::borrow::Cow::Borrowed("sink"),
        jqf_sdk::PipelineFailure::SinkContract => std::borrow::Cow::Borrowed("sink-contract"),
        jqf_sdk::PipelineFailure::InvalidCooperativeCredits => {
            std::borrow::Cow::Borrowed("invalid-cooperative-credits")
        }
        jqf_sdk::PipelineFailure::Halt { status, .. } => std::borrow::Cow::Owned(format!("halt:{status}")),
        jqf_sdk::PipelineFailure::EditOutputCount { observed } => {
            std::borrow::Cow::Owned(format!("edit-output-count:{observed}"))
        }
        jqf_sdk::PipelineFailure::SplitName { .. } => std::borrow::Cow::Borrowed("split-name"),
        jqf_sdk::PipelineFailure::SplitCollision { .. } => std::borrow::Cow::Borrowed("split-collision"),
    };
    class.into_owned()
}

/// The payload-free class name of a codec failure kind: the variant, never
/// the payload. The net compares CLASSES across the designated route and the
/// floor; a `Resource` ceiling number or an `InternalContractViolation`
/// contract text would legitimately differ between them (the projected route
/// allocates less), so the class must not carry it. Exhaustive on purpose: a
/// new `CodecFailureKind` variant fails to compile here until it has a class.
fn codec_kind_class(kind: &jqf_codec_core::CodecFailureKind) -> &'static str {
    match kind {
        jqf_codec_core::CodecFailureKind::InvalidInput => "invalid-input",
        jqf_codec_core::CodecFailureKind::UnsupportedRepresentation => "unsupported-representation",
        jqf_codec_core::CodecFailureKind::RequirementMismatch => "requirement-mismatch",
        jqf_codec_core::CodecFailureKind::ProviderRouteMismatch => "provider-route-mismatch",
        jqf_codec_core::CodecFailureKind::InvalidTag => "invalid-tag",
        jqf_codec_core::CodecFailureKind::CollidingTags => "colliding-tags",
        jqf_codec_core::CodecFailureKind::Resource(_) => "resource",
        jqf_codec_core::CodecFailureKind::Control(_) => "control",
        jqf_codec_core::CodecFailureKind::Overflow => "overflow",
        jqf_codec_core::CodecFailureKind::AllocationFailure => "allocation-failure",
        jqf_codec_core::CodecFailureKind::InternalContractViolation { .. } => "internal-contract-violation",
        jqf_codec_core::CodecFailureKind::RawNulByte => "raw-nul-byte",
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one route selector: the range-locate and ordinary arms are read as one table"
)]
pub(crate) fn oracle_run_over(
    route: OracleRoute,
    catalog: CodecCatalog<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    program_source: &str,
    input: &[u8],
) -> Result<OracleOutcome, String> {
    let source = match route {
        OracleRoute::Designated => program_source.to_owned(),
        // The floor-forcing wrapper the corpus's `floorparity` rows use, kept
        // identical here so the CLI corpus and this harness force the SAME floor.
        OracleRoute::Floor => format!("[.][0] | ({program_source})"),
    };
    let mut oracle_resources = resources();
    let program = program_for(&source, &oracle_resources)?;
    let mut sink = PartialSink {
        bytes: Vec::new(),
        boundaries: Vec::new(),
        reports: Vec::new(),
    };

    // The BARE-SLICE publish rung, read exactly as the CLI's selector reads it:
    // last before the ordinary route, and declining into it without publishing.
    if route == OracleRoute::Designated && program.range_locate_eligible() {
        let requirement = program
            .try_range_locate_requirement(&oracle_resources)
            .map_err(|error| format!("oracle range-locate requirement: {:?}", error.kind()))?;
        let source = probe_source(input);
        let request = jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(input))
            .with_catalog(catalog)
            .with_source(source)
            .with_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_output_format(
                FormatId::try_new(format.as_str()).expect("format id"),
                DialectId::try_new(dialect.as_str()).expect("dialect id"),
            )
            .with_policy(single_document_policy())
            .with_framing(FacadeFraming::item_suffix(b"\n"))
            .with_resources(&mut oracle_resources)
            .with_requirement(&requirement)
            .range_locate();
        let run = jqf_sdk::execute(request, &mut sink);
        match run {
            Ok(jqf_sdk::Outcome::Served(_)) => {
                return Ok(OracleOutcome {
                    bytes: sink.bytes,
                    completed: true,
                    result: requirement.result(),
                    range_located: true,
                    failure_class: None,
                });
            }
            // container dispatch and the adjacency law are the
            // same answer here: nothing published, the document handed to the
            // ordinary route below.
            Ok(jqf_sdk::Outcome::Declined) => {
                sink.bytes.clear();
                sink.boundaries.clear();
                sink.reports.clear();
            }
            Err(error) => {
                return Ok(OracleOutcome {
                    bytes: sink.bytes,
                    completed: false,
                    result: requirement.result(),
                    range_located: true,
                    failure_class: Some(failure_class(error.pipeline_failure().expect("pipeline failure"))),
                });
            }
        }
    }

    let requirement = program
        .try_requirement(&oracle_resources)
        .map_err(|error| format!("oracle requirement: {:?}", error.kind()))?;
    let source = probe_source(input);
    let request = jqf_sdk::Request::new(&program, jqf_sdk::Input::Whole(input))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_output_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_policy(single_document_policy())
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(&mut oracle_resources)
        .with_requirement(&requirement);
    let outcome = jqf_sdk::execute(request, &mut sink);
    let (completed, failure_class) = match outcome {
        Ok(jqf_sdk::Outcome::Served(_)) => (true, None),
        Ok(jqf_sdk::Outcome::Declined) => (false, None),
        Err(error) => (
            false,
            Some(failure_class(error.pipeline_failure().expect("pipeline failure"))),
        ),
    };
    Ok(OracleOutcome {
        bytes: sink.bytes,
        completed,
        result: requirement.result(),
        range_located: false,
        failure_class,
    })
}

/// The single-document decode policy for the fast-lane receipts: strict,
/// no adjacent-value tolerance (one document consuming the whole input).
pub(crate) fn single_document_policy() -> PipelinePolicy<'static> {
    PipelinePolicy {
        decode: DecodeRequest {
            validation: ValidationMode::Strict,
            diagnostics: DiagnosticPolicy::ErrorsOnly,
            dialect: json_dialect(),
            options: None,
            allow_adjacent_values: false,
            value_separator: jqf_codec_json::VALUE_SEPARATORS,
        },
        encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::None,
        encode_options: None,
        cooperative_credits: 7,
        split: None,

        max_iterations: None,
    }
}

pub(crate) fn probe_source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(14), SourceKind::Input),
        "probe.json",
        bytes,
        0,
    )
}

pub(crate) fn program_for(source: &str, resources: &ResourceContext<'_>) -> Result<CompiledProgram, String> {
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    try_compile_program(source, policy, CompileOptions::new(), resources)
        .map_err(|error| format!("program {source:?}: {error}"))
}

#[allow(
    clippy::too_many_arguments,
    reason = "smoke keeps all public pipeline boundary inputs visible"
)]
fn drive<Sink>(
    catalog: CodecCatalog<'_, '_>,
    bytes: &[u8],
    requirement: &jqf_codec_core::AccessRequirement,
    program: &CompiledProgram,
    format: &FormatId,
    dialect: &DialectId,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
    source_id: u32,
    source_name: &'static str,
) -> Result<jqf_sdk::PipelineReport, jqf_sdk::Failure>
where
    Sink: ItemSink,
    Sink::Error: std::fmt::Display,
{
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(source_id), SourceKind::Input),
        source_name,
        bytes,
        0,
    );
    let request = jqf_sdk::Request::new(program, jqf_sdk::Input::Whole(bytes))
        .with_catalog(catalog)
        .with_source(source)
        .with_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_output_format(
            FormatId::try_new(format.as_str()).expect("format id"),
            DialectId::try_new(dialect.as_str()).expect("dialect id"),
        )
        .with_policy(PipelinePolicy {
            decode: DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: json_dialect(),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            encode_diagnostics: DiagnosticPolicy::ErrorsOnly,
            preservation: PreservationRequest::Report,
            encode_options: None,
            cooperative_credits: 7,
            split: None,
            max_iterations: None,
        })
        .with_framing(FacadeFraming::item_suffix(b"\n"))
        .with_resources(resources)
        .with_requirement(requirement);
    match jqf_sdk::execute(request, sink) {
        Ok(jqf_sdk::Outcome::Served(jqf_sdk::Report::Pipeline(report))) => Ok(report),
        Ok(jqf_sdk::Outcome::Served(other)) => panic!("unexpected report: {other:?}"),
        Ok(jqf_sdk::Outcome::Declined) => panic!("the single-document drive must not decline"),
        Err(error) => Err(error),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "smoke keeps all public pipeline boundary inputs visible"
)]
pub(crate) fn execute_root<Sink>(
    catalog: CodecCatalog<'_, '_>,
    bytes: &[u8],
    requirement: &jqf_codec_core::AccessRequirement,
    program: &CompiledProgram,
    format: &FormatId,
    dialect: &DialectId,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<jqf_sdk::PipelineReport, jqf_sdk::Failure>
where
    Sink: ItemSink,
    Sink::Error: std::fmt::Display,
{
    drive(
        catalog,
        bytes,
        requirement,
        program,
        format,
        dialect,
        resources,
        sink,
        12,
        "fault.json",
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "smoke keeps all public boundary inputs visible"
)]
pub(crate) fn run<Sink>(
    catalog: CodecCatalog<'_, '_>,
    bytes: &[u8],
    requirement: &jqf_codec_core::AccessRequirement,
    program: &CompiledProgram,
    format: &FormatId,
    dialect: &DialectId,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<jqf_sdk::PipelineReport, String>
where
    Sink: ItemSink,
    Sink::Error: std::fmt::Display,
{
    drive(
        catalog,
        bytes,
        requirement,
        program,
        format,
        dialect,
        resources,
        sink,
        11,
        "smoke.json",
    )
    .map_err(|error| format!("{error:?}"))
}
