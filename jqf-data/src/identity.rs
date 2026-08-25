//! Shared storage for names: nonempty, no ASCII whitespace or control byte.
//!
//! [`validate`] is the one check. [`IdentityText`] holds the copy. [`IdentityInterner`] deduplicates names inside one
//! document schema.

use alloc::{string::String, vec::Vec};
use core::{fmt, hash::Hash};
use jqf_resource::ResourceError;
use triomphe::Arc;

/// Why a name was refused.
pub(crate) enum IdentityError {
    Empty,
    InvalidCharacter,
}

/// Refuse an empty name or any ASCII control or whitespace byte.
///
/// Every public name constructor goes through here first. `const` so static descriptors can check the same rule at
/// compile time.
pub(crate) const fn validate(value: &str) -> Result<(), IdentityError> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(IdentityError::Empty);
    }
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_control() || bytes[index].is_ascii_whitespace() {
            return Err(IdentityError::InvalidCharacter);
        }
        index += 1;
    }
    Ok(())
}

/// Copy `value` into its own allocation. Fails if the allocator refuses.
pub(crate) fn try_copy_str(value: &str) -> Result<String, ResourceError> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len())?;
    owned.push_str(value);
    Ok(owned)
}

/// Name text in one shared allocation.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct IdentityText(Arc<String>);

impl IdentityText {
    /// Copy `value` into shared storage.
    pub(crate) fn try_new(value: &str) -> Result<Self, ResourceError> {
        Arc::try_new(try_copy_str(value)?)
            .map(Self)
            .map_err(|_| ResourceError::AllocationFailed)
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// The five slots one dynamic-schema append can intern: node kind, occurrence role, fact kind, fact role, tag text.
const BATCH_SLOTS: usize = 5;

/// Ids and texts from one intern batch.
type PreparedBatch = ([Option<IdentityId>; BATCH_SLOTS], [Option<IdentityText>; BATCH_SLOTS]);

/// Deduplicating name table for one document schema.
///
/// Dedup is a linear scan by design: the table holds SCHEMA names (node kinds, occurrence/fact roles, tag texts)
/// bounded by a format's descriptor vocabulary — hundreds of entries, not data-scale millions — so the scan is a
/// few hundred cheap comparisons per append. If a real corpus ever shows schema-name counts where that matters, this is
/// the place for a hash index (`no_std` rules out `std::collections`; it would come from a dependency).
pub(crate) struct IdentityInterner {
    values: Vec<IdentityText>,
}

/// Dense handle into one [`IdentityInterner`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct IdentityId(u32);

impl IdentityId {
    /// Handle for `index`. `None` if it does not fit in `u32`.
    pub(crate) fn from_index(index: usize) -> Option<Self> {
        u32::try_from(index).ok().map(Self)
    }

    pub(crate) const fn index(self) -> usize {
        self.0 as usize
    }
}

impl IdentityInterner {
    /// Empty table.
    pub(crate) const fn new() -> Self {
        Self { values: Vec::new() }
    }

    /// The interned position of `value`, if present. The one lookup every reuse check shares, so a future hash index
    /// lands here.
    fn position_of(&self, value: &str) -> Option<usize> {
        self.values
            .as_slice()
            .iter()
            .position(|existing| existing.as_str() == value)
    }

    /// Clones the canonical text for `value` if it is already interned.
    pub(crate) fn existing_text(&self, value: &str) -> Option<IdentityText> {
        self.position_of(value)
            .map(|index| self.values.as_slice()[index].clone())
    }

    /// Consumes the interner into its identity table (the schema's identity arena).
    pub(crate) fn into_values(self) -> Vec<IdentityText> {
        self.values
    }

    /// Reserves exact additional identity capacity.
    pub(crate) fn try_reserve_exact(&mut self, additional: usize) -> Result<(), ResourceError> {
        self.values.try_reserve_exact(additional)?;
        Ok(())
    }

    /// Interns one append's slots in one pass, deduplicating against interned values and the batch's own earlier slots
    /// alike, and returns the per-slot ids and canonical texts.
    pub(crate) fn try_prepare_batch(
        &mut self,
        values: [Option<&str>; BATCH_SLOTS],
    ) -> Result<PreparedBatch, ResourceError> {
        let mut canonical: [Option<IdentityText>; BATCH_SLOTS] = [const { None }; BATCH_SLOTS];
        let mut ids = [None; BATCH_SLOTS];
        let mut texts: [Option<IdentityText>; BATCH_SLOTS] = [const { None }; BATCH_SLOTS];
        let mut new_count = 0_usize;
        for (index, value) in values.into_iter().enumerate() {
            let Some(value) = value else { continue };
            if let Some(existing_index) = self.position_of(value) {
                ids[index] = IdentityId::from_index(existing_index);
                texts[index] = Some(self.values.as_slice()[existing_index].clone());
                continue;
            }
            if let Some(staged_index) = canonical
                .iter()
                .position(|existing| existing.as_ref().is_some_and(|item| item.as_str() == value))
            {
                ids[index] = IdentityId::from_index(self.values.len() + staged_index);
                texts[index] = Some(
                    canonical[staged_index]
                        .as_ref()
                        .expect("located staged identity")
                        .clone(),
                );
                continue;
            }
            let new = IdentityText::try_new(value)?;
            let text = new.clone();
            ids[index] = IdentityId::from_index(self.values.len() + new_count);
            texts[index] = Some(text);
            canonical[new_count] = Some(new);
            new_count += 1;
        }
        self.values.extend(canonical.into_iter().take(new_count).flatten());
        Ok((ids, texts))
    }

    /// Interns one identity, reusing the interned text when present, and returns its dense id with the canonical text.
    pub(crate) fn try_prepare_one(&mut self, value: &str) -> Result<(IdentityId, IdentityText), ResourceError> {
        if let Some(index) = self.position_of(value) {
            let id = IdentityId::from_index(index).ok_or(ResourceError::ArithmeticOverflow)?;
            return Ok((id, self.values.as_slice()[index].clone()));
        }
        let id = IdentityId::from_index(self.values.len()).ok_or(ResourceError::ArithmeticOverflow)?;
        let new = IdentityText::try_new(value)?;
        self.values.push(new.clone());
        Ok((id, new))
    }
}

impl fmt::Debug for IdentityText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}
