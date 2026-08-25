//! The pushed-down demand routes, driven directly through the HTML provider.
//!
//! The pushed-down demand routes, driven directly through the HTML provider.
//!
//! One drive per advertised demand slot — located — opened with the requirement
//! the ENGINE lowers for that path (`jqf_engine`'s own lowerings, so the
//! harness cannot drift from what the CLI asks for).

use jqf_codec_core::{
    AccessOutcome, CodecRunContext, DecodeRequest, DiagnosticPolicy, ErasedProvider, ExactSelectionRecord,
    ValidationMode,
};
use jqf_data::DialectId;
use jqf_engine::{CodecRequirementPolicy, StaticForwardStep, try_lower_forward_requirement};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use super::Answer;
use super::corpus::Step;
use super::hash;

pub(crate) const CREDIT: u32 = 4_096;

static CONTROL: ContinueControl = ContinueControl;

pub(crate) fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 256 << 20, u64::MAX, 256)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(CREDIT).expect("work meter"),
    )
    .expect("resources")
}

pub(crate) fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "differential.html",
        bytes,
        0,
    )
}

pub(crate) fn request<'a>() -> DecodeRequest<'a> {
    let dialect: &'static DialectId = Box::leak(Box::new(
        DialectId::try_new(jqf_codec_html::HTML_DOCUMENT_DIALECT_ID).expect("dialect"),
    ));
    DecodeRequest {
        validation: ValidationMode::Strict,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        dialect,
        options: None,
        allow_adjacent_values: false,
        value_separator: &[],
    }
}

pub(crate) fn provider<'source>(
    bytes: &'source [u8],
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedProvider<'source>, String> {
    jqf_codec_html::registration()
        .map_err(|error| format!("registration: {error:?}"))?
        .decoder()
        .ok_or_else(|| "the HTML registration carries no decoder".to_owned())?
        .create_provider(source(bytes), request(), resources)
        .map_err(|error| format!("provider: {:?}", error.kind()))
}

fn policy() -> CodecRequirementPolicy {
    CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly)
}

fn forward(steps: &[Step]) -> Vec<StaticForwardStep<'_>> {
    steps
        .iter()
        .map(|step| match step {
            Step::Member(key) => StaticForwardStep::ObjectKey(key),
            Step::Index(index) => StaticForwardStep::ArrayIndex(*index),
            Step::Range(start, end) => StaticForwardStep::ArrayRange {
                start: *start,
                end: *end,
            },
        })
        .collect()
}

/// The located route: the exact path's value, or its negative observation.
pub(crate) fn located(bytes: &[u8], steps: &[Step]) -> Answer {
    let mut resources = resources();
    let outcome = (|| -> Result<Answer, String> {
        let mut provider = provider(bytes, &mut resources)?;
        let requirement = try_lower_forward_requirement(policy(), &forward(steps), &resources)
            .map_err(|error| format!("located requirement: {:?}", error.kind()))?;
        let handle = provider
            .bind(&requirement)
            .map_err(|error| format!("located bind: {error:?}"))?;
        let mut session = provider
            .open(&handle, &mut resources)
            .map_err(|error| format!("located open: {:?}", error.kind()))?;
        {
            let mut run = CodecRunContext::new(&mut resources);
            run.set_cooperative_credits(CREDIT);
            let result = session
                .decode(&mut run)
                .map_err(|error| format!("located decode: {:?}", error.kind()))?;
            let AccessOutcome::Located(located) = result.outcome() else {
                return Err("the located route published a non-located outcome".to_owned());
            };
            let document = located.product().document();
            let names = hash::document_names(document, &mut resources);
            Ok(selection_answer(document, &names, located.result()))
        }
    })();
    match outcome {
        Ok(answer) => answer,
        // The deliberate decline (range/plural-member hits are a stream, not
        // one Located document) is agreement by fallback, never a harness
        // failure — see `Answer::Declined`.
        Err(error) if error.contains("RequirementMismatch") => Answer::Declined,
        Err(error) => Answer::Failed(error),
    }
}

fn selection_answer(
    document: &jqf_data::Document<'_>,
    names: &[(jqf_data::NodeId, String)],
    record: &ExactSelectionRecord,
) -> Answer {
    match record {
        ExactSelectionRecord::Node { node, .. } => Answer::Value(hash::node(document, names, *node)),
        ExactSelectionRecord::Missing { step_index, .. } => Answer::Missing(*step_index),
        ExactSelectionRecord::TypeMismatch {
            step_index,
            actual_type,
            ..
        } => Answer::Mismatch(*step_index, *actual_type),
    }
}
