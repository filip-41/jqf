//! Scoped-decode differential over the fixed corpus × a curated exact-path set.
//!
//! For every accepted corpus document and every path, the scoped JSON route must
//! produce the identical observation the whole-decode-then-navigate reference
//! produces. The reference reimplements the generic exact interpreter over the
//! public `Document` view API, so this is an independent oracle for the scoped
//! route. Paths deliberately cover hits, missing keys, negative indices, type
//! mismatches, duplicate-key objects and unicode keys.

use jqf_codec_core::{
    AccessOutcome, CodecRunContext, DecodeRequest, DiagnosticPolicy, ExactSelectionRecord, ValidationMode,
};
use jqf_data::DialectId;
use jqf_engine::{
    CodecRequirementPolicy, StaticForwardStep, try_lower_forward_requirement, try_lower_root_requirement,
};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use super::corpus::Case;
use super::semantic;

const CREDIT: u32 = 4_096;
const MEMORY_BYTES: u64 = 128 << 20;

static CONTROL: ContinueControl = ContinueControl;

#[derive(Clone, Copy, Debug)]
enum Step {
    Member(&'static str),
    Index(i64),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Observation {
    Node(u64),
    Missing(usize),
    TypeMismatch(usize, jqf_data::ValueKind),
    Reject,
}

/// Curated exact paths exercising every observation class.
const PATHS: &[&[Step]] = &[
    &[],
    &[Step::Member("a")],
    &[Step::Member("a"), Step::Member("b")],
    &[Step::Member("catalog")],
    &[Step::Member("catalog"), Step::Index(0)],
    &[Step::Member("catalog"), Step::Index(-1)],
    &[Step::Member("missing-key")],
    &[Step::Member("a"), Step::Member("missing")],
    &[Step::Index(0)],
    &[Step::Index(-1)],
    &[Step::Index(2)],
    &[Step::Member("items"), Step::Index(-1)],
    &[Step::Member("é")],
    &[Step::Member("𝄞")],
    &[Step::Member("key")],
    &[Step::Member("a"), Step::Index(0), Step::Member("b")],
];

/// Runs the scoped differential over `cases`, returning `(comparisons, divergences)`.
pub(crate) fn run(cases: &[Case]) -> (usize, Vec<String>) {
    let mut comparisons = 0;
    let mut divergences = Vec::new();
    for case in cases {
        for path in PATHS {
            comparisons += 1;
            let scoped = scoped_observation(&case.bytes, path, case.depth_limit);
            let reference = reference_observation(&case.bytes, path, case.depth_limit);
            match (scoped, reference) {
                (Ok(scoped), Ok(reference)) if scoped == reference => {}
                (Ok(scoped), Ok(reference)) => divergences.push(format!(
                    "{}: path={path:?} scoped={scoped:?} reference={reference:?}",
                    case.name
                )),
                (scoped, reference) => divergences.push(format!(
                    "{}: path={path:?} harness scoped={scoped:?} reference={reference:?}",
                    case.name
                )),
            }
        }
    }
    (comparisons, divergences)
}

fn resources(max_depth: u32) -> Result<ResourceContext<'static>, String> {
    ResourceContext::new(
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
    .map_err(|error| format!("resources: {error:?}"))
}

fn source(bytes: &[u8]) -> ResolvedSource<'_> {
    ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "scoped-differential",
        bytes,
        0,
    )
}

fn forward_steps(path: &[Step]) -> Vec<StaticForwardStep<'_>> {
    path.iter()
        .map(|step| match step {
            Step::Member(key) => StaticForwardStep::ObjectKey(key),
            Step::Index(index) => StaticForwardStep::ArrayIndex(*index),
        })
        .collect()
}

fn scoped_observation(bytes: &[u8], path: &[Step], depth: u32) -> Result<Observation, String> {
    let mut resources = resources(depth)?;
    let registration = jqf_codec_json::registration().map_err(|error| format!("registration: {error:?}"))?;
    let mut provider = registration
        .decoder()
        .ok_or_else(|| "no decoder".to_owned())?
        .create_provider(source(bytes), request(), &mut resources)
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let steps = forward_steps(path);
    let requirement = try_lower_forward_requirement(policy, &steps, &resources)
        .map_err(|error| format!("requirement: {:?}", error.kind()))?;
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    poll_located(&mut provider, &mut session, &mut resources, bytes.len())
}

fn poll_located(
    _provider: &mut jqf_codec_core::ErasedProvider<'_>,
    session: &mut jqf_codec_core::ErasedAccessSession<'_>,
    resources: &mut ResourceContext<'_>,
    _len: usize,
) -> Result<Observation, String> {
    {
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(CREDIT);
        let Ok(result) = session.decode(&mut run) else {
            return Ok(Observation::Reject);
        };
        let AccessOutcome::Located(located) = result.into_parts().0 else {
            return Err("scoped route returned a non-located outcome".to_owned());
        };
        Ok(match located.result() {
            ExactSelectionRecord::Node { node, .. } => Observation::Node(semantic::jqf_value(
                &located
                    .product()
                    .document()
                    .materialize_node(*node, resources)
                    .map_err(|error| format!("materialize: {error:?}"))?,
            )),
            ExactSelectionRecord::Missing { step_index, .. } => Observation::Missing(*step_index),
            ExactSelectionRecord::TypeMismatch {
                step_index,
                actual_type,
                ..
            } => Observation::TypeMismatch(*step_index, *actual_type),
        })
    }
}

fn reference_observation(bytes: &[u8], path: &[Step], depth: u32) -> Result<Observation, String> {
    let mut resources = resources(depth)?;
    let registration = jqf_codec_json::registration().map_err(|error| format!("registration: {error:?}"))?;
    let mut provider = registration
        .decoder()
        .ok_or_else(|| "no decoder".to_owned())?
        .create_provider(source(bytes), request(), &mut resources)
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let requirement = try_lower_root_requirement(policy, Some(0), &resources)
        .map_err(|error| format!("requirement: {:?}", error.kind()))?;
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(CREDIT);
        let Ok(result) = session.decode(&mut run) else {
            return Ok(Observation::Reject);
        };
        let AccessOutcome::FullDocument(product) = result.into_parts().0 else {
            return Err("whole route returned a non-document outcome".to_owned());
        };
        navigate(product.document(), path, &mut resources)
    }
}

fn request<'a>() -> DecodeRequest<'a> {
    let dialect: &'static DialectId = Box::leak(Box::new(
        DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
    ));
    DecodeRequest {
        validation: ValidationMode::Strict,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        dialect,
        options: None,
        allow_adjacent_values: false,
        value_separator: jqf_codec_json::VALUE_SEPARATORS,
    }
}

fn navigate(
    document: &jqf_data::Document<'_>,
    path: &[Step],
    resources: &mut ResourceContext<'_>,
) -> Result<Observation, String> {
    let mut node = document.root_handle();
    for (cursor, step) in path.iter().enumerate() {
        let view = document.value_view(node).map_err(|error| format!("view: {error:?}"))?;
        match step {
            Step::Member(key) => match view.object().map_err(|error| format!("object: {error:?}"))? {
                Some(object) => match object.get(key) {
                    Some(value) => {
                        node = document
                            .node_handle(value.node())
                            .map_err(|error| format!("handle: {error:?}"))?;
                    }
                    None => return Ok(Observation::Missing(cursor)),
                },
                None => {
                    return Ok(Observation::TypeMismatch(
                        cursor,
                        view.kind().map_err(|error| format!("kind: {error:?}"))?,
                    ));
                }
            },
            Step::Index(index) => match view.array().map_err(|error| format!("array: {error:?}"))? {
                Some(array) => {
                    let position = if *index < 0 {
                        i64::try_from(array.len())
                            .ok()
                            .and_then(|len| len.checked_add(*index))
                            .and_then(|value| usize::try_from(value).ok())
                    } else {
                        usize::try_from(*index).ok()
                    };
                    let Some(position) = position else {
                        return Ok(Observation::Missing(cursor));
                    };
                    match array.get(position) {
                        Some(value) => {
                            node = document
                                .node_handle(value.node())
                                .map_err(|error| format!("handle: {error:?}"))?;
                        }
                        None => return Ok(Observation::Missing(cursor)),
                    }
                }
                None => {
                    return Ok(Observation::TypeMismatch(
                        cursor,
                        view.kind().map_err(|error| format!("kind: {error:?}"))?,
                    ));
                }
            },
        }
    }
    Ok(Observation::Node(semantic::jqf_value(
        &document
            .materialize_node(node, resources)
            .map_err(|error| format!("materialize: {error:?}"))?,
    )))
}
