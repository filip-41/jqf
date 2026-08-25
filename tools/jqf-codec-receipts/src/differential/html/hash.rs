//! One value hash, used on BOTH sides of every comparison.
//!
//! The two sides publish different DOCUMENTS — a demand route builds a fresh
//! subtree/run document, the floor navigates the whole recovered one — so the
//! answers can only be compared as values. The hash walks a node's semantic
//! projection (kind, scalar text, ordered children) plus its element NAME fact,
//! which is the one attached fact a markup answer must carry: a route that
//! selected the wrong same-shaped sibling would pass a name-blind compare.

use jqf_data::{
    BatchLimit, Document, FactPayloadView, LocalOwnerRef, NodeHandle, NodeId, ReaderPoll, ScalarView, ValueKind,
};
use jqf_resource::ResourceContext;

const CREDIT: u32 = 4_096;
const SEED: u64 = 0xcbf2_9ce4_8422_2325;

const TAG_NULL: u64 = 1;
const TAG_BOOL: u64 = 2;
const TAG_NUMBER: u64 = 4;
const TAG_STRING: u64 = 7;
const TAG_ARRAY_OPEN: u64 = 9;
const TAG_ARRAY_CLOSE: u64 = 0x0a;
const TAG_OBJECT_OPEN: u64 = 10;
const TAG_OBJECT_CLOSE: u64 = 0x0b;
const TAG_NAME: u64 = 0x11;
const TAG_OPAQUE: u64 = 0xbeef;

/// The element names of a document, by owner node.
///
/// Read through the attached-fact reader rather than the finalize-time owner
/// index: a document built through the plain `finish` path carries no index,
/// and both sides of a comparison must read names the same way.
pub(crate) fn document_names(document: &Document<'_>, resources: &mut ResourceContext<'_>) -> Vec<(NodeId, String)> {
    let mut names = Vec::new();
    let Ok(mut reader) = document.fact_reader(resources) else {
        return names;
    };
    let Some(limit) = BatchLimit::new(usize::MAX) else {
        return names;
    };
    loop {
        match reader.poll_batch(limit, resources) {
            Ok(ReaderPoll::Batch(batch)) => {
                for fact in batch.iter() {
                    if !role_is(fact.role().as_str(), "name") {
                        continue;
                    }
                    let FactPayloadView::Text(text) = fact.payload() else {
                        continue;
                    };
                    let LocalOwnerRef::Node(owner) = fact.owner() else {
                        continue;
                    };
                    names.push((owner, String::from(text)));
                }
            }
            Ok(ReaderPoll::Pending) => {
                if resources.try_begin_next_cooperative_entry(CREDIT).is_err() {
                    return names;
                }
            }
            // The end of the facts, and a reader that cannot go on, are the
            // same observation here: these are the names there are.
            Ok(ReaderPoll::End(_)) | Err(_) => return names,
        }
    }
}

/// Whether a fact role names `selector` — the engine's own accessor match
/// (`html.name@1` and `name` are the same role).
fn role_is(role: &str, selector: &str) -> bool {
    if role == selector {
        return true;
    }
    let core = role.rsplit_once('.').map_or(role, |(_, rest)| rest);
    let semantic = core.split_once('@').map_or(core, |(semantic, _)| semantic);
    semantic == selector
}

/// The element name of one node, when it has one (elements do, text leaves do
/// not — which is also how the reference walk tells the two apart).
pub(crate) fn name_of(names: &[(NodeId, String)], node: NodeId) -> Option<&str> {
    names
        .iter()
        .find(|(owner, _)| *owner == node)
        .map(|(_, name)| name.as_str())
}

/// The value hash of one node.
#[must_use]
pub(crate) fn node(document: &Document<'_>, names: &[(NodeId, String)], handle: NodeHandle) -> u64 {
    let mut hash = SEED;
    walk(document, names, handle, &mut hash);
    hash
}

/// The value hash of an ordered sequence of nodes published as one array (the
/// range/run shape: a located range's document root IS this array).
#[must_use]
pub(crate) fn array(document: &Document<'_>, names: &[(NodeId, String)], handles: &[NodeHandle]) -> u64 {
    let mut hash = SEED;
    mix(&mut hash, TAG_ARRAY_OPEN);
    for handle in handles {
        walk(document, names, *handle, &mut hash);
    }
    mix(&mut hash, TAG_ARRAY_CLOSE);
    hash
}

fn mix(hash: &mut u64, value: u64) {
    *hash ^= value;
    *hash = hash.wrapping_mul(0x100_0000_01b3);
}

fn mix_bytes(hash: &mut u64, bytes: &[u8]) {
    mix(hash, bytes.len() as u64);
    for byte in bytes {
        mix(hash, u64::from(*byte));
    }
}

fn walk(document: &Document<'_>, names: &[(NodeId, String)], handle: NodeHandle, hash: &mut u64) {
    if let Ok(node) = document.resolve_node_handle(handle)
        && let Some(name) = name_of(names, node)
    {
        mix(hash, TAG_NAME);
        mix_bytes(hash, name.as_bytes());
    }
    let Ok(view) = document.value_view(handle) else {
        mix(hash, TAG_OPAQUE);
        return;
    };
    if let Ok(Some(scalar)) = view.scalar() {
        match scalar {
            ScalarView::Null => mix(hash, TAG_NULL),
            ScalarView::Bool(value) => mix(hash, TAG_BOOL + u64::from(value)),
            ScalarView::Number(_) => mix(hash, TAG_NUMBER),
            ScalarView::String(text) => {
                mix(hash, TAG_STRING);
                mix_bytes(hash, text.as_bytes());
            }
            _ => mix(hash, TAG_OPAQUE),
        }
        return;
    }
    match view.kind() {
        Ok(ValueKind::Array) => {
            mix(hash, TAG_ARRAY_OPEN);
            if let Ok(Some(array)) = view.array() {
                for item in array.iter() {
                    if let Ok(child) = document.node_handle(item.node()) {
                        walk(document, names, child, hash);
                    }
                }
            }
            mix(hash, TAG_ARRAY_CLOSE);
        }
        Ok(ValueKind::Object) => {
            mix(hash, TAG_OBJECT_OPEN);
            if let Ok(Some(object)) = view.object() {
                for entry in object.iter().flatten() {
                    mix_bytes(hash, entry.key().as_bytes());
                    if let Ok(child) = document.node_handle(entry.value().node()) {
                        walk(document, names, child, hash);
                    }
                }
            }
            mix(hash, TAG_OBJECT_CLOSE);
        }
        _ => mix(hash, TAG_OPAQUE),
    }
}
