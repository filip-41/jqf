//! Shared heap payloads for owned values.
//!
//! Every heap-backed `Value` variant allocates here. Clone is a refcount bump. A later write copies first if another
//! handle still sees the payload. Text can reserve unused tail bytes so appends stay cheap; readers only see the used
//! prefix.
//!
//! This module does not know what a `Value` is. Children hold their own allocations.

use alloc::vec::Vec;
use core::mem::MaybeUninit;
use core::{fmt, ops::Deref, ptr};

use triomphe::{Arc, HeaderSlice, UniqueArc};

use super::ValueAllocationError;

/// What one shared allocation knows about itself.
///
/// `spare` is tail bytes reserved ahead of use so that a growing text payload does not re-copy its prefix per append.
/// Every payload that never grows leaves `spare` at zero, which is the truth for it — its tail is entirely in use.
struct Allocation {
    spare: u32,
}

/// One shared payload.
///
/// Sized values and unsized `str` / `[u8]` tails use the same type.
pub struct Shared<T: ?Sized>(Arc<HeaderSlice<Allocation, T>>);

impl<T: ?Sized> Shared<T> {
    /// Tail bytes this allocation reserved but does not hold.
    fn spare(&self) -> usize {
        self.0.header.spare as usize
    }

    /// Another handle on the same allocation. Copies nothing.
    #[must_use]
    pub fn clone_shared(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: ?Sized> Shared<T> {
    /// Whether this is the only handle on the allocation.
    ///
    /// `true` means a write can happen in place. `false` means copy first.
    #[must_use]
    pub(crate) fn is_unique(&self) -> bool {
        self.0.is_unique()
    }

    /// Whether these two handles name the same allocation.
    #[must_use]
    pub(crate) fn shares_allocation_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// An allocation-identity key: the shared allocation's own address.
    ///
    /// Two LIVE handles with equal keys name one allocation. The stale-key rules are stated in full at
    /// [`super::Object::allocation_key`], the public entry both containers delegate from.
    #[must_use]
    pub(crate) fn allocation_key(&self) -> usize {
        Arc::as_ptr(&self.0).cast::<u8>() as usize
    }

    /// Borrows the bookkeeping and payload of a UNIQUE allocation.
    fn parts_mut(&mut self) -> Option<(&mut Allocation, &mut T)> {
        Arc::get_mut(&mut self.0).map(|inner| (&mut inner.header, &mut inner.slice))
    }

    /// Mutable payload if this is the only handle. `None` if another handle still exists — copy first.
    pub(crate) fn payload_mut(&mut self) -> Option<&mut T> {
        self.parts_mut().map(|(_, payload)| payload)
    }

    /// Borrows the payload of an allocation the caller has ALREADY gated as unique (its own `is_unique()` check, run
    /// once). Fails closed with [`ValueAllocationError`] if another handle still exists.
    pub(crate) fn borrow_owned(&mut self) -> Result<&mut T, ValueAllocationError> {
        self.parts_mut().map(|(_, payload)| payload).ok_or(ValueAllocationError)
    }
}

impl<T> Shared<T> {
    /// Allocate one shared payload. Fails if the allocator refuses.
    pub fn try_new(payload: T) -> Result<Self, ValueAllocationError> {
        Arc::try_new(HeaderSlice {
            header: Allocation { spare: 0 },
            slice: payload,
        })
        .map(Self)
        .map_err(|_| ValueAllocationError)
    }

    /// Take the payload if this is the last handle. Otherwise give the handle back.
    pub(crate) fn try_into_payload(self) -> Result<T, Self>
    where
        T: Sized,
    {
        match Arc::try_unwrap(self.0) {
            Ok(inner) => Ok(inner.slice),
            Err(inner) => Err(Self(inner)),
        }
    }
}

impl<T> Shared<Vec<T>> {
    /// Grows a UNIQUE element spine to hold `additional` more elements.
    ///
    /// The caller should have gated the allocation as unique (the array's `try_values_mut` runs that gate once); this
    /// method still re-checks via `Arc::get_mut` and fails closed with [`ValueAllocationError`] rather than write
    /// through a shared spine. The charge for the grown slots is the ambient allocator's: the spine reallocation pays
    /// for its real bytes when they land.
    pub(crate) fn try_grow_entries(&mut self, additional: usize) -> Result<&mut Vec<T>, ValueAllocationError> {
        if additional == 0 {
            return self.parts_mut().ok_or(ValueAllocationError).map(|(_, v)| v);
        }
        let (_, values) = self.parts_mut().ok_or(ValueAllocationError)?;
        values.try_reserve(additional).map_err(|_| ValueAllocationError)?;
        Ok(values)
    }
}

impl Shared<str> {
    /// Shared text. Fails if the allocator refuses.
    pub fn try_from_str(text: &str) -> Result<Self, ValueAllocationError> {
        Self::try_from_reserved_str(text, 0)
    }

    /// Allocates a text payload whose last `spare` tail bytes are reserved but NOT in use.
    ///
    /// The reserved bytes must already be NUL in `text`, which is what keeps the whole tail valid UTF-8 while only its
    /// prefix is readable.
    fn try_from_reserved_str(text: &str, spare: usize) -> Result<Self, ValueAllocationError> {
        // The caller's contract — the reserved tail is already NUL — is what keeps `as_str`'s prefix split on a
        // character boundary; assert it rather than trust it.
        debug_assert!(
            spare <= text.len() && text.as_bytes()[text.len() - spare..].iter().all(|byte| *byte == 0),
            "a reserved tail must be NUL bytes of the text itself"
        );
        let header = Allocation {
            spare: u32::try_from(spare).map_err(|_| ValueAllocationError)?,
        };
        Arc::try_from_header_and_str(header, text)
            .map(Self)
            .map_err(|_| ValueAllocationError)
    }

    /// The used prefix as text. Unused reserved tail bytes stay hidden.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The reserved tail is whole NUL bytes and the used prefix is whole UTF-8, so the split is always on a
        // character boundary. A zero spare (every finished string) skips the subtract.
        let spare = self.spare();
        if spare == 0 {
            &self.0.slice
        } else {
            &self.0.slice[..self.0.slice.len() - spare]
        }
    }

    /// Append `suffix`, growing in place when this is the only handle.
    ///
    /// If another handle still sees this allocation, this copies first. That includes the case where `suffix` borrows
    /// this same payload.
    ///
    /// # Errors
    ///
    /// Returns [`ValueAllocationError`] when the allocation fails.
    pub fn try_extend(&mut self, suffix: &str) -> Result<(), ValueAllocationError> {
        if suffix.is_empty() {
            return Ok(());
        }
        let used = self.as_str().len();
        let spare = self.spare();
        if spare >= suffix.len() && self.is_unique() {
            {
                let (header, payload) = self.parts_mut().ok_or(ValueAllocationError)?;
                header.spare = u32::try_from(spare - suffix.len()).map_err(|_| ValueAllocationError)?;
                // SAFETY: the bytes written are `suffix`'s own, complete UTF-8,
                // and they land exactly on `suffix.len()` of the reserved NUL
                // bytes that follow the used prefix — never inside a character.
                // The tail is therefore whole UTF-8 again when the borrow ends,
                // and it was whole UTF-8 throughout (a NUL is a character).
                let tail = unsafe { payload.as_bytes_mut() };
                tail[used..used + suffix.len()].copy_from_slice(suffix.as_bytes());
                return Ok(());
            }
        }
        self.try_reallocate_extended(suffix)
    }

    /// Grows onto a FRESH allocation, reserving ahead so the next appends land in place.
    fn try_reallocate_extended(&mut self, suffix: &str) -> Result<(), ValueAllocationError> {
        let used = self.as_str().len();
        let needed = used.checked_add(suffix.len()).ok_or(ValueAllocationError)?;
        let capacity = reserved_capacity(used, needed);
        *self = try_joined_shared(self.as_str(), suffix, capacity - needed)?;
        Ok(())
    }
}

/// Allocates one shared payload holding `prefix ++ suffix` and `spare` NUL tail bytes. The used prefix is the
/// concatenation; the spare law is unchanged (NUL tail + `header.spare`).
///
/// # Safety invariant
///
/// Written bytes are complete UTF-8 (`prefix` then `suffix`), they land on the reserved tail of this new allocation,
/// NULs are valid UTF-8 characters, and the used prefix ends on a character boundary. That is the same invariant
/// [`Shared::try_extend`]'s in-place arm already keeps.
fn try_joined_shared(prefix: &str, suffix: &str, spare: usize) -> Result<Shared<str>, ValueAllocationError> {
    let needed = prefix.len() + suffix.len();
    let capacity = needed + spare;
    let header = Allocation {
        spare: u32::try_from(spare).map_err(|_| ValueAllocationError)?,
    };
    let mut unique: UniqueArc<HeaderSlice<Allocation, [MaybeUninit<u8>]>> =
        UniqueArc::try_from_header_and_uninit_slice(header, capacity).map_err(|_| ValueAllocationError)?;
    // SAFETY: every byte of the uninit slice is written before `assume_init`:
    // prefix, then suffix (each already UTF-8), then `spare` NUL bytes (each
    // a valid UTF-8 character). The used prefix therefore ends on a
    // character boundary, matching the in-place `try_extend` arm.
    unsafe {
        let dst = unique.slice.as_mut_ptr().cast::<u8>();
        ptr::copy_nonoverlapping(prefix.as_ptr(), dst, prefix.len());
        ptr::copy_nonoverlapping(suffix.as_ptr(), dst.add(prefix.len()), suffix.len());
        if spare > 0 {
            ptr::write_bytes(dst.add(needed), 0, spare);
        }
    }
    // SAFETY: the slice is fully initialized by the writes above.
    let unique = unsafe { unique.assume_init_slice_with_header() };
    let bytes: Arc<HeaderSlice<Allocation, [u8]>> = unique.shareable();
    // SAFETY: `HeaderSlice` is `repr(C)`, `str` and `[u8]` share a fat-pointer
    // layout (the same transmute `Arc::try_from_header_and_str` uses), and the
    // bytes are valid UTF-8 by the writes above.
    let text = unsafe { Arc::from_raw(Arc::into_raw(bytes) as *const HeaderSlice<Allocation, str>) };
    Ok(Shared(text))
}

/// How large an allocation one growth step takes to hold `needed` bytes.
///
/// Doubling is what makes a fold linear: each growth at least doubles, so a fold to N bytes takes O(log N) growths that
/// copy O(N) bytes in total, instead of one copy of the whole prefix per append.
///
/// A concatenation that is NOT an accumulation reserves nothing: doubling only exceeds `needed` when the suffix is
/// shorter than the prefix it lands on, which is the accumulator shape and not the shape of `a + b`.
fn reserved_capacity(used: usize, needed: usize) -> usize {
    let doubled = used.saturating_mul(2).max(needed);
    // `spare` is a `u32`, so a payload past that reserve simply stops reserving.
    doubled.min(needed.saturating_add(u32::MAX as usize))
}

impl Shared<[u8]> {
    /// Allocates one shared byte payload.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, ValueAllocationError> {
        let header = Allocation { spare: 0 };
        Arc::try_from_header_and_slice(header, bytes)
            .map(Self)
            .map_err(|_| ValueAllocationError)
    }

    /// Returns the payload as bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        let spare = self.spare();
        if spare == 0 {
            &self.0.slice
        } else {
            &self.0.slice[..self.0.slice.len() - spare]
        }
    }
}

impl<T: ?Sized> Clone for Shared<T> {
    fn clone(&self) -> Self {
        self.clone_shared()
    }
}

// The three `Deref` and `Debug` pairs below exist instead of one blanket pair because a tail payload may RESERVE bytes
// it does not hold, and a blanket impl over `self.0.slice` would hand the reserved NUL bytes to every reader that
// reaches the payload through deref coercion rather than through the named accessor. A sized payload reserves nothing
// and has no such split, so the three impls do not overlap and each one names the whole truth for its payload.
impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0.slice
    }
}

impl Deref for Shared<str> {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for Shared<[u8]> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl<T: fmt::Debug> fmt::Debug for Shared<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0.slice, formatter)
    }
}

impl fmt::Debug for Shared<str> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl fmt::Debug for Shared<[u8]> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_slice(), formatter)
    }
}

impl PartialEq<str> for Shared<str> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Shared<str> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::Shared;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    /// One request context; the ambient allocator does the charging now, so a fixture needs only a context to pass
    /// along the (retained) parameter.
    fn unlimited_resources() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test resource context")
    }

    /// The mutation gate is uniqueness alone now: a payload another handle can see must refuse the borrow, while a
    /// unique one grants it.
    #[test]
    fn a_shared_payload_refuses_mutation() {
        let _resources = unlimited_resources();
        let mut text = Shared::try_from_str("text").expect("string");
        let twin = text.clone_shared();
        assert!(
            text.payload_mut().is_none(),
            "a shared allocation must not be written through"
        );
        drop(twin);
        assert!(
            text.payload_mut().is_some(),
            "the unique allocation borrows the payload"
        );
    }

    /// The allocation key agrees exactly with `shares_allocation_with`: equal keys on live handles name one allocation,
    /// and two independent allocations never share a key.
    #[test]
    fn an_allocation_key_agrees_with_sharing() {
        let _resources = unlimited_resources();
        let text = Shared::try_from_str("text").expect("string");
        let twin = text.clone_shared();
        assert!(text.shares_allocation_with(&twin));
        assert_eq!(text.allocation_key(), twin.allocation_key());

        let other = Shared::try_from_str("text").expect("string");
        assert!(!text.shares_allocation_with(&other));
        assert_ne!(text.allocation_key(), other.allocation_key());
    }

    /// The growth law's in-place arm: a growth that lands in the reserved tail KEEPS the allocation, while a growth
    /// past it reallocates. The pointer witness is the only observable — the text result is identical either way,
    /// which is the whole point of the amortized reserve.
    #[test]
    fn a_growth_that_lands_in_spare_keeps_the_allocation() {
        let _resources = unlimited_resources();
        let mut text = Shared::try_from_str("ab").expect("string");
        assert_eq!(text.spare(), 0);
        text.try_extend("c").expect("growth reallocates");
        assert_eq!(text.spare(), 1, "doubling reserves one tail byte");
        let pointer = text.0.as_ptr();
        text.try_extend("d").expect("growth lands in spare");
        assert_eq!(text.spare(), 0);
        assert_eq!(text.0.as_ptr(), pointer, "a spare-capacity append must write in place");
        assert_eq!(text.as_str(), "abcd");
    }

    /// A growth past spare writes prefix+suffix+NULs into one Shared payload without re-validating the reserved tail as
    /// UTF-8.
    #[test]
    fn a_growth_reallocation_keeps_utf8() {
        let _resources = unlimited_resources();
        let mut text = Shared::try_from_str("ab").expect("string");
        text.try_extend("é").expect("utf-8 growth");
        assert_eq!(text.as_str(), "abé");
        text.try_extend("cd").expect("second growth");
        assert_eq!(text.as_str(), "abécd");
    }
}
