//! Per-document authoring scratch, recycled across a record stream.
//!
//! A record-stream decoder builds ONE document per record: every vector here starts empty, grows to that record's
//! shape, and dies with the document, so a stream of N records pays N regrow chains for the same few shapes. Parking
//! the spare capacity on the decoder's session and swapping it back in turns those N chains into one.
//!
//! This carries CAPACITY ONLY. Every vector is handed back EMPTY (`clear`, never a stale length), so no record can
//! observe another record's data, and the counter tables still zero-fill from length 0 exactly as a fresh document's
//! do.
//!
//! Retention is bounded by [`TRANSIENT_REUSE_CAP`]. The releases these vectors replace are deliberate peak-RSS steps
//! — the per-role position tables and the adjacency scratch are dropped before the relationship-arena pass so they
//! never sit under the finalizer's peak — so a one-off large document drops its scratch on the floor exactly as it
//! does today, and only record-shaped capacity survives to the next document.

use alloc::vec::Vec;

use super::handle::{FactId, OccurrenceId};
use super::storage::{NodeRecord, OccurrenceRecord, StorageRange};

/// The largest scratch vector worth carrying to the next document, in elements. Record-shaped builds sit far below it;
/// a one-off large document releases its scratch instead of holding it under the next build.
pub(super) const TRANSIENT_REUSE_CAP: usize = 1024;

/// Clears one scratch vector for reuse, releasing it outright when it grew past the retention cap.
pub(super) fn recycle<T>(vector: &mut Vec<T>) {
    vector.clear();
    if vector.capacity() > TRANSIENT_REUSE_CAP {
        *vector = Vec::new();
    }
}

/// Moves one finished scratch vector into its slot in the bag, leaving the live side empty.
///
/// Keeps whichever buffer is larger, which makes the call idempotent: a vector publication already parked mid-poll is
/// not swapped back out by the sweep at the end.
pub(super) fn park<T>(slot: &mut Vec<T>, live: &mut Vec<T>) {
    recycle(live);
    if live.capacity() > slot.capacity() {
        core::mem::swap(slot, live);
    }
}

/// Spare capacity for the vectors one document build fills and drops.
///
/// Opaque by design: every field is a `jqf-data`-internal record type, so a codec parks the whole bag on its session
/// beside its own frame workspace without naming what is inside it.
pub struct DocumentTransients {
    /// Per-role occurrence-position counters (`[role][node]`). The OUTER length survives with every inner table
    /// cleared: the role loop then pushes nothing and each inner table re-zero-fills from length 0, which is where this
    /// bag's allocation traffic actually lives.
    pub(super) owner_positions: Vec<Vec<u32>>,
    pub(super) staged_nodes: Vec<NodeRecord>,
    pub(super) staged_occurrences: Vec<OccurrenceRecord>,
    pub(super) counts: Vec<usize>,
    pub(super) owner_occurrences: Vec<OccurrenceId>,
    pub(super) fact_owner_nodes: Vec<u32>,
    pub(super) fact_owner_ranges: Vec<StorageRange>,
    pub(super) fact_owner_ids: Vec<FactId>,
}

impl Default for DocumentTransients {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentTransients {
    /// A bag holding nothing: the first document allocates exactly as it does without recycling.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            owner_positions: Vec::new(),
            staged_nodes: Vec::new(),
            staged_occurrences: Vec::new(),
            counts: Vec::new(),
            owner_occurrences: Vec::new(),
            fact_owner_nodes: Vec::new(),
            fact_owner_ranges: Vec::new(),
            fact_owner_ids: Vec::new(),
        }
    }

    /// Parks one per-role position table, keeping the outer table's inner allocations alive with every counter cleared.
    pub(super) fn park_owner_positions(&mut self, table: &mut Vec<Vec<u32>>) {
        if table.capacity() > TRANSIENT_REUSE_CAP {
            *table = Vec::new();
            return;
        }
        for inner in table.iter_mut() {
            recycle(inner);
        }
        core::mem::swap(&mut self.owner_positions, table);
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn recycling_keeps_capacity_but_never_data() {
        let mut vector: Vec<u32> = Vec::with_capacity(16);
        vector.extend([1, 2, 3]);
        recycle(&mut vector);
        assert!(vector.is_empty(), "a recycled vector carries no data");
        assert!(vector.capacity() >= 16, "record-shaped capacity survives");
    }

    #[test]
    fn recycling_releases_a_vector_past_the_cap() {
        let mut vector: Vec<u32> = Vec::with_capacity(TRANSIENT_REUSE_CAP + 1);
        recycle(&mut vector);
        assert_eq!(
            vector.capacity(),
            0,
            "a one-off large scratch is released, not held under the next build"
        );
    }

    #[test]
    fn parked_position_tables_keep_inner_capacity_and_zero_counters() {
        let mut bag = DocumentTransients::new();
        let mut table = vec![Vec::with_capacity(32), Vec::with_capacity(32)];
        table[0].extend([7_u32, 9]);
        bag.park_owner_positions(&mut table);
        assert_eq!(bag.owner_positions.len(), 2, "the outer table survives");
        assert!(
            bag.owner_positions.iter().all(alloc::vec::Vec::is_empty),
            "every counter table is cleared so the next document re-zero-fills"
        );
        assert!(
            bag.owner_positions.iter().all(|inner| inner.capacity() >= 32),
            "inner capacity is what this bag exists to keep"
        );
    }
}
