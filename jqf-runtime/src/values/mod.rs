//! The adjacent-value parallel drive: ordered shards over the default stdin.
//!
//! # This is a ROUTING decision, not an inference
//!
//! jqf never decides what its input "is". The never-infer law stands exactly as written: the default input is a stream
//! of RFC 8259 texts separated by arbitrary whitespace, no format is detected, no dialect is selected, and input is
//! never declared NDJSON. What this module adds is the same distinction the record route already draws — SPLITTING a
//! stream at value boundaries is a routing choice, DECLARING a format is inference — now applied to the default path,
//! whose shards are decoded by exactly the adjacent-value ladder the serial drive uses.
//!
//! The ordering law, the clean-morsel law, and the yield-to-serial recovery live in [`crate::parallel`]. What is this
//! route's own is where a shard may START ([`boundary`], whose soundness argument is the module's opening) and what one
//! shard decodes ([`drive_value_range`], which is `jqf_sdk::execute` over a byte range).

mod boundary;
mod plan;

use core::fmt;

use jqf_codec_core::CodecError;
use jqf_codec_json::{JsonEncodeOptions, ndjson::NdjsonTerminator};
use jqf_engine::CompiledProgram;
use jqf_resource::ResourceContext;
use jqf_sdk::{Input, ItemSink, Outcome, Report, SequenceReport};

use boundary::ShardStep;
pub use boundary::ValueMorsels;
pub use plan::plan_request;

pub use crate::parallel::{ParallelPlan, PlanDecision};

use crate::output::{OutputTarget, build_request};
use crate::parallel::{Morsel, MorselDrive, MorselSink, MorselSource, MorselUnits, Relay, SkippingSink, relay_morsels};
use crate::workers::MorselFallbackCause;

/// How a request's output is framed.
///
/// Kept as plain `Copy` fields rather than as built codec options so a worker can rebuild the options inside its own
/// child ledger.
#[derive(Clone, Copy, Debug)]
pub struct ValueOutputSpec {
    /// Which family of bytes this drive publishes (JSON, NDJSON, or json-seq).
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

/// Everything one adjacent-value drive needs, in a form that crosses a thread.
///
/// Every field is `Copy` and thread-safe by construction: borrowed immutable bytes and plain values. No account, no
/// document, no provider, no encoder, and no sink appears here, which is what lets a worker rebuild the whole drive
/// from its own numeric grant.
#[derive(Clone, Copy, Debug)]
pub struct ValueDriveSpec<'request> {
    /// The whole retained input.
    pub input: &'request [u8],
    /// The source name diagnostics render.
    pub source_name: &'request str,
    /// Output framing.
    pub output: ValueOutputSpec,
    /// The process-lifetime catalog of the record formats the drive serves catalog, installed once through
    /// [`crate::records::install_record_catalog`].
    pub catalog: jqf_sdk::CodecCatalog<'static, 'static>,
    /// Credits installed on every cooperative resume.
    pub cooperative_credits: u32,
}

/// An adjacent-value drive's failure, at the boundary that raised it.
#[derive(Debug)]
pub enum ValueDriveError<SinkError> {
    /// Construction before the pipeline ran: requirement lowering. The static name says which step.
    Setup {
        /// The construction step that failed.
        step: &'static str,
        /// The codec's own failure.
        error: CodecError,
    },
    /// The sequence pipeline's own failure, unchanged.
    Pipeline(jqf_sdk::Failure),
    /// The caller's sink refused a relayed shard's bytes.
    Sink(SinkError),
    /// A worker envelope or the parent ledger refused the request.
    Resource(jqf_resource::ResourceError),
    /// The host cancelled the request, the deadline expired, or the physical memory ceiling was exceeded.
    Control(jqf_resource::ControlError),
}

/// Completion summary of one adjacent-value request, serial or parallel.
///
/// The production reader is the [`fmt::Display`] impl, which the CLI prints as the relay line; the fields stay private
/// to it.
#[derive(Clone, Copy, Debug)]
pub struct ValueRequestReport {
    items: u64,
    shards_published: u64,
    fell_back: Option<MorselFallbackCause>,
    degraded: bool,
    cancelled_worker_panics: u32,
}

/// The relay half of a request's diagnostics.
///
/// The PLAN is printable before the drive runs and the relay only afterwards, so they are two lines: a request that
/// fails still gets to say what it planned, which is what lets a differential harness confirm the lane actually engaged
/// on an input it also expects to fail.
impl fmt::Display for ValueRequestReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fallback = match self.fell_back {
            None => "none",
            Some(MorselFallbackCause::Diagnostics) => "diagnostics",
            Some(MorselFallbackCause::DriveFailed) => "drive-failed",
            Some(MorselFallbackCause::WorkerUnavailable) => "worker-unavailable",
        };
        write!(
            formatter,
            "relay: shards_published={} fallback={fallback} items={} degraded={} worker_panics={}",
            self.shards_published, self.items, self.degraded, self.cancelled_worker_panics
        )
    }
}

/// Partitions `plan`'s input at proven top-level boundaries, or demotes the plan to serial when there is nothing safe
/// to cut.
///
/// The partition is separate from [`execute_value_request`] because the CLI prints the plan BEFORE the drive: a plan
/// that says `parallel` when boundary discovery already declined would be a diagnostics line that lies.
///
/// Only the first two cuts are proven here. The relay pulls the rest while workers already run, so the coordinator does
/// not walk the whole input before the first dispatch.
#[must_use]
pub fn partition_request(input: &[u8], plan: ParallelPlan) -> (ParallelPlan, ValueMorsels<'_>) {
    if !plan.is_parallel() {
        return (plan, ValueMorsels::empty());
    }
    let mut morsels = ValueMorsels::new(input, plan.morsel_bytes());
    if morsels.prove_parallel() {
        (plan, morsels)
    } else {
        (plan.demoted(PlanDecision::NoBoundary), ValueMorsels::empty())
    }
}

/// Runs one adjacent-value request, in parallel when the plan says so.
///
/// A serial plan drives exactly the code path the switch-off drives; there is no one-worker coordinator hiding behind
/// it.
///
/// # Errors
///
/// Returns the adjacent-value drive's own failure. The parallel lane raises no error of its own: every coordinator,
/// grant, or worker problem yields to serial instead, and only the serial redrive's failure can surface.
pub fn execute_value_request<Sink: ItemSink>(
    spec: ValueDriveSpec<'_>,
    plan: ParallelPlan,
    morsels: ValueMorsels<'_>,
    program: &CompiledProgram,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<ValueRequestReport, ValueDriveError<Sink::Error>>
where
    Sink::Error: core::fmt::Display,
{
    if !plan.is_parallel() || spec.output.target.is_stateful() {
        let sequence = drive_value_range(spec, spec.input, program, resources, sink)?;
        return Ok(ValueRequestReport {
            items: sequence.items(),
            shards_published: 0,
            fell_back: None,
            degraded: false,
            cancelled_worker_panics: 0,
        });
    }
    match relay_morsels(spec, plan, morsels, program, resources, sink) {
        Relay::Complete {
            units: _,
            items,
            morsels_published,
            degraded,
            cancelled_worker_panics,
        } => {
            // No input charge here, for the record route's reason: the bytes were read once by the host, and this lane
            // only borrows them.
            Ok(ValueRequestReport {
                items,
                shards_published: morsels_published,
                fell_back: None,
                degraded,
                cancelled_worker_panics,
            })
        }
        Relay::SinkFailed(error) => Err(ValueDriveError::Sink(error)),
        Relay::Refused(error) => Err(ValueDriveError::Resource(error)),
        Relay::Control(error) => Err(ValueDriveError::Control(error)),
        Relay::YieldToSerial {
            published_bytes,
            morsels_published,
            cause,
            cancelled_worker_panics,
        } => {
            // Serial's own drive, over the whole input, minus the bytes the clean prefix already wrote. What follows is
            // serial verbatim.
            let mut skipping = SkippingSink::new(sink, published_bytes);
            let sequence = drive_value_range(spec, spec.input, program, resources, &mut skipping)?;
            Ok(ValueRequestReport {
                items: sequence.items(),
                shards_published: morsels_published,
                fell_back: Some(cause),
                degraded: true,
                cancelled_worker_panics,
            })
        }
    }
}

/// Runs the serial adjacent-value drive over `bytes`, publishing through `sink`.
///
/// This is the ONE adjacent-value drive in the process: the serial path calls it over the whole input, a worker calls
/// it over its shard, and the fallback calls it again over the whole input. There is no second decode ladder.
///
/// # Errors
///
/// Returns the construction or pipeline failure the sequence drive raised.
pub(crate) fn drive_value_range<Sink: ItemSink>(
    spec: ValueDriveSpec<'_>,
    bytes: &[u8],
    program: &CompiledProgram,
    resources: &mut ResourceContext<'_>,
    sink: &mut Sink,
) -> Result<SequenceReport, ValueDriveError<Sink::Error>>
where
    Sink::Error: core::fmt::Display,
{
    let requirement = program
        .try_requirement(resources)
        .map_err(|error| ValueDriveError::Setup {
            step: "requirement",
            error,
        })?;
    build_request(
        spec.output.target,
        spec.output.terminator,
        spec.output.json,
        spec.output.no_newline,
        // The value lane's CSV output keeps RFC 4180's comma: the --csv-delimiter dial is CSV-INPUT-only , so no
        // value-lane request can carry one.
        None,
        spec.catalog,
        spec.source_name,
        bytes,
        // The default stdin IS a stream of adjacent texts. This is the one opt-in that distinguishes this drive from a
        // single-document one, and it names no format.
        true,
        spec.cooperative_credits,
        // The split destination is CLI-only; the value lane's parallel shards are unreachable with split engaged (the
        // plan declines to serial), so no shard carries a split program.
        None,
        // The value lane carries no iteration dial of its own: a serial flagless request is served by the CLI's serial
        // tail, whose policy carries `--max-iterations`.
        None,
        |request| {
            let req = jqf_sdk::Request::new(program, Input::Whole(bytes))
                .with_catalog(request.catalog)
                .with_source(request.source)
                .with_format(request.format, request.dialect)
                .with_output_format(request.output_format, request.output_dialect)
                .with_policy(request.policy)
                .with_framing(request.framing)
                .with_resources(resources)
                .with_requirement(&requirement);
            let outcome = jqf_sdk::execute(req, sink).map_err(ValueDriveError::Pipeline)?;
            // A plain request never declines and the sequence drive always serves a sequence report; anything else is
            // an internal contract violation, never a caller mistake.
            match outcome {
                Outcome::Served(Report::Sequence(report)) => Ok(report),
                _ => Err(ValueDriveError::Setup {
                    step: "sequence-report",
                    error: jqf_codec_core::CodecError::new(
                        jqf_codec_core::CodecFailureKind::InternalContractViolation {
                            contract: "value drive served a non-sequence outcome",
                        },
                    ),
                }),
            }
        },
    )
}

impl MorselSource for ValueMorsels<'_> {
    fn next_morsel(&mut self) -> Result<Option<Morsel>, MorselFallbackCause> {
        match self.pull() {
            ShardStep::Shard(morsel) => Ok(Some(morsel)),
            ShardStep::Exhausted | ShardStep::Decline => Ok(None),
            ShardStep::Malformed => Err(MorselFallbackCause::DriveFailed),
        }
    }
}

/// The adjacent-value route's worker-side drive.
impl<'request> MorselDrive<'request> for ValueDriveSpec<'request> {
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
        let report =
            drive_value_range(self, bytes, program, resources, sink).map_err(|_| MorselFallbackCause::DriveFailed)?;
        Ok(MorselUnits {
            units: 0,
            items: report.items(),
        })
    }
}
