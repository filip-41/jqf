//! `render.tree@1`: a tree view of any owned semantic value.
//!
//! Every node line is exactly two spaces per depth, its full path, ` = `, an optional `&N ` definition, one term, and
//! LF. Root path is `$`; arrays append `[INDEX]`; object members append `[KEY]#OCCURRENCE` with the stored member
//! ordinal; a tag payload appends `.payload`. Terms are the shared scalar spelling (String in the strict-JSON-quoted,
//! tree-forced spelling), `array(COUNT)`, `object(COUNT)`, or `tag(JSON_STRING)`.
//!
//! Shared container allocations are anchored: a charged prepass counts occurrences per distinct `Array`/`Object`
//! allocation, assigns `N` in first-preorder order from zero, and the emit pass prints `&N ` before a first
//! occurrence's term and only `*N` as a later occurrence's term with no descendants. Both walks are iterative, so
//! nesting past the printer's own 10,000 ceiling errors instead of recursing or truncating. Both walks also descend
//! through tag layers into payload children — a container shared only under tags is anchored exactly where emit will
//! print it, and a tagged root anchors the document beneath it.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use jqf_codec_core::CodecError;
use jqf_data::{Array, Object, Value};

use super::error::unsupported;
use super::scalar::{StringStyle, write_json_quoted, write_scalar, write_tree_quoted};

/// One distinct container allocation discovered by the sharing prepass.
struct Slot {
    /// How many times it appears in the tree.
    occurrences: usize,
    /// Its anchor, assigned in first-preorder order among SHARED slots.
    anchor: Option<u32>,
}

/// A container reference for allocation-identity comparison.
#[derive(Clone, Copy)]
enum ContainerRef<'value> {
    /// An array allocation.
    Array(&'value Array),
    /// An object allocation.
    Object(&'value Object),
}

impl ContainerRef<'_> {
    /// The O(1) allocation-identity key: container kind plus the shared allocation's own address. Exact while the value
    /// tree is alive — every referenced allocation is pinned by its live refcount for the whole render, so two live
    /// allocations never share an address and two handles on one allocation carry one key. Empty containers are covered
    /// too: the key names the Arc allocation itself, which exists even when the element buffer does not.
    fn allocation_key(&self) -> (u8, usize) {
        match self {
            Self::Array(array) => (0, array.allocation_key()),
            Self::Object(object) => (1, object.allocation_key()),
        }
    }
}

/// The sharing prepass's slot table: slots in first-discovery (first-preorder) order — anchor numbering follows that
/// order — plus an O(log C) lookup keyed on `(kind, allocation key)`, so a document with C containers pays O(C log C)
/// instead of the O(C²) pairwise scan this table replaces. The key covers empty containers as well (their key names
/// the Arc allocation), which is what retired the per-kind empty buckets and their quadratic miss-scan.
struct SlotTable {
    /// The slots, in first-discovery order.
    slots: Vec<Slot>,
    /// `(kind, allocation key)` → slot index.
    by_key: BTreeMap<(u8, usize), usize>,
}

impl SlotTable {
    fn new() -> Self {
        SlotTable {
            slots: Vec::new(),
            by_key: BTreeMap::new(),
        }
    }

    /// Counts one occurrence of `reference`, inserting a new slot on its first sighting.
    fn occurrence(&mut self, reference: ContainerRef<'_>) {
        let key = reference.allocation_key();
        if let Some(index) = self.by_key.get(&key).copied() {
            self.slots[index].occurrences = self.slots[index].occurrences.saturating_add(1);
            return;
        }
        let index = self.slots.len();
        self.slots.push(Slot {
            occurrences: 1,
            anchor: None,
        });
        self.by_key.insert(key, index);
    }

    /// The slot naming the same allocation as `reference`, if any.
    fn find(&self, reference: &ContainerRef<'_>) -> Option<usize> {
        self.by_key.get(&reference.allocation_key()).copied()
    }
}

fn container_ref(value: &Value) -> Option<ContainerRef<'_>> {
    // A tag layer is its OWN node, never a container for sharing purposes: only its payload child (a distinct node) can
    // be shared.
    match value {
        Value::Tagged { .. } => None,
        _ => match value.untagged() {
            Value::Array(array) => Some(ContainerRef::Array(array)),
            Value::Object(object) => Some(ContainerRef::Object(object)),
            _ => None,
        },
    }
}

/// One node line's alias status.
#[derive(Clone, Copy)]
enum Alias {
    /// An ordinary node: no anchor.
    None,
    /// A shared container's first occurrence, carrying its anchor.
    First(u32),
    /// A shared container's later occurrence: only the alias, no descendants.
    Repeat(u32),
}

/// Renders one owned value as a tree frame. The frame ends with each node line's LF; the facade appends the final one.
///
/// # Errors
///
/// Returns an `UnsupportedShape` reject beyond the depth ceiling, an allocation failure, or an internal-contract error.
pub(crate) fn render(value: &Value) -> Result<String, CodecError> {
    let table = count_sharing(value)?;
    let mut out = String::new();
    emit(value, &table, &mut out)?;
    // Every node line carries its LF; the final line's LF is the FACADE's, so the encoder's frame ends without one
    // (exactly like the table renderers).
    out.pop();
    Ok(out)
}

/// The sharing prepass: count occurrences per distinct container allocation and assign anchors in first-preorder order
/// among the shared ones. The walk descends through tag layers into payload children exactly as emit does, so a
/// container shared only under tags is counted where emit will print it (and a tagged root anchors the document beneath
/// it).
fn count_sharing(root: &Value) -> Result<SlotTable, CodecError> {
    let mut table = SlotTable::new();
    let mut next: Option<&Value> = Some(root);
    let mut stack: Vec<Cursor<'_>> = Vec::new();
    loop {
        if let Some(node) = next.take() {
            let reference = container_ref(node);
            if let Some(reference) = reference {
                table.occurrence(reference);
            }
            // Descend into containers AND tag payloads: emit descends into both, so a container shared only under tags
            // must be counted where emit will print it. The old prepass stopped at tag layers, which is how such a
            // container printed in full at every occurrence and a tagged root disabled anchoring entirely.
            if matches!(node, Value::Tagged { .. }) || reference.is_some() {
                if stack.len() >= crate::MAX_NESTING_DEPTH {
                    return Err(unsupported(
                        "tree-depth",
                        "the value nests past the tree renderer's depth ceiling",
                    ));
                }
                stack.push(cursor_for(node));
            }
        }
        if !step_cursor(&mut stack, &mut next) {
            break;
        }
    }
    // Assign anchors to shared slots in slot order (first-preorder).
    let mut anchor = 0_u32;
    for slot in &mut table.slots {
        if slot.occurrences > 1 {
            slot.anchor = Some(anchor);
            anchor = anchor.saturating_add(1);
        }
    }
    Ok(table)
}

/// The emit pass: one line per node in preorder, descending only into first occurrences of shared containers (and into
/// every tag payload).
fn emit(root: &Value, table: &SlotTable, out: &mut String) -> Result<(), CodecError> {
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut path = String::from("$");
    let mut next: Option<&Value> = Some(root);
    // Per-slot "first occurrence already visited" flags for the shared law.
    let mut seen: Vec<bool> = alloc::vec![false; table.slots.len()];
    loop {
        if let Some(node) = next.take() {
            let depth = stack.len();
            if depth >= crate::MAX_NESTING_DEPTH {
                return Err(unsupported(
                    "tree-depth",
                    "the value nests past the tree renderer's depth ceiling",
                ));
            }
            // One slot lookup per node. The prepass registered every container, so a miss here means a scalar, a tag,
            // or the defensive never-registered arm below.
            let slot = container_ref(node).and_then(|reference| table.find(&reference));
            let alias = node_alias(slot, table, &seen);
            let descend = match node {
                Value::Tagged { .. } => true,
                _ => match slot {
                    Some(index) => table.slots[index].anchor.is_none() || !seen[index],
                    None => container_ref(node).is_some(),
                },
            };
            write_line(out, depth, &path, node, alias)?;
            if descend
                && let Some(index) = slot
                && table.slots[index].anchor.is_some()
            {
                seen[index] = true;
            }
            if descend && let Some(frame) = frame_for(node, path.len()) {
                stack.push(frame);
            }
        }
        if !step(&mut stack, &mut next, Some(&mut path)) {
            break;
        }
    }
    Ok(())
}

/// A node's alias status under the sharing law.
fn node_alias(slot: Option<usize>, table: &SlotTable, seen: &[bool]) -> Alias {
    match slot {
        Some(index) => match table.slots[index].anchor {
            Some(anchor) => {
                if seen[index] {
                    Alias::Repeat(anchor)
                } else {
                    Alias::First(anchor)
                }
            }
            None => Alias::None,
        },
        None => Alias::None,
    }
}

/// Writes one node line: indent, path, ` = `, optional anchor, term, LF.
fn write_line(out: &mut String, depth: usize, path: &str, node: &Value, alias: Alias) -> Result<(), CodecError> {
    for _ in 0..depth {
        push(out, "  ");
    }
    push(out, path);
    push(out, " = ");
    match alias {
        Alias::None => {}
        Alias::First(anchor) => {
            push(out, &alloc::format!("&{anchor} "));
        }
        Alias::Repeat(anchor) => {
            push(out, &alloc::format!("*{anchor}"));
            push(out, "\n");
            return Ok(());
        }
    }
    write_term(out, node)?;
    push(out, "\n");
    Ok(())
}

/// Writes one node term.
fn write_term(out: &mut String, node: &Value) -> Result<(), CodecError> {
    match node {
        Value::Array(array) => {
            push(out, &alloc::format!("array({})", array.len()));
            Ok(())
        }
        Value::Object(object) => {
            push(out, &alloc::format!("object({})", object.len()));
            Ok(())
        }
        Value::Tagged { tag, .. } => {
            push(out, "tag(");
            write_tree_quoted(out, tag.as_str());
            push(out, ")");
            Ok(())
        }
        _ => write_scalar(out, node, StringStyle::TreeQuoted),
    }
}

/// One container being walked, and how far the walk has got through it.
enum Cursor<'value> {
    /// An array being iterated.
    Array {
        /// The array.
        array: &'value Array,
        /// Next element index.
        index: usize,
    },
    /// An object being iterated.
    Object {
        /// The object.
        object: &'value Object,
        /// Next member index (the stored ordinal).
        index: usize,
    },
    /// A tagged value whose payload child is pending or done.
    Tagged {
        /// The tagged payload.
        payload: &'value Value,
        /// Whether the payload child has been yielded.
        visited: bool,
    },
}

fn cursor_for(value: &Value) -> Cursor<'_> {
    match value {
        Value::Tagged { payload, .. } => Cursor::Tagged {
            payload,
            visited: false,
        },
        _ => match value.untagged() {
            Value::Array(array) => Cursor::Array { array, index: 0 },
            Value::Object(object) => Cursor::Object { object, index: 0 },
            _ => unreachable!("scalars have no cursor"),
        },
    }
}

/// One emit frame: a cursor plus the path length when the frame was entered.
struct Frame<'value> {
    /// The container cursor.
    cursor: Cursor<'value>,
    /// The path length at this container's own line, so children and the close both truncate back to it.
    path_len_before: usize,
}

/// The frame to descend into one node's children, if any.
fn frame_for(node: &Value, path_len: usize) -> Option<Frame<'_>> {
    match node {
        Value::Tagged { .. } => Some(Frame {
            cursor: cursor_for(node),
            path_len_before: path_len,
        }),
        _ => match node.untagged() {
            Value::Array(_) | Value::Object(_) => Some(Frame {
                cursor: cursor_for(node),
                path_len_before: path_len,
            }),
            _ => None,
        },
    }
}

/// Advances the sharing prepass's innermost cursor by one child, or closes it. Returns `false` when the walk is done.
fn step_cursor<'value>(stack: &mut Vec<Cursor<'value>>, next: &mut Option<&'value Value>) -> bool {
    let Some(cursor) = stack.last_mut() else {
        return false;
    };
    match cursor {
        Cursor::Array { array, index } => {
            let (array, at) = (*array, *index);
            if let Some(child) = array.get(at) {
                *index = at + 1;
                *next = Some(child);
                return true;
            }
        }
        Cursor::Object { object, index } => {
            let (object, at) = (*object, *index);
            if let Some(entry) = object.get_index(at) {
                *index = at + 1;
                *next = Some(entry.value());
                return true;
            }
        }
        Cursor::Tagged { payload, visited } => {
            if !*visited {
                *visited = true;
                *next = Some(payload);
                return true;
            }
        }
    }
    stack.pop();
    true
}

/// Advances the emit walk's innermost frame by one child, or closes it. Returns `false` when the walk is done.
fn step<'value>(stack: &mut Vec<Frame<'value>>, next: &mut Option<&'value Value>, path: Option<&mut String>) -> bool {
    let Some(frame) = stack.last_mut() else {
        return false;
    };
    let path_len_before = frame.path_len_before;
    match &mut frame.cursor {
        Cursor::Array { array, index } => {
            let (array, at) = (*array, *index);
            if let Some(child) = array.get(at) {
                *index = at + 1;
                if let Some(path) = path {
                    path.truncate(path_len_before);
                    let _ = write!(path, "[{at}]");
                }
                *next = Some(child);
                return true;
            }
        }
        Cursor::Object { object, index } => {
            let (object, at) = (*object, *index);
            if let Some(entry) = object.get_index(at) {
                *index = at + 1;
                if let Some(path) = path {
                    path.truncate(path_len_before);
                    path.push('[');
                    write_json_quoted(path, entry.key(), true);
                    let _ = write!(path, "]#{at}");
                }
                *next = Some(entry.value());
                return true;
            }
        }
        Cursor::Tagged { payload, visited } => {
            if !*visited {
                *visited = true;
                if let Some(path) = path {
                    path.truncate(path_len_before);
                    path.push_str(".payload");
                }
                *next = Some(payload);
                return true;
            }
        }
    }
    stack.pop();
    if let Some(path) = path {
        path.truncate(path_len_before);
    }
    true
}

/// Appends `text` to `out`.
fn push(out: &mut String, text: &str) {
    out.push_str(text);
}
