//! Shared Eytzinger layout for object lookup.
//!
//! Owned objects and borrowed object views both search this way: one comparison per level, children at `2i+1` and
//! `2i+2`.

use core::cmp::Ordering;

/// Fills `output` with Eytzinger-order slots: each slot stores the `sorted_position` of the cursor-th sorted element,
/// so a binary walk visits one slot per level. Fails when a position does not fit `T`.
pub(crate) fn try_fill_eytzinger_by<T>(
    output: &mut [T],
    mut sorted_position: impl FnMut(usize) -> usize,
) -> Result<(), ()>
where
    T: TryFrom<usize>,
{
    let mut cursor = 0;
    try_fill_slots(output, 0, &mut cursor, &mut sorted_position)
}

fn try_fill_slots<T, F>(output: &mut [T], slot: usize, cursor: &mut usize, sorted_position: &mut F) -> Result<(), ()>
where
    T: TryFrom<usize>,
    F: FnMut(usize) -> usize,
{
    if slot >= output.len() {
        return Ok(());
    }
    try_fill_slots(
        output,
        slot.saturating_mul(2).saturating_add(1),
        cursor,
        sorted_position,
    )?;
    output[slot] = T::try_from(sorted_position(*cursor)).map_err(|_| ())?;
    *cursor += 1;
    try_fill_slots(
        output,
        slot.saturating_mul(2).saturating_add(2),
        cursor,
        sorted_position,
    )
}

/// Searches Eytzinger-ordered `positions` with one `compare` per level; returns the first equal position encountered
/// along one descent, or `None`.
///
/// "First" is exact only because callers guarantee unique keys (object winner dedup upstream), so at most one position
/// compares equal; with duplicates the walk returns whichever equal element its single path meets, not the leftmost.
/// The closure returns the ordering of the position's OWN value against the sought value (`Ordering::Less` means the
/// element at this position is smaller than the target, which descends right); callers comparing the other direction
/// would walk to the wrong subtree.
pub(crate) fn find_eytzinger<T, E>(
    positions: &[T],
    mut compare: impl FnMut(T) -> Result<Ordering, E>,
) -> Result<Option<T>, E>
where
    T: Copy,
{
    let mut slot = 0;
    while let Some(&position) = positions.get(slot) {
        match compare(position)? {
            Ordering::Less => slot = slot.saturating_mul(2).saturating_add(2),
            Ordering::Greater => slot = slot.saturating_mul(2).saturating_add(1),
            Ordering::Equal => return Ok(Some(position)),
        }
    }
    Ok(None)
}
