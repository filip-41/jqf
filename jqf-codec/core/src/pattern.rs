//! Portable structural access footprints: whole document or an exact path.
//!
//! Sibling: [`crate::prune`] builds a kept-subtree hint from an exact path.

use alloc::string::String;
use alloc::vec::Vec;
use jqf_resource::{ResourceContext, ResourceError};

/// Version of the canonical access-footprint encoding.
///
/// Version 2 adds [`PortableStep::SemanticRange`]. Version 3 retires the projection byte: every exact footprint selects
/// the value AND its complete semantic subtree, so the byte encoded a distinction nothing could observe. The bump is
/// mandatory rather than cosmetic: version 1 encodings had no step tag `2`, so a v1 reader handed a v2 range encoding
/// would mis-parse the step stream rather than reject it.
pub(crate) const ACCESS_FOOTPRINT_VERSION: u32 = 3;

/// One portable step in an exact semantic path.
///
/// The vocabulary deliberately contains no format-native token, parser, or event concepts. A negative index preserves
/// the signed index semantics; the interpreter resolves it against the observed container length.
#[derive(Debug, Eq, PartialEq)]
pub enum PortableStep {
    /// Select one exact format-neutral semantic member identity.
    SemanticMember(String),
    /// Select one exact signed semantic array position.
    SemanticIndex(i64),
    /// Select the contiguous signed semantic RANGE `[start, end)` of an array container.
    ///
    /// Both bounds carry the SAME resolution law [`Self::SemanticIndex`] does, and for the same reason: the interpreter
    /// resolves them against the observed container length. `None` is an OPEN bound (the container's own edge). The
    /// engine normalizes an authored `.[a:b]` to this form at lower time — start floored, end ceiled, saturating at
    /// the `i64` edges — and declines every bound spelling that no signed integer can carry, so a step that reaches
    /// this vocabulary is always resolvable from the length alone.
    SemanticRange {
        /// The inclusive start position, or `None` for the container's start.
        start: Option<i64>,
        /// The exclusive end position, or `None` for the container's end.
        end: Option<i64>,
    },
}

/// One [`PortableStep`] copied into session-owned storage.
///
/// A pushed-down route reads its path once at open and navigates with it on every poll, so the copy outlives every
/// document the session decodes — the retained memory category the [`own_steps`] copy charges.
#[derive(Debug)]
pub enum OwnedStep {
    /// The exact format-neutral semantic member identity.
    Member(String),
    /// One signed semantic array position.
    Index(i64),
    /// A contiguous signed range `[start, end)` of an array container.
    ///
    /// Both bounds resolve exactly as [`PortableStep::SemanticRange`] resolves them; that variant's doc is the law's
    /// single home.
    Range {
        /// Lower bound (open at the container's start when `None`).
        start: Option<i64>,
        /// Upper bound (open at the container's end when `None`).
        end: Option<i64>,
    },
}

/// Copies a requirement's portable path into session-owned storage.
///
/// Every pushed-down route needs the same copy for the same reason — the requirement it reads the path from does not
/// outlive the open — so the charge is the retained category for all of them.
pub fn own_steps(steps: &[PortableStep]) -> Result<Vec<OwnedStep>, ResourceError> {
    let mut owned = Vec::new();
    owned.try_reserve_exact(steps.len()).map_err(ResourceError::from)?;
    for step in steps {
        let owned_step = match step {
            PortableStep::SemanticMember(member) => {
                let member = member.as_str();
                let mut stored = String::new();
                stored.try_reserve_exact(member.len()).map_err(ResourceError::from)?;
                stored.push_str(member);
                OwnedStep::Member(stored)
            }
            PortableStep::SemanticIndex(index) => OwnedStep::Index(*index),
            PortableStep::SemanticRange { start, end } => OwnedStep::Range {
                start: *start,
                end: *end,
            },
        };
        owned.push(owned_step);
    }
    Ok(owned)
}

/// Immutable exact semantic path.
#[derive(Debug)]
pub struct ExactPath {
    steps: Vec<PortableStep>,
}

impl ExactPath {
    /// Creates an empty exact path, the sole representation of a root selection when it is wrapped by
    /// [`AccessFootprint::try_exact`].
    pub fn try_new(_resources: &ResourceContext<'_>) -> Self {
        Self { steps: Vec::new() }
    }

    /// Appends one exact semantic member identity.
    pub fn try_push_semantic_member(
        &mut self,
        member: &str,
        _resources: &ResourceContext<'_>,
    ) -> Result<(), ResourceError> {
        let mut stored = String::new();
        stored
            .try_reserve_exact(member.len())
            .map_err(jqf_resource::ResourceError::from)?;
        stored.push_str(member);
        self.steps.push(PortableStep::SemanticMember(stored));
        Ok(())
    }

    /// Appends one exact signed semantic array position.
    pub fn try_push_semantic_index(&mut self, index: i64, _resources: &ResourceContext<'_>) {
        self.steps.push(PortableStep::SemanticIndex(index));
    }

    /// Appends one contiguous signed semantic RANGE step (`None` is an open bound). See [`PortableStep::SemanticRange`]
    /// for the resolution law.
    pub fn try_push_semantic_range(&mut self, start: Option<i64>, end: Option<i64>, _resources: &ResourceContext<'_>) {
        self.steps.push(PortableStep::SemanticRange { start, end });
    }

    /// Returns the exact path steps in authored structural order.
    #[must_use]
    pub fn steps(&self) -> &[PortableStep] {
        &self.steps
    }

    pub(crate) fn try_clone_in(&self, resources: &ResourceContext<'_>) -> Result<Self, ResourceError> {
        let mut copy = Self::try_new(resources);
        for step in self.steps() {
            match step {
                PortableStep::SemanticMember(key) => {
                    copy.try_push_semantic_member(key.as_str(), resources)?;
                }
                PortableStep::SemanticIndex(index) => {
                    copy.try_push_semantic_index(*index, resources);
                }
                PortableStep::SemanticRange { start, end } => {
                    copy.try_push_semantic_range(*start, *end, resources);
                }
            }
        }
        Ok(copy)
    }

    /// Returns whether this is the canonical empty root path.
    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.steps.is_empty()
    }

    /// Whether any step is a [`PortableStep::SemanticRange`].
    ///
    /// The NO-CORE-FALLBACK gate reads it: a core fallback adapter walks an already-materialized document and would
    /// have to reproduce the reference's whole len-relative slice law to serve a range step. Rather than teach the
    /// format-neutral fallback interpreter slice semantics, a range footprint simply fails to BIND against any adapter,
    /// and the caller keeps its ordinary route: a missing native route is a bind failure, never a silent substitution.
    #[must_use]
    pub fn has_semantic_range(&self) -> bool {
        self.steps()
            .iter()
            .any(|step| matches!(step, PortableStep::SemanticRange { .. }))
    }

    fn structurally_eq(&self, other: &Self) -> bool {
        self.steps() == other.steps()
    }
}

/// Deterministic non-authoritative fingerprint of one exact access footprint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccessFootprintFingerprint {
    value: u64,
}

/// Opaque canonical portable structural footprint.
///
/// This surface intentionally exports only complete-document and one exact-path forms. Future path-set, repeat,
/// descent, and union variants are absent until their complete provider and engine verticals exist.
#[derive(Debug)]
pub struct AccessFootprint {
    form: FootprintForm,
}

#[derive(Debug)]
enum FootprintForm {
    Whole,
    Exact { path: ExactPath },
}

impl AccessFootprint {
    /// Creates the one complete-document structural footprint.
    pub fn try_whole(_resources: &ResourceContext<'_>) -> Self {
        Self {
            form: FootprintForm::Whole,
        }
    }

    /// Creates the one exact-path structural footprint.
    ///
    /// An empty `path` is the sole root-selection representation. It is never normalized to [`Self::try_whole`],
    /// because whole-document authority and root selection have different result contracts. The selection covers the
    /// value at the path AND its complete semantic subtree — the only authority any provider serves, so no projection
    /// dial exists.
    pub fn try_exact(path: ExactPath, _resources: &ResourceContext<'_>) -> Self {
        Self {
            form: FootprintForm::Exact { path },
        }
    }

    /// Returns whether this means complete semantic input authority.
    #[must_use]
    pub const fn is_whole(&self) -> bool {
        matches!(self.form, FootprintForm::Whole)
    }

    /// Returns the selected exact path, if this is an exact footprint.
    #[must_use]
    pub fn exact_path(&self) -> Option<&ExactPath> {
        match &self.form {
            FootprintForm::Whole => None,
            FootprintForm::Exact { path, .. } => Some(path),
        }
    }

    pub(crate) fn try_clone_in(&self, resources: &ResourceContext<'_>) -> Result<Self, ResourceError> {
        match &self.form {
            FootprintForm::Whole => Ok(Self::try_whole(resources)),
            FootprintForm::Exact { path } => Ok(Self::try_exact(path.try_clone_in(resources)?, resources)),
        }
    }

    /// Returns exact canonical structural equality, independent of account identity and fingerprint collisions.
    #[must_use]
    pub fn structurally_eq(&self, other: &Self) -> bool {
        match (&self.form, &other.form) {
            (FootprintForm::Whole, FootprintForm::Whole) => true,
            (FootprintForm::Exact { path: left_path }, FootprintForm::Exact { path: right_path }) => {
                left_path.structurally_eq(right_path)
            }
            _ => false,
        }
    }

    /// Returns a deterministic non-authoritative fingerprint of the canonical encoding. On the recycled-session path
    /// this value IS the reuse key: matching compares raw fingerprints with plain equality and never re-walks
    /// structure, so exact canonical comparison is asserted by tests and tooling ([`Self::structurally_eq`]) but NOT
    /// enforced where a residual is reused — a collision would recycle the wrong session silently. The encoding
    /// version mixes into the hash, so keys cannot match across encoding generations.
    #[must_use]
    pub fn fingerprint(&self) -> AccessFootprintFingerprint {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        mix_bytes(&mut hash, &ACCESS_FOOTPRINT_VERSION.to_le_bytes());
        match &self.form {
            FootprintForm::Whole => mix(&mut hash, 0),
            FootprintForm::Exact { path } => {
                mix(&mut hash, 1);
                mix_path(&mut hash, path);
            }
        }
        AccessFootprintFingerprint { value: hash }
    }

    /// Whether this footprint addresses one exact path.
    pub(crate) const fn is_exact(&self) -> bool {
        matches!(self.form, FootprintForm::Exact { .. })
    }
}

impl PartialEq for AccessFootprint {
    fn eq(&self, other: &Self) -> bool {
        self.structurally_eq(other)
    }
}

impl Eq for AccessFootprint {}

/// The fingerprint contribution of one optional signed bound: a presence byte plus, when present, the little-endian
/// `i64`. Absent bounds still mix the byte, so the contribution stays fixed-width per bound.
fn bound_bytes(bound: Option<i64>) -> (u8, [u8; 8]) {
    match bound {
        None => (0, [0; 8]),
        Some(value) => (1, value.to_le_bytes()),
    }
}

/// Mixes one exact path's canonical step list into a fingerprint.
fn mix_path(hash: &mut u64, path: &ExactPath) {
    mix_bytes(
        hash,
        &u32::try_from(path.steps().len()).unwrap_or(u32::MAX).to_le_bytes(),
    );
    for step in path.steps() {
        match step {
            PortableStep::SemanticMember(member) => {
                mix(hash, 0);
                mix_bytes(
                    hash,
                    &u32::try_from(member.as_str().len()).unwrap_or(u32::MAX).to_le_bytes(),
                );
                mix_bytes(hash, member.as_str().as_bytes());
            }
            PortableStep::SemanticIndex(index) => {
                mix(hash, 1);
                mix_bytes(hash, &index.to_le_bytes());
            }
            PortableStep::SemanticRange { start, end } => {
                mix(hash, 2);
                for bound in [*start, *end] {
                    let (present, value) = bound_bytes(bound);
                    mix(hash, present);
                    mix_bytes(hash, &value);
                }
            }
        }
    }
}

fn mix(hash: &mut u64, byte: u8) {
    *hash ^= u64::from(byte);
    *hash = hash.wrapping_mul(0x100_0000_01b3);
}

fn mix_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        mix(hash, *byte);
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::{AccessFootprint, ExactPath};
    use crate::test_support::resources;

    #[test]
    fn root_selection_has_one_exact_representation() {
        let resources = resources();
        let root = ExactPath::try_new(&resources);
        let exact = AccessFootprint::try_exact(root, &resources);
        let whole = AccessFootprint::try_whole(&resources);

        assert!(exact.exact_path().is_some_and(ExactPath::is_root));
        assert!(!whole.exact_path().is_some_and(ExactPath::is_root));
        assert_ne!(exact, whole);
    }

    #[test]
    fn equal_footprints_have_equal_fingerprints() {
        let resources = resources();
        let mut first_path = ExactPath::try_new(&resources);
        first_path
            .try_push_semantic_member("items", &resources)
            .expect("member");
        first_path.try_push_semantic_index(-1, &resources);
        let first = AccessFootprint::try_exact(first_path, &resources);

        let mut second_path = ExactPath::try_new(&resources);
        second_path
            .try_push_semantic_member("items", &resources)
            .expect("member");
        second_path.try_push_semantic_index(-1, &resources);
        let second = AccessFootprint::try_exact(second_path, &resources);

        assert_eq!(first, second);
        assert_eq!(first.fingerprint(), second.fingerprint());
    }
}
