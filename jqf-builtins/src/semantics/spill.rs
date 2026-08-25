//! The external-sort spill path for `sort`/`sort_by`: when a keyed sort's keys exceed the request's spill budget, the
//! overflow is written as sorted runs to the host spill store and merged with a bounded heap, instead of holding every
//! key in memory. `group_by`/`unique_by` stay in memory — their run-detection needs the adjacent ORDER, which the
//! merge does not expose.
//!
//! # The soundness law
//!
//! The spilled ORDER is byte-identical to the in-memory order, by construction:
//! the same stable `compare_key` comparator orders each run (over the ORIGINAL owned keys, never decoded copies), the
//! merge re-compares the runs' head keys with the SAME comparator, and the position tiebreak is the same stability law
//! the in-memory `sort_entries` keeps. The output is rebuilt from the merged POSITION sequence exactly as the in-memory
//! path rebuilds it from the sorted entries — the spill changes the order computation, never the publication.
//!
//! # The decline law
//!
//! Spill is an OPTIMIZATION with a fallback, never a new meaning: any doubt — no store, no budget, a non-scalar key
//! (the scalar encoding is closed), an allocation failure — declines to the in-memory sort with zero observable
//! difference. A HOST failure FAILS the request with the host error, even before the first run was written: a sort that
//! cannot spill at all is not the floor, and handing the whole dataset to the memory governor is the unbounded growth
//! the spill pillar exists to prevent.
//!
//! # The budget's meaning
//!
//! `max_spill_bytes` bounds the PER-RUN in-memory key footprint (the chunk that is sorted and written before the next
//! run forms) — the request's key residency never exceeds it. DISK usage is the dataset size: an external sort's runs
//! total the keys.
//!
//! # The disk ceiling
//!
//! `max_spill_disk_bytes` (the ledger's SIXTH dimension, opt-in via `--max-spill-disk-bytes`) bounds the request's
//! CUMULATIVE run-file bytes on the host store. The charge happens at the run-write site, BEFORE the run is created, so
//! a refusal has zero host side effects; the charge is never released (an external sort's runs coexist on disk until
//! the merge finishes, so the cumulative charge IS the peak). A refusal is a machine `ResourceLimit::SpillDiskBytes`
//! limit-exceeded error that FAILS the request — it never declines, even on the first run: declining would hand the
//! whole dataset to the memory governor, trading a bounded disk breach for the unbounded memory growth the RSS pillar
//! exists to prevent. With the ceiling unset (the default) the admission always succeeds and every existing spill path
//! answers exactly as before.
//!
//! # The scalar encoding
//!
//! One byte tag plus payload, EXACTLY round-trippable:
//!
//! | tag | value | payload | | --- | --- | --- | | 0 | `null` | — | | 1 | `false` | — | | 2 | `true` | — | | 3 |
//! string | u32 byte length + UTF-8 bytes | | 4 | integer | u32 text length + canonical decimal text (big integers
//! only) | | 5 | decimal | u32 text length + canonical `DecimalText` spelling | | 6 | float | the f64's raw bits
//! (exact) | | 7 | array | u32 count + u32-length-prefixed elements | | 8 | machine integer | i64 two's-complement
//! little-endian (8 bytes) |
//!
//! The array row is one level deep: its length-prefixed elements are scalar encodings, so a nested non-scalar element
//! declines the whole spill.
//! Objects, tagged values, and byte strings are not encodable and decline the whole spill — the common overflow
//! market (huge number/string arrays) is scalar, and the closed table is the soundness boundary.

use alloc::collections::BinaryHeap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use core::cmp::Ordering;

use jqf_data::{Decimal, Float, Integer, Number, Value};
use jqf_resource::{ResourceContext, ResourceError, RunCursorId, RunId, SpillStore};

use super::depth::{TooDeep, comparison_error};
use super::keyed::KeyedEntry;
use super::keyed::{KeyValue, compare_key};
use super::order::NanLaw;
use crate::error::EngineRunError;

const TAG_NULL: u8 = 0;
const TAG_FALSE: u8 = 1;
const TAG_TRUE: u8 = 2;
const TAG_STRING: u8 = 3;
const TAG_INTEGER: u8 = 4;
const TAG_DECIMAL: u8 = 5;
const TAG_FLOAT: u8 = 6;
const TAG_ARRAY: u8 = 7;
/// Machine `i64`: tag + 8 little-endian two's-complement bytes. Decode accepts only this form for the tag; big integers
/// that do not fit `i64` stay on [`TAG_INTEGER`]'s length-prefixed decimal text so sort keys remain order-correct after
/// decode.
const TAG_I64: u8 = 8;

fn host(error: ResourceError) -> EngineRunError {
    EngineRunError::Codec(jqf_codec_core::CodecError::from(error))
}

/// The canonical decimal text [`Decimal::parse`] round-trips — the `render::push_decimal` walk, with `.ok()?`
/// preserving the spill's decline-on-failure.
fn decimal_text(decimal: &Decimal) -> Option<String> {
    let mut out = String::new();
    super::render::push_decimal(&mut out, decimal.coefficient().as_str(), decimal.scale(), usize::MAX).ok()?;
    Some(out)
}

/// Encodes one scalar key into `out`. Returns `None` when the key is not a scalar — the caller DECLINES the whole
/// spill (the closed table).
fn encode_key(out: &mut Vec<u8>, key: &KeyValue) -> Option<()> {
    match key {
        KeyValue::Empty => encode_array(out, 0, core::iter::empty()),
        KeyValue::One(value) => encode_array(out, 1, core::iter::once(value)),
        KeyValue::Many(array) => encode_array(out, array.len(), array.iter()),
        KeyValue::Bare(value) => encode_value(out, value),
    }
}

/// The spill key encoder, for the perf A/B harness only
/// (`benchmark-internals`): encodes one key value through the production
/// path and discards the bytes.
#[cfg(feature = "benchmark-internals")]
pub fn encode_key_for_bench(key: &Value) -> Option<()> {
    let mut out = alloc::vec::Vec::new();
    encode_value(&mut out, key)
}

/// Encodes one scalar-or-bare value key into `out`. Returns `None` when the value is not encodable — the caller
/// DECLINES the whole spill (the closed table).
fn encode_value(out: &mut Vec<u8>, key: &Value) -> Option<()> {
    fn text(out: &mut Vec<u8>, text: &str) -> Option<()> {
        let len = u32::try_from(text.len()).ok()?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(text.as_bytes());
        Some(())
    }
    // Temporals have no spill encoding. The decline MUST sit above the match: a catch-all `unreachable!` that runs
    // first aborts the process on a valid `sort_by` over temporal keys once `--max-spill-bytes` engages.
    if key.is_temporal() {
        return None;
    }
    match key.untagged() {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(false) => out.push(TAG_FALSE),
        Value::Bool(true) => out.push(TAG_TRUE),
        Value::String(value) => {
            let bytes = value.as_bytes();
            let len = u32::try_from(bytes.len()).ok()?;
            out.push(TAG_STRING);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        }
        Value::Number(number) => match number.category() {
            jqf_data::NumberCategory::Integer => {
                if let Some(value) = number.as_machine() {
                    out.push(TAG_I64);
                    out.extend_from_slice(&value.to_le_bytes());
                } else {
                    out.push(TAG_INTEGER);
                    text(out, number.to_integer()?.as_str())?;
                }
            }
            jqf_data::NumberCategory::Decimal => {
                out.push(TAG_DECIMAL);
                text(out, &decimal_text(number.as_decimal()?)?)?;
            }
            jqf_data::NumberCategory::Float => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&number.as_float()?.get().to_bits().to_le_bytes());
            }
        },
        // The `_by` KEY is the ARRAY of the key filter's outputs — a small array of scalars in the common case. Its
        // elements are encoded length-prefixed; a nested non-scalar element declines the whole spill (the closed
        // table).
        Value::Array(array) => {
            encode_array(out, array.len(), array.iter())?;
        }
        Value::Object(_) | Value::Tagged { .. } | Value::Bytes(_) => {
            return None;
        }
        Value::LocalDate(_) | Value::LocalTime(_) | Value::LocalDateTime(_) | Value::OffsetDateTime(_) => {
            unreachable!("temporal kinds decline before the match")
        }
    }
    Some(())
}

/// Encodes `count` length-prefixed key elements after the array header.
fn encode_array<'a>(out: &mut Vec<u8>, count: usize, elements: impl Iterator<Item = &'a Value>) -> Option<()> {
    out.push(TAG_ARRAY);
    let count = u32::try_from(count).ok()?;
    out.extend_from_slice(&count.to_le_bytes());
    for element in elements {
        // The prefix is RESERVED first and patched after its element lands:
        // splicing four bytes in front of every element shifts the whole
        // encoded tail per element, which is quadratic for wide keys.
        let prefix = out.len();
        out.extend_from_slice(&0u32.to_le_bytes());
        encode_value(out, element)?;
        let len = u32::try_from(out.len() - prefix - 4).ok()?;
        out[prefix..prefix + 4].copy_from_slice(&len.to_le_bytes());
    }
    Some(())
}

/// Decodes one key from the front of `bytes`, returning the value and the bytes it consumed.
#[allow(
    clippy::too_many_lines,
    reason = "the spill key decode walks the whole key grammar in one fn; the allocation collapse re-wrapped statements across extra lines without adding statements"
)]
fn decode_key(bytes: &[u8]) -> Result<(&[u8], Value), EngineRunError> {
    let (&tag, rest) = bytes
        .split_first()
        .ok_or_else(|| EngineRunError::internal_contract("empty spill entry"))?;
    let (payload, consumed) = match tag {
        TAG_NULL | TAG_FALSE | TAG_TRUE => (&[] as &[u8], 1usize),
        TAG_ARRAY => (&[] as &[u8], 5usize),
        TAG_I64 => (
            rest.get(..8)
                .ok_or_else(|| EngineRunError::internal_contract("spill i64 truncated"))?,
            9,
        ),
        TAG_STRING | TAG_INTEGER | TAG_DECIMAL => {
            let len = u32::from_le_bytes(
                rest.get(..4)
                    .ok_or_else(|| EngineRunError::internal_contract("spill entry truncated"))?
                    .try_into()
                    .map_err(|_| EngineRunError::internal_contract("spill length"))?,
            ) as usize;
            let bytes = rest
                .get(4..4 + len)
                .ok_or_else(|| EngineRunError::internal_contract("spill payload truncated"))?;
            (bytes, 5 + len)
        }
        TAG_FLOAT => (
            rest.get(..8)
                .ok_or_else(|| EngineRunError::internal_contract("spill float truncated"))?,
            9,
        ),
        _ => return Err(EngineRunError::internal_contract("spill tag unknown")),
    };
    let value = match tag {
        TAG_ARRAY => {
            // The `_by` key: count then length-prefixed scalar elements.
            let count = u32::from_le_bytes(
                rest.get(..4)
                    .ok_or_else(|| EngineRunError::internal_contract("spill array count"))?
                    .try_into()
                    .map_err(|_| EngineRunError::internal_contract("spill array count"))?,
            ) as usize;
            let mut cursor = rest
                .get(4..)
                .ok_or_else(|| EngineRunError::internal_contract("spill array payload"))?;
            let mut elements =
                jqf_data::Array::try_with_capacity(count).map_err(|_| EngineRunError::allocation_failure())?;
            for _ in 0..count {
                let len = u32::from_le_bytes(
                    cursor
                        .get(..4)
                        .ok_or_else(|| EngineRunError::internal_contract("spill element len"))?
                        .try_into()
                        .map_err(|_| EngineRunError::internal_contract("spill element len"))?,
                ) as usize;
                let element_bytes = cursor
                    .get(4..4 + len)
                    .ok_or_else(|| EngineRunError::internal_contract("spill element payload"))?;
                let (after, element) = decode_key(element_bytes)?;
                if !after.is_empty() {
                    return Err(EngineRunError::internal_contract("spill element overrun"));
                }
                elements
                    .try_push(element)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                cursor = cursor
                    .get(4 + len..)
                    .ok_or_else(|| EngineRunError::internal_contract("spill array overrun"))?;
            }
            return Ok((cursor, Value::Array(elements)));
        }
        TAG_NULL => Value::Null,
        TAG_FALSE => Value::Bool(false),
        TAG_TRUE => Value::Bool(true),
        TAG_STRING => {
            let text = core::str::from_utf8(payload)
                .map_err(|_| EngineRunError::internal_contract("spill string not UTF-8"))?;
            Value::try_string(text).map_err(|_| EngineRunError::allocation_failure())?
        }
        TAG_I64 => {
            let bits: [u8; 8] = payload
                .try_into()
                .map_err(|_| EngineRunError::internal_contract("spill i64 bits"))?;
            Value::Number(Number::integer(Integer::from_i64(i64::from_le_bytes(bits))))
        }
        TAG_INTEGER => {
            let text = core::str::from_utf8(payload)
                .map_err(|_| EngineRunError::internal_contract("spill integer not UTF-8"))?;
            let integer = Integer::parse(text).map_err(|_| EngineRunError::internal_contract("spill int"))?;
            Value::Number(Number::try_integer_unaccounted(integer).map_err(|_| EngineRunError::allocation_failure())?)
        }
        TAG_DECIMAL => {
            let text = core::str::from_utf8(payload)
                .map_err(|_| EngineRunError::internal_contract("spill decimal not UTF-8"))?;
            let decimal = Decimal::parse(text).map_err(|_| EngineRunError::internal_contract("spill dec"))?;
            Value::Number(Number::try_decimal_unaccounted(decimal).map_err(|_| EngineRunError::allocation_failure())?)
        }
        TAG_FLOAT => {
            let bits = u64::from_le_bytes(
                payload
                    .try_into()
                    .map_err(|_| EngineRunError::internal_contract("spill float bits"))?,
            );
            Value::Number(Number::float(Float::new(f64::from_bits(bits))))
        }
        _ => return Err(EngineRunError::internal_contract("spill tag unknown")),
    };
    Ok((
        bytes
            .get(consumed..)
            .ok_or_else(|| EngineRunError::internal_contract("spill entry overruns the buffer"))?,
        value,
    ))
}

/// The merge head of one run: its next key and position, plus the cursor.
struct RunHead {
    key: KeyValue,
    position: u64,
    cursor: RunCursorId,
}

/// [`RunHead`] ordered for a [`BinaryHeap`] that yields the SMALLEST head.
///
/// [`BinaryHeap`] is a max-heap, so `cmp` is [`compare_heads`] reversed: the head the merge must publish next compares
/// `Greater` and pops first. The order is strict — positions are unique — so the heap's answer is exactly the
/// linear scan's, for the same reason [`compare_heads`] is a total order over (key, position). This is what turns the
/// merge's per-step O(runs) scan into an O(log runs) pop.
///
/// The second field is the merge's SHARED depth-tripped flag: a comparison that runs past the ceiling cannot fail the
/// heap's infallible `Ord`, so it records the trip here and the drive checks the flag before publishing.
struct HeapHead(RunHead, Rc<Cell<bool>>);

impl PartialEq for HeapHead {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for HeapHead {}

impl PartialOrd for HeapHead {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapHead {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_heads(&other.0, &self.0).unwrap_or_else(|TooDeep| {
            // The merge comparator is infallible, so a too-deep comparison reports the only ordering it may —
            // `Equal`, which leaves the head where it was — and sets the shared flag the drive checks before it
            // publishes anything.
            self.1.set(true);
            Ordering::Equal
        })
    }
}

/// Maps a decoded run key back to the [`KeyValue`] the in-memory entry held:
/// an array decodes to `Empty`/`One`/`Many` by its length (the `_by` boxed form), any other value to `Bare` (the
/// arity-zero whole-value form).
fn decoded_key(value: Value) -> Result<KeyValue, EngineRunError> {
    match value {
        Value::Array(array) => match array.len() {
            0 => Ok(KeyValue::Empty),
            1 => Ok(KeyValue::One(
                array
                    .try_into_vec()
                    .map_err(|_| EngineRunError::allocation_failure())?
                    .pop()
                    .expect("length-1 array"),
            )),
            _ => Ok(KeyValue::Many(array)),
        },
        value => Ok(KeyValue::Bare(value)),
    }
}

/// Flushes the current in-memory entries as ONE sorted run, when the spill conditions hold. Returns `Ok(true)` when a
/// run was written (the caller clears its entries), `Ok(false)` when the spill DECLINES (no store, no budget, or a
/// non-scalar key — the caller keeps its in-memory law).
pub fn try_flush(
    store: &dyn SpillStore,
    entries: &[KeyedEntry],
    runs: &mut Vec<RunId>,
    resources: &ResourceContext<'_>,
) -> Result<bool, EngineRunError> {
    if entries.is_empty() {
        return Ok(false);
    }
    let mut sorted: Vec<usize> = (0..entries.len()).collect();
    let mut too_deep = false;
    sorted.sort_by(|&left, &right| {
        compare_entries(&entries[left], &entries[right]).unwrap_or_else(|TooDeep| {
            too_deep = true;
            Ordering::Equal
        })
    });
    let sorted = sorted;
    if too_deep {
        // The in-memory sort raises the depth error, and the spill must too:
        // sorting a too-deep key by input position is a silent wrong answer, not a fallback. Checked BEFORE a run is
        // written, so a refusal has zero host side effects.
        return Err(comparison_error(resources));
    }
    let mut ordered = Vec::new();
    for &index in &sorted {
        let start = ordered.len();
        if encode_key(&mut ordered, &entries[index].key).is_none() {
            // The closed table: a non-scalar key declines the WHOLE spill.
            return Ok(false);
        }
        let key_len =
            u32::try_from(ordered.len() - start).map_err(|_| EngineRunError::internal_contract("spill key length"))?;
        ordered.splice(start..start, key_len.to_le_bytes());
        ordered.extend_from_slice(&(entries[index].position as u64).to_le_bytes());
    }
    // The disk ceiling's charge site: the run's exact byte count is charged BEFORE the run is created, so a ceiling
    // refusal has zero host side effects — not even a directory — and a refusal is a machine resource error that
    // fails the request (it never declines: see the module docs). The ceiling is OPT-IN: 0 is unset, and the charge is
    // skipped entirely, so with the dial never given the admission always succeeds and the fallback law is
    // byte-identical.
    if resources.limits().max_spill_disk_bytes() > 0 {
        resources.charge_spill_disk(ordered.len() as u64).map_err(host)?;
    }
    let id = store.create_run().map_err(host)?;
    if let Err(error) = store.write_run(id, &ordered) {
        // The run exists but is not usable: remove it so a failed flush leaves no run bytes on the host store. The
        // ceiling charge stays held by design, which is what makes reclaiming the FILE here the only cleanup left to
        // do.
        let _ = store.delete_run(id);
        return Err(host(error));
    }
    runs.push(id);
    Ok(true)
}

/// The merge body. [`merge_run_positions`] wraps this so every failure path deletes every run: the runs are charged
/// against the disk ceiling for the request's lifetime, and an error aborts the request — leaving them on the host
/// store would hold dead bytes until teardown.
fn merge_run_positions_inner(
    store: &dyn SpillStore,
    runs: &[RunId],
    total: usize,
    resources: &ResourceContext<'_>,
) -> Result<Vec<usize>, EngineRunError> {
    let too_deep = Rc::new(Cell::new(false));
    let mut head_storage: Vec<HeapHead> = Vec::new();
    head_storage
        .try_reserve_exact(runs.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    let mut heads: BinaryHeap<HeapHead> = BinaryHeap::from(head_storage);
    // One caller-owned read buffer, cleared per read: the entry bytes never need to outlive the decode (each head's key
    // is decoded to an OWNED [`KeyValue`], because it must persist in the heap across pops).
    let mut buf = Vec::new();
    for &id in runs {
        let cursor = store.open_run(id).map_err(host)?;
        if let Some(position) = store.read_next(cursor, &mut buf).map_err(host)? {
            let (_, key) = decode_key(&buf)?;
            heads.push(HeapHead(
                RunHead {
                    key: decoded_key(key)?,
                    position,
                    cursor,
                },
                Rc::clone(&too_deep),
            ));
        }
        buf.clear();
    }
    let mut positions: Vec<usize> = Vec::new();
    positions
        .try_reserve_exact(total)
        .map_err(|_| EngineRunError::allocation_failure())?;
    while let Some(head) = heads.pop() {
        let head = head.0;
        positions.push(usize::try_from(head.position).unwrap_or(usize::MAX));
        if let Some(position) = store.read_next(head.cursor, &mut buf).map_err(host)? {
            let (_, key) = decode_key(&buf)?;
            heads.push(HeapHead(
                RunHead {
                    key: decoded_key(key)?,
                    position,
                    cursor: head.cursor,
                },
                Rc::clone(&too_deep),
            ));
        }
        buf.clear();
    }
    if too_deep.get() {
        // A comparison past the depth ceiling during the merge is the same error the in-memory sort raises; the flag
        // was set by a pop/push comparison above. The caller aborts the whole publication, so the positions already
        // collected are never published.
        return Err(comparison_error(resources));
    }
    if positions.len() != total {
        // Every flushed chunk was non-empty, so the runs hold exactly one row per collected key: a shorter merge means
        // a host read silently came up short. Publishing a shortened array would be a wrong answer, not a degraded one.
        return Err(EngineRunError::internal_contract(
            "spill merge produced fewer positions than entries",
        ));
    }
    for &id in runs {
        let _ = store.delete_run(id);
    }
    Ok(positions)
}

/// Merges the runs (each sorted by the same stable law) into the global POSITION sequence the keyed publication
/// rebuilds from.
///
/// On ANY failure every run is deleted (best-effort): the request is aborting, and dead run bytes must not sit on the
/// host store until teardown.
pub fn merge_run_positions(
    store: &dyn SpillStore,
    runs: &[RunId],
    total: usize,
    resources: &ResourceContext<'_>,
) -> Result<Vec<usize>, EngineRunError> {
    let result = merge_run_positions_inner(store, runs, total, resources);
    if result.is_err() {
        for &id in runs {
            let _ = store.delete_run(id);
        }
    }
    result
}

/// Reads every row of every run back as an entry — the tail-flush RECOVERY:
/// when the final flush declines (a non-scalar key sits in the last chunk), the caller folds these rows into its
/// in-memory table and publishes through the ordinary sort, honoring the decline law with zero observable difference.
/// The encoding is exactly round-trippable and the runs were sorted by the same comparator
/// [`super::keyed::sort_entries`] applies, so the re-sorted whole is the order a never-spilled sort would publish.
/// Runs are deleted after the read, as [`merge_run_positions`] deletes them — including on a failed read: any error
/// deletes every run (best-effort) before propagating.
pub fn read_run_entries(store: &dyn SpillStore, runs: &[RunId]) -> Result<Vec<KeyedEntry>, EngineRunError> {
    let result = read_run_entries_inner(store, runs);
    if result.is_err() {
        for &id in runs {
            let _ = store.delete_run(id);
        }
    }
    result
}

fn read_run_entries_inner(store: &dyn SpillStore, runs: &[RunId]) -> Result<Vec<KeyedEntry>, EngineRunError> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    for &id in runs {
        let cursor = store.open_run(id).map_err(host)?;
        while let Some(position) = store.read_next(cursor, &mut buf).map_err(host)? {
            let (_, key) = decode_key(&buf)?;
            let key = decoded_key(key)?;
            out.try_reserve(1).map_err(|_| EngineRunError::allocation_failure())?;
            out.push(KeyedEntry {
                key,
                position: usize::try_from(position).unwrap_or(usize::MAX),
            });
            buf.clear();
        }
        buf.clear();
        let _ = store.delete_run(id);
    }
    Ok(out)
}

/// The stable comparison of two entries: key order, then position.
///
/// [`NanLaw::Total`] for the same reason [`super::keyed::sort_entries`] uses it:
/// the spill reproduces the in-memory sort's law exactly, and the in-memory sort runs on the consistent one.
///
/// Returns [`TooDeep`] when a comparison passes the comparison row's ceiling, exactly as the in-memory
/// [`super::keyed::sort_entries`] raises it — a spilled sort must not quietly order by position what the in-memory
/// path refuses.
fn compare_entries(left: &KeyedEntry, right: &KeyedEntry) -> Result<Ordering, TooDeep> {
    compare_key(&left.key, &right.key, 0, NanLaw::Total)
        .map(|ordering| ordering.then_with(|| left.position.cmp(&right.position)))
}

/// The merge comparison of two run heads: key order, then position. Same law as [`compare_entries`], since it merges
/// what that ordered — the same [`TooDeep`] propagation.
fn compare_heads(left: &RunHead, right: &RunHead) -> Result<Ordering, TooDeep> {
    compare_key(&left.key, &right.key, 0, NanLaw::Total)
        .map(|ordering| ordering.then_with(|| left.position.cmp(&right.position)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;
    use jqf_resource::{
        ContinueControl, RequestAccount, ResourceContext, ResourceError, ResourceLimits, RunCursorId, RunId,
        SpillStore, WorkMeter,
    };

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    #[test]
    fn machine_integers_round_trip_as_fixed_width_i64() {
        for n in [0_i64, -1, 1, i64::MIN, i64::MAX, 42, -999] {
            let key = Value::Number(Number::integer(Integer::from_i64(n)));
            let mut encoded = Vec::new();
            assert!(encode_key(&mut encoded, &KeyValue::Bare(key)).is_some());
            assert_eq!(encoded[0], TAG_I64, "n={n}");
            assert_eq!(encoded.len(), 9, "tag + 8 le bytes");
            let (rest, decoded) = decode_key(&encoded).unwrap();
            assert!(rest.is_empty());
            let Value::Number(number) = decoded.untagged() else {
                panic!("not a number");
            };
            assert_eq!(number.to_i64(), Some(n), "n={n}");
        }
    }

    #[test]
    fn big_integers_keep_the_text_arm() {
        let integer = Integer::parse("999999999999999999999999999999").expect("big");
        let key = Value::Number(Number::try_integer_unaccounted(integer).expect("number"));
        let mut encoded = Vec::new();
        assert!(encode_key(&mut encoded, &KeyValue::Bare(key)).is_some());
        assert_eq!(encoded[0], TAG_INTEGER);
        let (rest, decoded) = decode_key(&encoded).unwrap();
        assert!(rest.is_empty());
        let Value::Number(number) = decoded.untagged() else {
            panic!("not a number");
        };
        assert_eq!(
            number.to_integer().expect("int").as_str(),
            "999999999999999999999999999999"
        );
    }

    #[test]
    fn scalar_round_trips() {
        for (spelling, expected) in [("3", "3"), ("18824", "18824"), ("user-7", "user-7"), ("-0", "-0")] {
            let key = Value::try_string(spelling).unwrap();
            let mut encoded = Vec::new();
            assert!(encode_key(&mut encoded, &KeyValue::Bare(key)).is_some());
            let (rest, decoded) = decode_key(&encoded).unwrap();
            assert!(rest.is_empty());
            assert_eq!(render(&decoded), expected, "spelling {spelling}");
        }
    }

    #[test]
    fn array_key_round_trips() {
        let mut array = jqf_data::Array::try_with_capacity(2).unwrap();
        array.try_push(Value::try_string("18824").unwrap()).unwrap();
        array
            .try_push(Value::Number(Number::integer(Integer::from_i64(7))))
            .unwrap();
        let key = KeyValue::Many(array);
        let mut encoded = Vec::new();
        assert!(encode_key(&mut encoded, &key).is_some());
        let (rest, decoded) = decode_key(&encoded).unwrap();
        assert!(rest.is_empty(), "rest {rest:?}");
        let Value::Array(decoded_array) = decoded else {
            panic!("not an array");
        };
        assert_eq!(decoded_array.len(), 2);
    }

    /// The test renderer: strings as-is, integers as their text.
    fn render(value: &Value) -> String {
        match value.untagged() {
            Value::String(text) => text.as_str().to_string(),
            Value::Number(number) => number.to_i64().map_or_else(|| "number".to_string(), |i| i.to_string()),
            _ => String::new(),
        }
    }

    /// A value nested `levels` array levels deep (the innermost element is `null`). The comparison row's ceiling: a
    /// value nested 10001 levels compares at depths 0..=10000 and succeeds, one nested 10002 levels reaches 10001 and
    /// raises.
    fn nested_value(levels: usize, _resources: &ResourceContext<'_>) -> Value {
        let mut inner = Value::Null;
        for _ in 0..levels {
            let mut array = jqf_data::Array::try_with_capacity(1).unwrap();
            array.try_push(inner).unwrap();
            inner = Value::Array(array);
        }
        inner
    }

    /// A `SpillStore` whose methods PANIC: the too-deep tests assert the depth error fires BEFORE the store is touched,
    /// so a method call here means the guard moved (the vacuity guard of the test).
    struct PanicStore;

    impl SpillStore for PanicStore {
        fn create_run(&self) -> Result<RunId, ResourceError> {
            panic!("store touched on the too-deep path")
        }
        fn write_run(&self, _id: RunId, _bytes: &[u8]) -> Result<(), ResourceError> {
            panic!("store touched on the too-deep path")
        }
        fn open_run(&self, _id: RunId) -> Result<RunCursorId, ResourceError> {
            panic!("store touched on the too-deep path")
        }
        fn read_next(&self, _cursor: RunCursorId, _out: &mut Vec<u8>) -> Result<Option<u64>, ResourceError> {
            panic!("store touched on the too-deep path")
        }
        fn delete_run(&self, _id: RunId) -> Result<(), ResourceError> {
            panic!("store touched on the too-deep path")
        }
    }

    /// A comparison past the 10000-level ceiling is the in-memory sort's error, never a fold to `Equal`: the spill
    /// comparator must propagate [`TooDeep`] so the caller can raise `comparison_error` instead of silently sorting by
    /// input position. Runs on a big-stack thread (the comparison recursion and the drop of the nested value are the
    /// request thread's job, not this test thread's).
    #[test]
    fn too_deep_comparison_is_an_error_not_an_order() {
        extern crate std;
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let resources = resources();
                let key = KeyValue::Bare(nested_value(10002, &resources));
                let left = KeyedEntry { key, position: 0 };
                let right = KeyedEntry {
                    key: KeyValue::Bare(nested_value(10002, &resources)),
                    position: 1,
                };
                assert!(matches!(compare_entries(&left, &right), Err(TooDeep)));
                let left_head = RunHead {
                    key: left.key,
                    position: 0,
                    cursor: RunCursorId(0),
                };
                let right_head = RunHead {
                    key: right.key,
                    position: 1,
                    cursor: RunCursorId(1),
                };
                assert!(matches!(compare_heads(&left_head, &right_head), Err(TooDeep)));
            })
            .expect("spawn")
            .join()
            .expect("big-stack thread");
    }

    /// [`try_flush`] raises the same depth error the in-memory sort raises, BEFORE any run is written (the panic-on-use
    /// store proves the guard moved first). Same big-stack reasoning as the comparator test.
    #[test]
    fn try_flush_raises_on_a_too_deep_comparison() {
        extern crate std;
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(|| {
                let resources = resources();
                let entries = vec![
                    KeyedEntry {
                        key: KeyValue::Bare(nested_value(10002, &resources)),
                        position: 0,
                    },
                    KeyedEntry {
                        key: KeyValue::Bare(nested_value(10002, &resources)),
                        position: 1,
                    },
                ];
                let mut runs = Vec::new();
                let result = try_flush(&PanicStore, &entries, &mut runs, &resources);
                assert!(result.is_err(), "expected the depth error, got {result:?}");
                assert!(runs.is_empty(), "no run may be written on the too-deep path");
            })
            .expect("spawn")
            .join()
            .expect("big-stack thread");
    }

    /// A temporal key declines the spill encoder — it must not reach the match's `unreachable!`. Bytes already
    /// decline on their own arm.
    #[test]
    fn encode_value_declines_extended_kinds_that_have_no_spill_encoding() {
        let date = Value::LocalDate(jqf_data::LocalDate::new(2024, 1, 2).expect("date"));
        let mut out = Vec::new();
        assert!(encode_value(&mut out, &date).is_none());
        assert!(out.is_empty());

        let time = Value::LocalTime(
            jqf_data::LocalTime::new(1, 2, 3, jqf_data::FractionalSecond::parse("").expect("frac")).expect("time"),
        );
        out.clear();
        assert!(encode_value(&mut out, &time).is_none());

        let datetime = Value::LocalDateTime(jqf_data::LocalDateTime {
            date: jqf_data::LocalDate::new(2024, 1, 2).expect("date"),
            time: jqf_data::LocalTime::new(1, 2, 3, jqf_data::FractionalSecond::parse("").expect("frac"))
                .expect("time"),
        });
        out.clear();
        assert!(encode_value(&mut out, &datetime).is_none());

        let offset = Value::OffsetDateTime(jqf_data::OffsetDateTime {
            local: jqf_data::LocalDateTime {
                date: jqf_data::LocalDate::new(2024, 1, 2).expect("date"),
                time: jqf_data::LocalTime::new(1, 2, 3, jqf_data::FractionalSecond::parse("").expect("frac"))
                    .expect("time"),
            },
            offset: jqf_data::UtcOffset::UnknownLocalOffset,
        });
        out.clear();
        assert!(encode_value(&mut out, &offset).is_none());

        let bytes = Value::try_bytes(b"abc").expect("bytes");
        out.clear();
        assert!(encode_value(&mut out, &bytes).is_none());
    }
}
