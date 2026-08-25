//! Erased factory, provider, and validator carriers: thin `Box<dyn Trait>` newtypes over the concrete codec
//! implementations.
//!
//! A wrong concrete type does not compile.

use alloc::boxed::Box;
use core::marker::PhantomData;
use jqf_resource::ResourceError;
use jqf_source::ResolvedSource;

use crate::{CodecError, CodecFailureKind};

/// Allocates a box fallibly, keeping an allocation refusal on the error path instead of aborting. The counting
/// allocator can refuse an exact layout, and the allocator-failure-injection harness exercises exactly that refusal
/// through provider/factory/validator construction.
pub(crate) fn fallible_box<T>(value: T) -> Result<Box<T>, CodecError> {
    let layout = alloc::alloc::Layout::new::<T>();
    if layout.size() == 0 {
        // A zero-sized type has no allocation to refuse.
        return Ok(Box::new(value));
    }
    // SAFETY: `layout` is non-zero and valid for `T` (the size check above
    // and `Layout::new`'s alignment guarantee). `alloc` returns null on failure, which is the allocator contract.
    let raw = unsafe { alloc::alloc::alloc(layout) };
    let Some(raw) = core::ptr::NonNull::new(raw) else {
        return Err(CodecError::new(CodecFailureKind::Resource(
            ResourceError::AllocationFailed,
        )));
    };
    // SAFETY: `raw` is non-null, aligned for `T` (alloc's contract), and
    // uniquely owned; the pointer is valid for a write of `T`'s layout.
    unsafe { raw.as_ptr().cast::<T>().write(value) };
    // SAFETY: `raw` now holds a fully initialized `T`; `Box::from_raw` takes
    // ownership and deallocates with the same layout on drop.
    Ok(unsafe { Box::from_raw(raw.as_ptr().cast::<T>()) })
}

/// Source-bound erased provider state returned by decoder factories.
///
/// Like every request-local carrier here, this type is deliberately `!Send`: request accounts, documents, and providers
/// stay on the side that made them, so the marker is structural, not incidental.
pub struct ErasedProvider<'source> {
    pub(crate) source: ResolvedSource<'source>,
    pub(crate) owner: Box<dyn crate::provider::InputProvider>,
    pub(crate) provider_id: u64,
    _not_send: PhantomData<alloc::rc::Rc<()>>,
}

impl core::fmt::Debug for ErasedProvider<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ErasedProvider")
            .field("provider_id", &self.provider_id)
            .finish_non_exhaustive()
    }
}

impl<'source> ErasedProvider<'source> {
    pub(crate) fn try_new_with<T, F>(source: ResolvedSource<'source>, constructor: F) -> Result<Self, CodecError>
    where
        T: crate::provider::InputProvider,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let owner: Box<dyn crate::provider::InputProvider> = fallible_box(constructor()?)?;
        Ok(Self {
            source,
            owner,
            provider_id: crate::provider::fresh_provider_id()?,
            _not_send: PhantomData,
        })
    }

    /// Returns the retained source authority owned by the carrier.
    #[must_use]
    pub const fn source(&self) -> ResolvedSource<'source> {
        self.source
    }
}

/// Target-bound erased encoder factory state.
///
/// Deliberately `!Send` like [`ErasedProvider`] — same request-local law, made structural by the marker.
pub struct ErasedEncoderFactory {
    pub(crate) owner: Box<dyn crate::encode::EncoderFactoryImpl>,
    pub(crate) diagnostics_checked: bool,
    pub(crate) preservation: Option<crate::PreservationRequest>,
    _not_send: PhantomData<alloc::rc::Rc<()>>,
}

impl core::fmt::Debug for ErasedEncoderFactory {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ErasedEncoderFactory")
            .field("diagnostics_checked", &self.diagnostics_checked)
            .field("preservation", &self.preservation.is_some())
            .finish_non_exhaustive()
    }
}

impl ErasedEncoderFactory {
    /// Whether this factory emits the dialect's canonical spelling of a document. See
    /// [`crate::EncoderFactoryImpl::emits_canonical_form`].
    #[must_use]
    pub fn emits_canonical_form(&self) -> bool {
        self.owner.emits_canonical_form()
    }

    pub(crate) fn try_new_with<T, F>(constructor: F) -> Result<Self, CodecError>
    where
        T: crate::encode::EncoderFactoryImpl,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let owner: Box<dyn crate::encode::EncoderFactoryImpl> = fallible_box(constructor()?)?;
        Ok(Self {
            owner,
            diagnostics_checked: false,
            preservation: None,
            _not_send: PhantomData,
        })
    }
}

/// Target-bound erased tag-validator state.
pub struct ErasedTagValidator {
    pub(crate) owner: Box<dyn crate::tag::TagValidator>,
    _not_send: PhantomData<alloc::rc::Rc<()>>,
}

impl core::fmt::Debug for ErasedTagValidator {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("ErasedTagValidator")
    }
}

impl ErasedTagValidator {
    pub(crate) fn try_new_with<T, F>(constructor: F) -> Result<Self, CodecError>
    where
        T: crate::tag::TagValidator,
        F: FnOnce() -> Result<T, CodecError>,
    {
        let owner: Box<dyn crate::tag::TagValidator> = fallible_box(constructor()?)?;
        Ok(Self {
            owner,
            _not_send: PhantomData,
        })
    }
}
