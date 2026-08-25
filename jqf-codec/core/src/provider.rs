//! The downstream provider contract: the source-backed input a concrete codec implementation sees during open, and its
//! route/slot vocabulary.

use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::{AccessRequirement, CodecError, CodecFailureKind, ErasedAccessSession, RouteDescription, RouteSlot};

static NEXT_PROVIDER_ID: AtomicU64 = AtomicU64::new(1);
pub(crate) fn fresh_provider_id() -> Result<u64, CodecError> {
    NEXT_PROVIDER_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| value.checked_add(1))
        .map_err(|_| CodecError::new(CodecFailureKind::Overflow))
}

/// Source-backed input visible to a concrete provider only during open.
#[derive(Clone, Copy)]
pub struct ProviderInput<'source> {
    source: ResolvedSource<'source>,
}

impl<'source> ProviderInput<'source> {
    /// Binds one open call to a retained source without reading its bytes.
    pub(crate) const fn new(source: ResolvedSource<'source>) -> Self {
        Self { source }
    }

    /// Returns retained source authority without reading its bytes.
    #[must_use]
    pub const fn source(self) -> ResolvedSource<'source> {
        self.source
    }
}

/// Downstream source-bound provider implementation.
pub trait InputProvider: Any {
    /// The provider's fixed route table, declared from static knowledge alone — reading source bytes is open's job,
    /// never this call's.
    ///
    /// Row order is the binding tie-break: when several bundles could serve one requirement, the EARLIEST matching row
    /// wins, so a codec with a preferred route must list it first.
    fn route_descriptions(&self) -> &[RouteDescription];
    /// Whether the format AUTHORITATIVELY lacks expanded-name markup attributes, so an attribute demand is answerable
    /// as a known absence.
    fn supports_attribute_absence(&self) -> bool {
        false
    }
    /// Opens exactly the core-selected provider-local slot.
    fn open_route<'source>(
        &mut self,
        input: ProviderInput<'source>,
        slot: RouteSlot,
        requirement: &AccessRequirement,
        resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedAccessSession<'source>, CodecError>;

    /// Reinitializes an already-constructed session of this provider's own concrete type for one more adjacent value on
    /// the same sealed slot.
    ///
    /// This is the adjacent-value reuse hook behind [`crate::ErasedProvider::open_at_reusing`]: instead of constructing
    /// a fresh session per value, the provider resets the recycled one and keeps its retained workspaces (step vectors,
    /// frame stacks, scratch). The reset must leave exactly the state a fresh [`Self::open_route`] would have produced
    /// for `requirement`, so a value that failed cannot poison the next one.
    ///
    /// Returning `Ok(false)` means "cannot recycle this state for this requirement"; the caller then drops it and
    /// constructs a fresh session, so declining is always safe. The default declines, leaving every existing provider's
    /// lifecycle untouched.
    ///
    /// # Errors
    ///
    /// Returns a codec error only when the reset itself fails (for example an accounting failure while re-preparing a
    /// retained workspace).
    fn try_reopen_route(
        &mut self,
        _state: &mut crate::RecycledSessionState<'_>,
        _slot: RouteSlot,
        _requirement: &AccessRequirement,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        Ok(false)
    }
}
