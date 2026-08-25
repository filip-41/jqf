//! Ordered selection-emission schedules: complete document or one origin.
//!
//! Sibling: [`crate::pattern`].

use core::fmt;

use jqf_resource::{ResourceContext, ResourceError};

use crate::pattern::AccessFootprint;

/// Engine-authored identity for one scheduled query emission.
///
/// A single ordinal: every production requirement schedules one selection at origin 0. A wider identity returns only
/// when a multi-selection requirement vertical gives ordinals distinct meanings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectionOrigin {
    ordinal: u64,
}

impl SelectionOrigin {
    /// Creates one exact authored origin identity.
    #[must_use]
    pub const fn new(ordinal: u64) -> Self {
        Self { ordinal }
    }
}

/// Failure while constructing or validating a schedule against a footprint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionScheduleError {
    /// One retained object belongs to a different request account.
    Resource(ResourceError),
    /// The complete-document schedule must bind the `Whole` footprint.
    CompleteScheduleRequiresWhole,
    /// A singleton exact schedule must bind an exact footprint terminal.
    SingletonScheduleRequiresExact,
}

impl From<ResourceError> for SelectionScheduleError {
    fn from(value: ResourceError) -> Self {
        Self::Resource(value)
    }
}

impl fmt::Display for SelectionScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resource(error) => error.fmt(formatter),
            Self::CompleteScheduleRequiresWhole => {
                formatter.write_str("an empty complete schedule requires the Whole footprint")
            }
            Self::SingletonScheduleRequiresExact => {
                formatter.write_str("a singleton schedule requires an Exact footprint terminal")
            }
        }
    }
}

impl core::error::Error for SelectionScheduleError {}

/// Opaque ordered emission schedule.
///
/// This surface exposes exactly two forms: an empty complete-document schedule and one singleton entry targeting the
/// sole terminal of an exact footprint. Future multiplicity, traversal, and dynamic runtime scheduling forms remain
/// unavailable until their product verticals exist.
#[derive(Debug)]
pub struct SelectionSchedule {
    form: SelectionScheduleForm,
}

#[derive(Debug)]
enum SelectionScheduleForm {
    Complete,
    /// The sole `Exact` terminal of the paired footprint with its authored origin identity. There is exactly one
    /// terminal, so no ordinal rides alongside; future multiplicity forms arrive as new enum arms.
    ExactSingleton {
        origin: SelectionOrigin,
    },
}

impl SelectionSchedule {
    /// Creates the only empty complete-document schedule for `footprint`.
    pub fn try_empty_complete(
        footprint: &AccessFootprint,
        _resources: &ResourceContext<'_>,
    ) -> Result<Self, SelectionScheduleError> {
        if !footprint.is_whole() {
            return Err(SelectionScheduleError::CompleteScheduleRequiresWhole);
        }
        Ok(Self {
            form: SelectionScheduleForm::Complete,
        })
    }

    /// Creates one singleton schedule entry that references the exact terminal of `footprint` and preserves its
    /// authored origin identity.
    pub fn try_singleton_exact(
        footprint: &AccessFootprint,
        origin: SelectionOrigin,
        _resources: &ResourceContext<'_>,
    ) -> Result<Self, SelectionScheduleError> {
        if !footprint.is_exact() {
            return Err(SelectionScheduleError::SingletonScheduleRequiresExact);
        }
        Ok(Self {
            form: SelectionScheduleForm::ExactSingleton { origin },
        })
    }

    /// Returns whether this is the complete-document empty schedule.
    #[must_use]
    pub const fn is_empty_complete(&self) -> bool {
        matches!(self.form, SelectionScheduleForm::Complete)
    }

    /// Returns the sole exact emission origin, if present.
    #[must_use]
    pub const fn singleton_origin(&self) -> Option<SelectionOrigin> {
        match &self.form {
            SelectionScheduleForm::Complete => None,
            SelectionScheduleForm::ExactSingleton { origin, .. } => Some(*origin),
        }
    }

    /// Returns exact canonical structural equality, independent of account identity and fingerprint collisions.
    #[must_use]
    pub fn structurally_eq(&self, other: &Self) -> bool {
        match (&self.form, &other.form) {
            (SelectionScheduleForm::Complete, SelectionScheduleForm::Complete) => true,
            (
                SelectionScheduleForm::ExactSingleton { origin: left_origin },
                SelectionScheduleForm::ExactSingleton { origin: right_origin },
            ) => left_origin == right_origin,
            _ => false,
        }
    }
}

impl PartialEq for SelectionSchedule {
    fn eq(&self, other: &Self) -> bool {
        self.structurally_eq(other)
    }
}

impl Eq for SelectionSchedule {}

#[cfg(test)]
mod tests {
    use super::{SelectionOrigin, SelectionSchedule, SelectionScheduleError};
    use crate::pattern::{AccessFootprint, ExactPath};
    use crate::test_support::resources;

    #[test]
    fn rejects_invalid_footprint_schedule_pairings() {
        let resources = resources();
        let whole = AccessFootprint::try_whole(&resources);
        let exact = AccessFootprint::try_exact(ExactPath::try_new(&resources), &resources);

        assert_eq!(
            SelectionSchedule::try_empty_complete(&exact, &resources),
            Err(SelectionScheduleError::CompleteScheduleRequiresWhole)
        );

        assert_eq!(
            SelectionSchedule::try_singleton_exact(&whole, SelectionOrigin::new(0), &resources),
            Err(SelectionScheduleError::SingletonScheduleRequiresExact)
        );
    }

    #[test]
    fn singleton_schedule_preserves_origin() {
        let resources = resources();
        let exact = AccessFootprint::try_exact(ExactPath::try_new(&resources), &resources);
        let origin = SelectionOrigin::new(1);
        let schedule = SelectionSchedule::try_singleton_exact(&exact, origin, &resources).expect("schedule");

        assert_eq!(schedule.singleton_origin(), Some(origin));
    }
}
