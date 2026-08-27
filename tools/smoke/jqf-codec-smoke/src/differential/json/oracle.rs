//! Drives jqf and `serde_json` decode over identical bytes and classifies each
//! into an accept-with-checksum or reject verdict for differential comparison.

use jqf_codec_core::{CodecRunContext, DecodeRequest, DiagnosticPolicy, ValidationMode};
use jqf_data::DialectId;
use jqf_engine::{
    CodecInputOutcome, CodecInputResult, CodecRequirementPolicy, EngineResult, try_lower_root_requirement,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::differential::{Verdict, format_poll_error};

use super::semantic;

/// Cooperative work granted per resumed poll; the protocol maximum
/// (the maximum credits one cooperative entry admits), so every
/// corpus input in this tool finishes in as few cooperative entries as the
/// protocol allows.
const CREDIT: u32 = 4_096;
/// Memory ceiling for one decode attempt; generous relative to every fixture.
const MEMORY_BYTES: u64 = 128 << 20;

static CONTROL: ContinueControl = ContinueControl;

/// Decodes `bytes` as a complete JSON document through jqf's strict-JSON
/// route and returns its accept/reject verdict.
pub(crate) fn decode_jqf(bytes: &[u8], max_depth: u32) -> Verdict {
    match try_decode_jqf(bytes, max_depth) {
        Ok(value) => Verdict::Accept(semantic::jqf_value(&value)),
        Err(reason) => Verdict::Reject(reason),
    }
}

/// Decodes `bytes` as a complete JSON document through `serde_json` and
/// returns its accept/reject verdict.
pub(crate) fn decode_serde(bytes: &[u8]) -> Verdict {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        Ok(value) => Verdict::Accept(semantic::serde_value(&value)),
        Err(error) => Verdict::Reject(error.to_string()),
    }
}

fn try_decode_jqf(bytes: &[u8], max_depth: u32) -> Result<jqf_data::Value, String> {
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(
            u64::MAX,
            u64::MAX,
            MEMORY_BYTES,
            u64::MAX,
            max_depth,
        ))
        .map_err(|error| format!("account: {error:?}"))?,
        &CONTROL,
        WorkMeter::try_new_v1(CREDIT).ok_or_else(|| "invalid work credit".to_owned())?,
    )
    .map_err(|error| format!("resources: {error:?}"))?;
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "differential",
        bytes,
        0,
    );
    let registration = jqf_codec_json::registration().map_err(|error| format!("registration: {error:?}"))?;
    let mut provider = registration
        .decoder()
        .ok_or_else(|| "strict JSON registration lacks a decoder".to_owned())?
        .create_provider(
            source,
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let demand = try_lower_root_requirement(policy, Some(0), &resources)
        .map_err(|error| format!("requirement: {:?}", error.kind()))?;
    let handle = provider.bind(&demand).map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(CREDIT);
        let result = match session.decode(&mut run) {
            Ok(result) => result,
            Err(error) => return Err(format_poll_error(&error)),
        };
        let (outcome, _report) = CodecInputResult::try_from_access(result)
            .map_err(|error| format!("handoff: {:?}", error.kind()))?
            .into_parts();
        let CodecInputOutcome::Result(EngineResult::Located(located)) = outcome else {
            return Err(format!("unexpected non-document outcome: {outcome:?}"));
        };
        located
            .product()
            .document()
            .materialize_root(&mut resources)
            .map_err(|error| format!("materialize: {error:?}"))
    }
}
