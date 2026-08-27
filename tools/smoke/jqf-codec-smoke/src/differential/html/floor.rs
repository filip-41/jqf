//! The floor: the whole-document route, then an independent walk of the
//! recovered document.
//!
//! This is the reference every demand route is compared against. It shares no
//! code with the codec's `locate` module — it navigates the published
//! [`jqf_data::Document`] through the public view API and reads element names
//! through the attached-fact reader — so a route and this walk agreeing is
//! evidence, not a tautology.

use jqf_codec_core::{AccessOutcome, CodecRunContext, DiagnosticPolicy, ValidationMode};
use jqf_data::{Document, NodeHandle, NodeId, ValueKind};
use jqf_engine::{CodecRequirementPolicy, try_lower_root_requirement};

use super::corpus::Step;
use super::hash;
use super::route::{CREDIT, provider, resources};
use super::{Answer, RouteAnswers};

/// Every floor answer for one document and one path, from ONE decode.
pub(crate) fn answers(bytes: &[u8], steps: &[Step]) -> RouteAnswers {
    with_document(bytes, |document, names| {
        let located = navigate(document, names, steps);
        RouteAnswers {
            located: located_answer(document, names, &located),
        }
    })
    .unwrap_or_else(|error| RouteAnswers::failed(&error))
}

/// Decodes the whole document and hands it to `read` together with its element
/// names.
fn with_document<T>(bytes: &[u8], read: impl FnOnce(&Document<'_>, &[(NodeId, String)]) -> T) -> Result<T, String> {
    let mut resources = resources();
    let mut provider = provider(bytes, &mut resources)?;
    let policy = CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
    let requirement = try_lower_root_requirement(policy, Some(0), &resources)
        .map_err(|error| format!("floor requirement: {:?}", error.kind()))?;
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("floor bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("floor open: {:?}", error.kind()))?;
    {
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(CREDIT);
        let result = session
            .decode(&mut run)
            .map_err(|error| format!("floor decode: {:?}", error.kind()))?;
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            return Err("floor route published a non-document outcome".to_owned());
        };
        let names = hash::document_names(product.document(), &mut resources);
        Ok(read(product.document(), &names))
    }
}

/// One located reference: the same grammar the routes publish.
enum Located {
    /// An element (an array of its children) or a text leaf.
    Node(NodeHandle),
    /// An ordered child sequence — the plural member step's and the range
    /// step's vehicle, published as an array.
    Range(Vec<NodeHandle>),
    Missing(usize),
    Mismatch(usize, ValueKind),
}

/// Walks the exact path over the recovered document.
fn navigate(document: &Document<'_>, names: &[(NodeId, String)], steps: &[Step]) -> Located {
    let mut node = Located::Node(document.root_handle());
    for (index, step) in steps.iter().enumerate() {
        node = match step {
            Step::Member(key) => {
                let candidates = match &node {
                    Located::Node(handle) => {
                        if is_element(document, names, *handle) {
                            children(document, *handle)
                        } else {
                            return Located::Mismatch(index, ValueKind::String);
                        }
                    }
                    Located::Range(handles) => handles
                        .iter()
                        .filter(|handle| is_element(document, names, **handle))
                        .flat_map(|handle| children(document, *handle))
                        .collect(),
                    Located::Missing(_) | Located::Mismatch(..) => return node,
                };
                let matched: Vec<NodeHandle> = candidates
                    .into_iter()
                    .filter(|handle| name_at(document, names, *handle) == Some(*key))
                    .collect();
                match matched.len() {
                    0 => return Located::Mismatch(index, ValueKind::Array),
                    1 => Located::Node(matched[0]),
                    _ => Located::Range(matched),
                }
            }
            Step::Index(raw) => {
                let items = match sequence(document, names, &node, index) {
                    Ok(items) => items,
                    Err(negative) => return negative,
                };
                let Some(handle) = position(*raw, items.len()).and_then(|at| items.get(at).copied()) else {
                    return Located::Missing(index);
                };
                Located::Node(handle)
            }
            Step::Range(start, end) => {
                let items = match sequence(document, names, &node, index) {
                    Ok(items) => items,
                    Err(negative) => return negative,
                };
                let begin = bound(*start, items.len(), 0);
                let finish = bound(*end, items.len(), items.len());
                if begin >= finish {
                    Located::Range(Vec::new())
                } else {
                    Located::Range(items[begin..finish].to_vec())
                }
            }
        };
    }
    node
}

/// The children an index/range step addresses, or the negative observation the
/// step produces instead.
fn sequence(
    document: &Document<'_>,
    names: &[(NodeId, String)],
    node: &Located,
    index: usize,
) -> Result<Vec<NodeHandle>, Located> {
    match node {
        Located::Node(handle) => {
            if is_element(document, names, *handle) {
                Ok(children(document, *handle))
            } else {
                Err(Located::Mismatch(index, ValueKind::String))
            }
        }
        Located::Range(handles) => Ok(handles.clone()),
        Located::Missing(step) => Err(Located::Missing(*step)),
        Located::Mismatch(step, kind) => Err(Located::Mismatch(*step, *kind)),
    }
}

/// One index resolved against the container length: a negative index counts
/// from the end, and anything outside the container is absent.
fn position(raw: i64, len: usize) -> Option<usize> {
    let length = i64::try_from(len).ok()?;
    let resolved = if raw < 0 { length.checked_add(raw)? } else { raw };
    if resolved < 0 || resolved >= length {
        return None;
    }
    usize::try_from(resolved).ok()
}

/// One range bound resolved against the container length (the wrap-from-the-end
/// law, clamped at both ends).
fn bound(raw: Option<i64>, len: usize, open: usize) -> usize {
    let (Some(value), Ok(length)) = (raw, i64::try_from(len)) else {
        return open;
    };
    let resolved = if value < 0 { length.saturating_add(value) } else { value };
    usize::try_from(resolved.clamp(0, length)).unwrap_or(0)
}

fn children(document: &Document<'_>, handle: NodeHandle) -> Vec<NodeHandle> {
    let Ok(view) = document.value_view(handle) else {
        return Vec::new();
    };
    let Ok(Some(array)) = view.array() else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|item| document.node_handle(item.node()).ok())
        .collect()
}

fn name_at<'a>(document: &Document<'_>, names: &'a [(NodeId, String)], handle: NodeHandle) -> Option<&'a str> {
    let node = document.resolve_node_handle(handle).ok()?;
    hash::name_of(names, node)
}

/// An element is a node carrying a name fact; a text leaf carries none. This is
/// the same discriminator the engine's markup member step uses.
fn is_element(document: &Document<'_>, names: &[(NodeId, String)], handle: NodeHandle) -> bool {
    name_at(document, names, handle).is_some()
}

fn located_answer(document: &Document<'_>, names: &[(NodeId, String)], located: &Located) -> Answer {
    match located {
        Located::Node(handle) => Answer::Value(hash::node(document, names, *handle)),
        Located::Range(handles) => Answer::Value(hash::array(document, names, handles)),
        Located::Missing(step) => Answer::Missing(*step),
        Located::Mismatch(step, kind) => Answer::Mismatch(*step, *kind),
    }
}
