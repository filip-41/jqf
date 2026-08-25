//! Owned arrays: a shared element list.
//!
//! Clone is a refcount bump. A later write copies first if another handle still sees the same list.

use alloc::vec::Vec;
use core::{slice, slice::SliceIndex};

use super::shared::Shared;
use super::{Value, ValueAllocationError};

/// Ordered list of values.
///
/// The elements live in one allocation. Clone is a refcount bump.
#[derive(Clone, Debug)]
pub struct Array(Shared<Vec<Value>>);

impl Array {
    /// Empty array. Fails if the allocator refuses.
    pub fn try_new() -> Result<Self, ValueAllocationError> {
        Self::try_from_vec(Vec::new())
    }

    /// Take ownership of `values` as a shared array.
    pub fn try_from_vec(values: Vec<Value>) -> Result<Self, ValueAllocationError> {
        Shared::try_new(values).map(Self)
    }

    /// Empty array with room for `capacity` elements.
    pub fn try_with_capacity(capacity: usize) -> Result<Self, ValueAllocationError> {
        let mut values = Vec::new();
        values.try_reserve_exact(capacity).map_err(|_| ValueAllocationError)?;
        Shared::try_new(values).map(Self)
    }

    /// Append `value`. Copies first if another handle still sees this array.
    pub fn try_push(&mut self, value: Value) -> Result<(), ValueAllocationError> {
        let values = self.try_values_mut(1)?;
        values.push(value);
        Ok(())
    }

    /// Append every element of `source` in one growth.
    pub fn try_extend_from(&mut self, source: &Array) -> Result<(), ValueAllocationError> {
        if source.is_empty() {
            return Ok(());
        }
        let values = self.try_values_mut(source.len())?;
        for value in source {
            values.push(value.clone());
        }
        Ok(())
    }

    /// Append `additional` nulls in one growth.
    pub fn try_extend_null(&mut self, additional: usize) -> Result<(), ValueAllocationError> {
        if additional == 0 {
            return Ok(());
        }
        let values = self.try_values_mut(additional)?;
        // `try_values_mut` reserved `additional` on both its branches, so the fill below cannot reallocate and cannot
        // abort on a failed allocation.
        let filled = values.len().checked_add(additional).ok_or(ValueAllocationError)?;
        values.resize_with(filled, || Value::Null);
        Ok(())
    }

    /// How many elements.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when there are no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Element or slice at `index`.
    #[must_use]
    pub fn get<I>(&self, index: I) -> Option<&I::Output>
    where
        I: SliceIndex<[Value]>,
    {
        self.0.get(index)
    }

    /// Mutable access to the element at `index`.
    ///
    /// Copies first if another handle still sees this array. Out of range returns `None` without copying.
    pub fn try_get_mut(&mut self, index: usize) -> Result<Option<&mut Value>, ValueAllocationError> {
        if index >= self.len() {
            return Ok(None);
        }
        Ok(self.try_values_mut(0)?.get_mut(index))
    }

    /// Elements in order.
    pub fn iter(&self) -> slice::Iter<'_, Value> {
        self.0.iter()
    }

    /// Another handle on the same element list.
    #[must_use]
    pub fn clone_shared(&self) -> Self {
        Self(self.0.clone_shared())
    }
}

impl Array {
    /// An allocation-identity key for this element list.
    ///
    /// Two LIVE handles with equal keys alias one element list — exactly what [`Self::shares_storage_with`] answers
    /// between them. Same stale-key contract as [`super::object::Object::allocation_key`], which states it in full.
    #[must_use]
    pub fn allocation_key(&self) -> usize {
        self.0.allocation_key()
    }

    /// Whether these two handles name the same element list.
    #[must_use]
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        self.0.shares_allocation_with(&other.0)
    }

    /// Take the elements as a `Vec`.
    ///
    /// The last handle hands the vector over. If another handle survives, this copies the list (elements stay shared).
    /// Wrap it again with [`Self::try_from_vec`] if you need an array.
    pub fn try_into_vec(self) -> Result<Vec<Value>, ValueAllocationError> {
        match self.0.try_into_payload() {
            Ok(values) => Ok(values),
            Err(shared) => {
                let mut detached = Vec::new();
                detached
                    .try_reserve_exact(shared.len())
                    .map_err(|_| ValueAllocationError)?;
                for value in shared.iter() {
                    detached.push(value.clone());
                }
                Ok(detached)
            }
        }
    }

    fn try_values_mut(&mut self, additional: usize) -> Result<&mut Vec<Value>, ValueAllocationError> {
        if self.0.is_unique() {
            let values = self.0.try_grow_entries(additional)?;
            return Ok(values);
        }
        // A shared spine detaches onto a fresh allocation whose real bytes the ambient allocator charges as they land,
        // so no entry is written uncharged.
        let capacity = self.0.len().checked_add(additional).ok_or(ValueAllocationError)?;
        let mut values = Vec::new();
        values.try_reserve_exact(capacity).map_err(|_| ValueAllocationError)?;
        for value in self.0.iter() {
            values.push(value.clone());
        }
        *self = Self(Shared::try_new(values)?);
        self.0.payload_mut().ok_or(ValueAllocationError)
    }
}

impl<'a> IntoIterator for &'a Array {
    type Item = &'a Value;
    type IntoIter = slice::Iter<'a, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Turn a signed index into a position in a container of `len` items.
///
/// A negative index counts from the end (`-1` is the last item). Out of range, including a negative past the start, is
/// `None`. Pass the length you already have; you do not need an [`Array`].
#[must_use]
pub fn resolve_index(len: usize, index: i64) -> Option<usize> {
    let len = u64::try_from(len).ok()?;
    let position = if index >= 0 {
        u64::try_from(index).ok()?
    } else {
        len.checked_sub(index.unsigned_abs())?
    };
    if position < len {
        usize::try_from(position).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::Array;
    use crate::Value;
    use alloc::vec::Vec;

    fn array_of(items: &[&str]) -> Array {
        let mut values = Vec::new();
        for item in items {
            values.push(Value::try_string(item).expect("element"));
        }
        Array::try_from_vec(values).expect("array fixture")
    }

    /// Out of range returns `None` without copying. An in-range write through a shared handle does copy.
    #[test]
    fn out_of_range_get_mut_answers_none_without_detaching() {
        let mut array = array_of(&["a", "b"]);
        let twin = array.clone_shared();
        assert!(array.try_get_mut(5).expect("probe").is_none());
        assert!(
            array.shares_storage_with(&twin),
            "an out-of-range probe must not detach the shared spine"
        );
        assert!(array.try_get_mut(0).expect("probe").is_some());
        assert!(!array.shares_storage_with(&twin), "an in-range write must detach");
    }

    /// `try_extend_null` appends nulls in one logical growth: the count lands in one pass, and a zero count is a no-op.
    #[test]
    fn extend_null_appends_and_accepts_zero() {
        let mut array = Array::try_new().expect("empty array");
        array.try_extend_null(3).expect("extend");
        assert_eq!(array.len(), 3);
        assert!(matches!(array.get(2), Some(Value::Null)));
        array.try_extend_null(0).expect("zero extend");
        assert_eq!(array.len(), 3);
    }

    /// Equal allocation keys alias one element list, and independently built lists never share a key — the O(1)
    /// witness behind `shares_storage_with`.
    #[test]
    fn allocation_keys_agree_exactly_with_storage_sharing() {
        let array = array_of(&["a"]);
        let twin = array.clone_shared();
        assert!(array.shares_storage_with(&twin));
        assert_eq!(array.allocation_key(), twin.allocation_key());

        let other = array_of(&["a"]);
        assert!(!array.shares_storage_with(&other));
        assert_ne!(array.allocation_key(), other.allocation_key());
    }

    /// `try_into_vec`: the last handle hands its vector over directly; a surviving twin forces a one-level detach whose
    /// elements STAY SHARED with the source's.
    #[test]
    fn into_vec_unique_and_twin_arms() {
        let array = array_of(&["a", "b"]);
        let values = array.try_into_vec().expect("unique arm");
        assert_eq!(values.len(), 2);

        let array = array_of(&["a", "b"]);
        let twin = array.clone_shared();
        let values = array.try_into_vec().expect("twin arm");
        let Value::String(detached) = &values[0] else {
            panic!("element is a string");
        };
        let Value::String(source) = twin.get(0).expect("element exists") else {
            panic!("source element is a string");
        };
        assert!(
            detached.shares_allocation_with(source),
            "the twin-arm detach must retain the source element allocations"
        );
    }

    /// The signed-index law at its edges: `-1` is the last item, a negative past the start is out of range, and
    /// `i64::MIN` resolves through its UNSIGNED magnitude — a negation-based refactor (`-index`) would overflow on
    /// exactly that one input.
    #[test]
    fn resolve_index_counts_from_the_end_without_negation_overflow() {
        assert_eq!(super::resolve_index(3, 0), Some(0));
        assert_eq!(super::resolve_index(3, 2), Some(2));
        assert_eq!(super::resolve_index(3, 3), None);
        assert_eq!(super::resolve_index(3, -1), Some(2));
        assert_eq!(super::resolve_index(3, -3), Some(0));
        assert_eq!(super::resolve_index(3, -4), None);
        assert_eq!(super::resolve_index(3, i64::MIN), None);
        assert_eq!(super::resolve_index(usize::MAX, -1), Some(usize::MAX - 1));
    }
}
