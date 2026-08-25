//! Compact ids for a document's kind, role, format, and dialect names.
//!
//! A prepared schema is built once from a [`DocumentSchemaRecipe`]. A dynamic schema grows as the builder admits names.
//! Storage and readers then use integer ids.

use alloc::{sync::Arc, vec::Vec};
use core::{
    num::NonZeroU64,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::identity::{IdentityId, IdentityInterner, IdentityText};
use crate::{DialectId, FormatId};

use super::{
    DataError, DocumentId, DocumentNodeKindId, FactKindId, FactRoleId, NodeRecord, OccurrenceRecord, OccurrenceRoleId,
    StoredDocumentFact,
};

static NEXT_SCHEMA_PROTOTYPE_ID: AtomicU64 = AtomicU64::new(1);

/// Process-local fresh identity for one prepared schema build; a prepared document's handles are bound to it so a
/// handle from one build cannot pass another's verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DocumentSchemaPrototypeId(NonZeroU64);

impl DocumentSchemaPrototypeId {
    fn try_fresh() -> Option<Self> {
        let value = NEXT_SCHEMA_PROTOTYPE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
            .ok()?;
        NonZeroU64::new(value).map(Self)
    }
}

macro_rules! binding_id {
    ($(#[$doc:meta])* $vis:vis $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        $vis struct $name(u32);
        impl $name {
            fn from_index(index: usize) -> Option<Self> {
                u32::try_from(index).ok().map(Self)
            }
            pub(crate) const fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

binding_id!(pub(crate) FormatBindingId);
binding_id!(pub(crate) DialectBindingId);
binding_id!(pub(crate) NodeKindBindingId);
binding_id!(pub(crate) OccurrenceRoleBindingId);

binding_id!(
    #[doc = "Compact interned fact-kind id inside one document schema."]
    #[doc = ""]
    #[doc = "Compare with [`super::DocumentFact::kind_binding`] instead of the namespaced string."]
    pub FactKindBindingId
);

binding_id!(
    #[doc = "Compact interned fact-role id inside one document schema."]
    #[doc = ""]
    #[doc = "Compare with [`super::DocumentFact::role_binding`] instead of the namespaced string."]
    pub FactRoleBindingId
);

/// Benchmark-observable proof of how a schema was built: whether it was recipe-provisioned or grown dynamically, and
/// the append counters for each route. Exists only with the `benchmark-internals` feature; the production build does
/// not carry the counters or the document field that would hold them.
#[cfg(feature = "benchmark-internals")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SchemaExecution {
    recipe_fingerprint: Option<u64>,
    prepared_appends: u64,
    dynamic_appends: u64,
    dynamic_existing_schema_fast_appends: u64,
    dynamic_schema_transaction_appends: u64,
    accounted_frontend: bool,
}

#[cfg(feature = "benchmark-internals")]
impl SchemaExecution {
    pub(crate) const fn accounted_dynamic() -> Self {
        Self {
            recipe_fingerprint: None,
            prepared_appends: 0,
            dynamic_appends: 0,
            dynamic_existing_schema_fast_appends: 0,
            dynamic_schema_transaction_appends: 0,
            accounted_frontend: true,
        }
    }
    pub(crate) fn bind_prepared(&mut self, fingerprint: u64) {
        self.recipe_fingerprint = Some(fingerprint);
    }
    pub(crate) fn record_prepared(&mut self) {
        self.prepared_appends = self.prepared_appends.saturating_add(1);
    }
    pub(crate) fn record_dynamic(&mut self) {
        self.dynamic_appends = self.dynamic_appends.saturating_add(1);
    }
    pub(crate) fn record_dynamic_schema_route(&mut self, existing_fast: bool) {
        if existing_fast {
            self.dynamic_existing_schema_fast_appends = self.dynamic_existing_schema_fast_appends.saturating_add(1);
        } else {
            self.dynamic_schema_transaction_appends = self.dynamic_schema_transaction_appends.saturating_add(1);
        }
    }
    pub(crate) const fn prepared_only(self) -> bool {
        self.recipe_fingerprint.is_some()
            && self.prepared_appends != 0
            && self.dynamic_appends == 0
            && self.accounted_frontend
    }
    pub(crate) const fn recipe_fingerprint(self) -> Option<u64> {
        self.recipe_fingerprint
    }
    pub(crate) const fn prepared_appends(self) -> u64 {
        self.prepared_appends
    }
    pub(crate) const fn dynamic_appends(self) -> u64 {
        self.dynamic_appends
    }
    pub(crate) const fn dynamic_existing_schema_fast_appends(self) -> u64 {
        self.dynamic_existing_schema_fast_appends
    }
    pub(crate) const fn dynamic_schema_transaction_appends(self) -> u64 {
        self.dynamic_schema_transaction_appends
    }
    pub(crate) const fn accounted_frontend(self) -> bool {
        self.accounted_frontend
    }
}

/// One schema table row: the interned identity and the owned value it names.
pub(crate) struct Binding<T> {
    identity: IdentityId,
    value: T,
}

/// Immutable post-build schema: one interned identity table plus six typed binding tables (formats, dialects, node
/// kinds, occurrence roles, fact kinds, fact roles).
pub(crate) struct DocumentSchema {
    identities: Vec<IdentityText>,
    formats: Vec<Binding<FormatId>>,
    dialects: Vec<Binding<DialectId>>,
    node_kinds: Vec<Binding<DocumentNodeKindId>>,
    occurrence_roles: Vec<Binding<OccurrenceRoleId>>,
    fact_kinds: Vec<Binding<FactKindId>>,
    fact_roles: Vec<Binding<FactRoleId>>,
}

impl DocumentSchema {
    pub(crate) fn format(&self, id: FormatBindingId) -> Option<&FormatId> {
        self.formats.get(id.index()).map(|binding| &binding.value)
    }
    pub(crate) fn dialect(&self, id: DialectBindingId) -> Option<&DialectId> {
        self.dialects.get(id.index()).map(|binding| &binding.value)
    }
    pub(crate) fn validated_node_kind(&self, id: NodeKindBindingId) -> &DocumentNodeKindId {
        let index = id.index();
        debug_assert!(index < self.node_kinds.len());
        // SAFETY: `validate_published_records` bounds-checks every published
        // node record before this accessor is reachable.
        &unsafe { self.node_kinds.get_unchecked(index) }.value
    }

    pub(crate) fn validated_occurrence_role(&self, id: OccurrenceRoleBindingId) -> &OccurrenceRoleId {
        let index = id.index();
        debug_assert!(index < self.occurrence_roles.len());
        // SAFETY: `validate_published_records` bounds-checks every published
        // occurrence record before this accessor is reachable.
        &unsafe { self.occurrence_roles.get_unchecked(index) }.value
    }

    pub(crate) fn validated_fact_kind(&self, id: FactKindBindingId) -> &FactKindId {
        let index = id.index();
        debug_assert!(index < self.fact_kinds.len());
        // SAFETY: `validate_published_records` bounds-checks every published
        // fact kind binding before this accessor is reachable.
        &unsafe { self.fact_kinds.get_unchecked(index) }.value
    }

    pub(crate) fn validated_fact_role(&self, id: FactRoleBindingId) -> &FactRoleId {
        let index = id.index();
        debug_assert!(index < self.fact_roles.len());
        // SAFETY: `validate_published_records` bounds-checks every published
        // fact role binding before this accessor is reachable.
        &unsafe { self.fact_roles.get_unchecked(index) }.value
    }

    pub(crate) fn fact_role_binding(&self, role: &str) -> Option<FactRoleBindingId> {
        self.fact_roles
            .iter()
            .position(|binding| binding.value.as_str() == role)
            .and_then(FactRoleBindingId::from_index)
    }

    pub(crate) fn fact_kind_binding(&self, kind: &str) -> Option<FactKindBindingId> {
        self.fact_kinds
            .iter()
            .position(|binding| binding.value.as_str() == kind)
            .and_then(FactKindBindingId::from_index)
    }

    pub(crate) fn interned_fact_roles(&self) -> impl Iterator<Item = (FactRoleBindingId, &str)> + '_ {
        self.fact_roles
            .iter()
            .enumerate()
            .filter_map(|(index, binding)| FactRoleBindingId::from_index(index).map(|id| (id, binding.value.as_str())))
    }

    pub(crate) fn validate(&self) -> bool {
        let len = self.identities.len();
        let matches = |identity: IdentityId, value: &str| {
            self.identities
                .get(identity.index())
                .is_some_and(|text| text.as_str() == value)
        };
        macro_rules! table_bound {
            ($field:ident) => {
                self.$field
                    .iter()
                    .all(|value| value.identity.index() < len && matches(value.identity, value.value.as_str()))
            };
        }
        table_bound!(formats)
            && table_bound!(dialects)
            && table_bound!(node_kinds)
            && table_bound!(occurrence_roles)
            && table_bound!(fact_kinds)
            && table_bound!(fact_roles)
    }

    /// Proves every binding index in the records about to become immutable names a row in its corresponding schema
    /// table.
    pub(crate) fn validate_published_records(
        &self,
        nodes: &[NodeRecord],
        occurrences: &[OccurrenceRecord],
        facts: &[StoredDocumentFact],
    ) -> bool {
        nodes.iter().all(|record| record.kind.index() < self.node_kinds.len())
            && occurrences
                .iter()
                .all(|record| record.role.index() < self.occurrence_roles.len())
            && facts.iter().all(|record| {
                record.kind_binding().index() < self.fact_kinds.len()
                    && record.role_binding().index() < self.fact_roles.len()
            })
    }

    #[cfg(any(test, feature = "benchmark-internals"))]
    pub(crate) fn counts(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.identities.len(),
            self.node_kinds.len(),
            self.occurrence_roles.len(),
            self.fact_kinds.len(),
            self.fact_roles.len(),
        )
    }

    #[cfg(feature = "benchmark-internals")]
    pub(crate) fn identity_utf8_bytes(&self) -> usize {
        self.identities.iter().map(|value| value.as_str().len()).sum()
    }

    #[cfg(feature = "benchmark-internals")]
    pub(crate) fn shallow_table_bytes(&self) -> usize {
        self.identities
            .capacity()
            .saturating_mul(core::mem::size_of::<IdentityText>())
            .saturating_add(
                self.formats
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Binding<FormatId>>()),
            )
            .saturating_add(
                self.dialects
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Binding<DialectId>>()),
            )
            .saturating_add(
                self.node_kinds
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Binding<DocumentNodeKindId>>()),
            )
            .saturating_add(
                self.occurrence_roles
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Binding<OccurrenceRoleId>>()),
            )
            .saturating_add(
                self.fact_kinds
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Binding<FactKindId>>()),
            )
            .saturating_add(
                self.fact_roles
                    .capacity()
                    .saturating_mul(core::mem::size_of::<Binding<FactRoleId>>()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_ids_are_four_bytes() {
        assert_eq!(core::mem::size_of::<IdentityId>(), 4);
        assert_eq!(core::mem::size_of::<FormatBindingId>(), 4);
        assert_eq!(core::mem::size_of::<DialectBindingId>(), 4);
        assert_eq!(core::mem::size_of::<NodeKindBindingId>(), 4);
        assert_eq!(core::mem::size_of::<OccurrenceRoleBindingId>(), 4);
        assert_eq!(core::mem::size_of::<FactKindBindingId>(), 4);
        assert_eq!(core::mem::size_of::<FactRoleBindingId>(), 4);
    }

    #[test]
    fn recipe_rejects_invalid_and_duplicate_typed_entries() {
        assert!(DocumentSchemaRecipe::try_new("json", None, &["kind", "kind"], &[], &[], &[]).is_err());
        assert!(DocumentSchemaRecipe::try_new("json", None, &["bad kind"], &[], &[], &[]).is_err());
        assert!(DocumentSchemaRecipe::try_new("json", None, &["same"], &["same"], &[], &[]).is_ok());
    }

    #[test]
    fn recipe_fingerprint_is_stable_and_order_sensitive() {
        let first = DocumentSchemaRecipe::try_new("json", Some("rfc8259"), &["a", "b"], &["r"], &[], &[]).unwrap();
        let same = DocumentSchemaRecipe::try_new("json", Some("rfc8259"), &["a", "b"], &["r"], &[], &[]).unwrap();
        let reordered = DocumentSchemaRecipe::try_new("json", Some("rfc8259"), &["b", "a"], &["r"], &[], &[]).unwrap();
        assert_eq!(first.fingerprint(), same.fingerprint());
        assert_ne!(first.fingerprint(), reordered.fingerprint());
    }

    #[test]
    fn checked_ids_reject_indices_above_u32() {
        if usize::BITS > 32 {
            assert!(IdentityId::from_index(u32::MAX as usize + 1).is_none());
            assert!(NodeKindBindingId::from_index(u32::MAX as usize + 1).is_none());
        }
    }
}

/// Resource-accounted incremental schema builder with per-table last-hit caches for the existing-schema fast-append
/// path.
pub(crate) struct AccountedSchemaBuilder {
    identities: IdentityInterner,
    formats: Vec<Binding<FormatId>>,
    dialects: Vec<Binding<DialectId>>,
    node_kinds: Vec<Binding<DocumentNodeKindId>>,
    occurrence_roles: Vec<Binding<OccurrenceRoleId>>,
    fact_kinds: Vec<Binding<FactKindId>>,
    fact_roles: Vec<Binding<FactRoleId>>,
    last_node_kind: Option<NodeKindBindingId>,
    last_occurrence_role: Option<OccurrenceRoleBindingId>,
    last_fact_kind: Option<FactKindBindingId>,
    last_fact_role: Option<FactRoleBindingId>,
}

/// The outcome of resolving one append's identities: the resolved ids and the tag identity text, whether the rows
/// already existed or were appended by this batch.
pub(crate) struct DynamicSchemaBindings {
    node_kind: Option<NodeKindBindingId>,
    occurrence_role: Option<OccurrenceRoleBindingId>,
    fact_kind: Option<FactKindBindingId>,
    fact_role: Option<FactRoleBindingId>,
    tag_text: Option<IdentityText>,
    /// Whether every demanded identity was already bound (no staged work).
    #[cfg(feature = "benchmark-internals")]
    existing: bool,
}

impl DynamicSchemaBindings {
    pub(crate) const fn node_kind(&self) -> Option<NodeKindBindingId> {
        self.node_kind
    }
    pub(crate) const fn occurrence_role(&self) -> Option<OccurrenceRoleBindingId> {
        self.occurrence_role
    }
    pub(crate) const fn fact_kind(&self) -> Option<FactKindBindingId> {
        self.fact_kind
    }
    pub(crate) const fn fact_role(&self) -> Option<FactRoleBindingId> {
        self.fact_role
    }
    pub(crate) fn take_tag_text(&mut self) -> Option<IdentityText> {
        self.tag_text.take()
    }
    #[cfg(feature = "benchmark-internals")]
    pub(crate) const fn is_existing(&self) -> bool {
        self.existing
    }
}

macro_rules! find_binding {
    ($name:ident, $field:ident, $last:ident, $id:ident) => {
        #[inline]
        fn $name(&mut self, value: &str) -> Option<$id> {
            if let Some(id) = self.$last
                && self
                    .$field
                    .as_slice()
                    .get(id.index())
                    .is_some_and(|binding| binding.value.as_str() == value)
            {
                return Some(id);
            }
            let id = self
                .$field
                .as_slice()
                .iter()
                .position(|binding| binding.value.as_str() == value)
                .and_then($id::from_index);
            self.$last = id;
            id
        }
    };
}

macro_rules! bind_accounted {
    ($self:ident, $field:ident, $id:ident, $wrapper:ident, $value:expr) => {{
        if let Some(index) = $self
            .$field
            .as_slice()
            .iter()
            .position(|binding| binding.value.as_str() == $value)
        {
            return $id::from_index(index).ok_or(DataError::ArithmeticOverflow);
        }
        crate::identity::validate($value).map_err(|_| DataError::InvalidDocument)?;
        let (identity, text) = $self.identities.try_prepare_one($value)?;
        let index = $id::from_index($self.$field.len()).ok_or(DataError::ArithmeticOverflow)?;
        $self.$field.try_reserve(1).map_err(jqf_resource::ResourceError::from)?;
        $self.$field.push(Binding {
            identity,
            value: $wrapper::from_accounted(text),
        });
        Ok(index)
    }};
}

impl AccountedSchemaBuilder {
    pub(crate) const fn new() -> Self {
        Self {
            identities: IdentityInterner::new(),
            formats: Vec::new(),
            dialects: Vec::new(),
            node_kinds: Vec::new(),
            occurrence_roles: Vec::new(),
            fact_kinds: Vec::new(),
            fact_roles: Vec::new(),
            last_node_kind: None,
            last_occurrence_role: None,
            last_fact_kind: None,
            last_fact_role: None,
        }
    }

    /// Resolves an append's typed identities, taking the existing-schema fast path only when every demanded identity is
    /// already bound AND the tag text is absent or already interned; anything else stages a preparation batch.
    pub(crate) fn resolve_or_prepare_bindings(
        &mut self,
        node_kind: Option<&str>,
        occurrence_role: Option<&str>,
        fact_kind: Option<&str>,
        fact_role: Option<&str>,
        tag_text: Option<&str>,
    ) -> Result<DynamicSchemaBindings, DataError> {
        let node_kind_id = node_kind.and_then(|value| self.find_node_kind(value));
        let occurrence_role_id = occurrence_role.and_then(|value| self.find_occurrence_role(value));
        let fact_kind_id = fact_kind.and_then(|value| self.find_fact_kind(value));
        let fact_role_id = fact_role.and_then(|value| self.find_fact_role(value));
        let all_typed_exist = node_kind.is_none() == node_kind_id.is_none()
            && occurrence_role.is_none() == occurrence_role_id.is_none()
            && fact_kind.is_none() == fact_kind_id.is_none()
            && fact_role.is_none() == fact_role_id.is_none();
        if all_typed_exist {
            let existing_tag = tag_text.and_then(|value| self.identities.existing_text(value));
            if tag_text.is_none() || existing_tag.is_some() {
                return Ok(DynamicSchemaBindings {
                    node_kind: node_kind_id,
                    occurrence_role: occurrence_role_id,
                    fact_kind: fact_kind_id,
                    fact_role: fact_role_id,
                    tag_text: existing_tag,
                    #[cfg(feature = "benchmark-internals")]
                    existing: true,
                });
            }
        }
        self.try_prepare_bindings(node_kind, occurrence_role, fact_kind, fact_role, tag_text)
    }

    // Generates the last-hit-cache table scans. Called once per node/occurrence from the existing-schema fast-append
    // path (`existing_node_bindings`/`existing_occurrence_role`). The last-hit cache check plus short linear scan is
    // hot there, so `#[inline]` folds it into the caller instead of paying a call frame per node/occurrence.
    find_binding!(find_node_kind, node_kinds, last_node_kind, NodeKindBindingId);
    find_binding!(
        find_occurrence_role,
        occurrence_roles,
        last_occurrence_role,
        OccurrenceRoleBindingId
    );
    find_binding!(find_fact_kind, fact_kinds, last_fact_kind, FactKindBindingId);
    find_binding!(find_fact_role, fact_roles, last_fact_role, FactRoleBindingId);

    /// The existing-schema fast path for a node append: both bound ids, or `None` when either identity is not yet
    /// bound.
    pub(crate) fn existing_node_bindings(
        &mut self,
        kind: &str,
        role: Option<&str>,
    ) -> Option<(NodeKindBindingId, Option<OccurrenceRoleBindingId>)> {
        let kind = self.find_node_kind(kind)?;
        let role = match role {
            Some(value) => Some(self.find_occurrence_role(value)?),
            None => None,
        };
        Some((kind, role))
    }

    /// The existing-schema fast path for a keyless occurrence append.
    pub(crate) fn existing_occurrence_role(&mut self, role: &str) -> Option<OccurrenceRoleBindingId> {
        self.find_occurrence_role(role)
    }

    /// Validates every identity, interns the batch, and pushes row appends only for slots not yet bound, returning the
    /// resolved bindings.
    fn try_prepare_bindings(
        &mut self,
        node_kind: Option<&str>,
        occurrence_role: Option<&str>,
        fact_kind: Option<&str>,
        fact_role: Option<&str>,
        tag_text: Option<&str>,
    ) -> Result<DynamicSchemaBindings, DataError> {
        for value in [node_kind, occurrence_role, fact_kind, fact_role, tag_text]
            .into_iter()
            .flatten()
        {
            crate::identity::validate(value).map_err(|_| DataError::InvalidDocument)?;
        }
        let (ids, mut texts) =
            self.identities
                .try_prepare_batch([node_kind, occurrence_role, fact_kind, fact_role, tag_text])?;

        // One resolved slot: the existing row's id, or the next index with a row append carrying the batch-interned
        // identity.
        macro_rules! prepare_slot {
            ($value:ident, $field:ident, $id:ident, $wrapper:ident, $index:literal) => {{
                let slot_id = $value
                    .map(|value| {
                        self.$field
                            .as_slice()
                            .iter()
                            .position(|binding| binding.value.as_str() == value)
                            .map_or_else(|| $id::from_index(self.$field.len()), $id::from_index)
                            .ok_or(DataError::ArithmeticOverflow)
                    })
                    .transpose()?;
                if let Some(id) = slot_id
                    && id.index() == self.$field.len()
                {
                    self.$field
                        .try_reserve(1)
                        .map_err(jqf_resource::ResourceError::from)?;
                    self.$field.push(Binding {
                        identity: ids[$index].ok_or(DataError::ArithmeticOverflow)?,
                        value: $wrapper::from_accounted(texts[$index].take().ok_or(DataError::Allocation)?),
                    });
                }
                slot_id
            }};
        }

        Ok(DynamicSchemaBindings {
            node_kind: prepare_slot!(node_kind, node_kinds, NodeKindBindingId, DocumentNodeKindId, 0),
            occurrence_role: prepare_slot!(
                occurrence_role,
                occurrence_roles,
                OccurrenceRoleBindingId,
                OccurrenceRoleId,
                1
            ),
            fact_kind: prepare_slot!(fact_kind, fact_kinds, FactKindBindingId, FactKindId, 2),
            fact_role: prepare_slot!(fact_role, fact_roles, FactRoleBindingId, FactRoleId, 3),
            tag_text: texts[4].take(),
            #[cfg(feature = "benchmark-internals")]
            existing: false,
        })
    }

    pub(crate) fn bind_format(&mut self, value: &str) -> Result<FormatBindingId, DataError> {
        bind_accounted!(self, formats, FormatBindingId, FormatId, value)
    }
    pub(crate) fn bind_dialect(&mut self, value: &str) -> Result<DialectBindingId, DataError> {
        bind_accounted!(self, dialects, DialectBindingId, DialectId, value)
    }
    pub(crate) fn bind_node_kind(&mut self, value: &str) -> Result<NodeKindBindingId, DataError> {
        bind_accounted!(self, node_kinds, NodeKindBindingId, DocumentNodeKindId, value)
    }
    pub(crate) fn bind_occurrence_role(&mut self, value: &str) -> Result<OccurrenceRoleBindingId, DataError> {
        bind_accounted!(self, occurrence_roles, OccurrenceRoleBindingId, OccurrenceRoleId, value)
    }
    pub(crate) fn bind_fact_kind(&mut self, value: &str) -> Result<FactKindBindingId, DataError> {
        bind_accounted!(self, fact_kinds, FactKindBindingId, FactKindId, value)
    }
    pub(crate) fn bind_fact_role(&mut self, value: &str) -> Result<FactRoleBindingId, DataError> {
        bind_accounted!(self, fact_roles, FactRoleBindingId, FactRoleId, value)
    }
    fn try_reserve_recipe(&mut self, recipe: &DocumentSchemaRecipe<'_>) -> Result<(), DataError> {
        let identities = recipe
            .node_kinds
            .len()
            .checked_add(recipe.occurrence_roles.len())
            .and_then(|count| count.checked_add(recipe.fact_kinds.len()))
            .and_then(|count| count.checked_add(recipe.fact_roles.len()))
            .ok_or(DataError::ArithmeticOverflow)?;
        self.identities.try_reserve_exact(identities)?;
        self.node_kinds
            .try_reserve_exact(recipe.node_kinds.len())
            .map_err(jqf_resource::ResourceError::from)?;
        self.occurrence_roles
            .try_reserve_exact(recipe.occurrence_roles.len())
            .map_err(jqf_resource::ResourceError::from)?;
        self.fact_kinds
            .try_reserve_exact(recipe.fact_kinds.len())
            .map_err(jqf_resource::ResourceError::from)?;
        self.fact_roles
            .try_reserve_exact(recipe.fact_roles.len())
            .map_err(jqf_resource::ResourceError::from)?;
        Ok(())
    }

    pub(crate) fn finish(self) -> DocumentSchema {
        DocumentSchema {
            identities: self.identities.into_values(),
            formats: self.formats,
            dialects: self.dialects,
            node_kinds: self.node_kinds,
            occurrence_roles: self.occurrence_roles,
            fact_kinds: self.fact_kinds,
            fact_roles: self.fact_roles,
        }
    }
}

/// Immutable canonical-string schema recipe which may be cached by a codec.
pub struct DocumentSchemaRecipe<'recipe> {
    format: &'recipe str,
    dialect: Option<&'recipe str>,
    node_kinds: &'recipe [&'recipe str],
    occurrence_roles: &'recipe [&'recipe str],
    fact_kinds: &'recipe [&'recipe str],
    fact_roles: &'recipe [&'recipe str],
}

impl<'recipe> DocumentSchemaRecipe<'recipe> {
    /// Validates a borrowed recipe without allocating.
    pub fn try_new(
        format: &'recipe str,
        dialect: Option<&'recipe str>,
        node_kinds: &'recipe [&'recipe str],
        occurrence_roles: &'recipe [&'recipe str],
        fact_kinds: &'recipe [&'recipe str],
        fact_roles: &'recipe [&'recipe str],
    ) -> Result<Self, DataError> {
        crate::identity::validate(format).map_err(|_| DataError::InvalidDocument)?;
        if let Some(value) = dialect {
            crate::identity::validate(value).map_err(|_| DataError::InvalidDocument)?;
        }
        for group in [node_kinds, occurrence_roles, fact_kinds, fact_roles] {
            for (index, value) in group.iter().enumerate() {
                crate::identity::validate(value).map_err(|_| DataError::InvalidDocument)?;
                if group[..index].contains(value) {
                    return Err(DataError::InvalidDocument);
                }
            }
        }
        Ok(Self {
            format,
            dialect,
            node_kinds,
            occurrence_roles,
            fact_kinds,
            fact_roles,
        })
    }

    /// Returns the canonical format identity.
    #[must_use]
    pub const fn format(&self) -> &'recipe str {
        self.format
    }

    /// Returns the optional canonical dialect identity.
    #[must_use]
    pub const fn dialect(&self) -> Option<&'recipe str> {
        self.dialect
    }

    /// Returns a deterministic observation-only recipe fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for group in [
            core::slice::from_ref(&self.format),
            self.dialect.as_slice(),
            self.node_kinds,
            self.occurrence_roles,
            self.fact_kinds,
            self.fact_roles,
        ] {
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x100_0000_01b3);
            for value in group {
                for byte in value.as_bytes() {
                    hash ^= u64::from(*byte);
                    hash = hash.wrapping_mul(0x100_0000_01b3);
                }
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
        hash
    }
}

macro_rules! prepared_handle {
    ($name:ident, $id:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub struct $name {
            prototype: DocumentSchemaPrototypeId,
            key: DocumentId,
            id: $id,
        }
    };
}
prepared_handle!(
    PreparedNodeKind,
    NodeKindBindingId,
    "Opaque request-bound node-kind handle."
);
prepared_handle!(
    PreparedOccurrenceRole,
    OccurrenceRoleBindingId,
    "Opaque request-bound occurrence-role handle."
);
/// Request-scoped immutable schema storage shared by documents from one codec provider lifecycle.
///
/// The prototype owns only canonical schema identities and compact bindings. Every builder created from it still
/// receives a fresh document key and fresh semantic, topology, fact, and text arenas.
#[derive(Clone)]
pub struct DocumentSchemaPrototype {
    identity: DocumentSchemaPrototypeId,
    schema: Arc<DocumentSchema>,
    format: FormatBindingId,
    dialect: Option<DialectBindingId>,
    #[cfg(feature = "benchmark-internals")]
    recipe_fingerprint: u64,
    node_kind_count: u32,
    occurrence_role_count: u32,
}

impl DocumentSchemaPrototype {
    /// Builds one immutable schema from a validated recipe.
    pub fn try_new(recipe: &DocumentSchemaRecipe<'_>) -> Result<Self, DataError> {
        let identity = DocumentSchemaPrototypeId::try_fresh().ok_or(DataError::ArithmeticOverflow)?;
        let node_kind_count = u32::try_from(recipe.node_kinds.len()).map_err(|_| DataError::ArithmeticOverflow)?;
        let occurrence_role_count =
            u32::try_from(recipe.occurrence_roles.len()).map_err(|_| DataError::ArithmeticOverflow)?;
        let mut schema = AccountedSchemaBuilder::new();
        let format = schema.bind_format(recipe.format())?;
        let dialect = recipe.dialect().map(|value| schema.bind_dialect(value)).transpose()?;
        schema.try_reserve_recipe(recipe)?;
        for value in recipe.node_kinds {
            schema.bind_node_kind(value)?;
        }
        for value in recipe.occurrence_roles {
            schema.bind_occurrence_role(value)?;
        }
        for value in recipe.fact_kinds {
            schema.bind_fact_kind(value)?;
        }
        for value in recipe.fact_roles {
            schema.bind_fact_role(value)?;
        }
        let schema = schema.finish();
        if !schema.validate() {
            return Err(DataError::InvalidDocument);
        }
        Ok(Self {
            identity,
            schema: Arc::new(schema),
            format,
            dialect,
            #[cfg(feature = "benchmark-internals")]
            recipe_fingerprint: recipe.fingerprint(),
            node_kind_count,
            occurrence_role_count,
        })
    }

    /// Clones the immutable schema and binds a fresh prepared handle set for one document key.
    pub(crate) fn try_prepare_document(&self, key: DocumentId) -> (Arc<DocumentSchema>, PreparedDocumentSchema) {
        (
            Arc::clone(&self.schema),
            PreparedDocumentSchema {
                prototype: self.identity,
                key,
                node_kind_count: self.node_kind_count,
                occurrence_role_count: self.occurrence_role_count,
            },
        )
    }

    pub(crate) const fn identity(&self) -> DocumentSchemaPrototypeId {
        self.identity
    }

    pub(crate) const fn format_binding(&self) -> FormatBindingId {
        self.format
    }

    pub(crate) const fn dialect_binding(&self) -> Option<DialectBindingId> {
        self.dialect
    }

    #[cfg(feature = "benchmark-internals")]
    pub(crate) const fn recipe_fingerprint(&self) -> u64 {
        self.recipe_fingerprint
    }
}

/// Request- and prototype-bound prepared schema handles.
pub struct PreparedDocumentSchema {
    prototype: DocumentSchemaPrototypeId,
    key: DocumentId,
    node_kind_count: u32,
    occurrence_role_count: u32,
}

impl PreparedDocumentSchema {
    /// Provisions a fresh prototype and binds every recipe slot in order, failing when a recipe slot lands on a
    /// different index than the recipe promises.
    pub(crate) fn try_new(
        key: DocumentId,
        recipe: &DocumentSchemaRecipe<'_>,
        schema: &mut AccountedSchemaBuilder,
    ) -> Result<Self, DataError> {
        let prototype = DocumentSchemaPrototypeId::try_fresh().ok_or(DataError::ArithmeticOverflow)?;
        let node_kind_count = u32::try_from(recipe.node_kinds.len()).map_err(|_| DataError::ArithmeticOverflow)?;
        let occurrence_role_count =
            u32::try_from(recipe.occurrence_roles.len()).map_err(|_| DataError::ArithmeticOverflow)?;
        schema.try_reserve_recipe(recipe)?;
        macro_rules! bind_slots {
            ($slice:expr, $binder:ident) => {
                for (slot, value) in $slice.iter().enumerate() {
                    if schema.$binder(value)?.index() != slot {
                        return Err(DataError::InvalidDocument);
                    }
                }
            };
        }
        bind_slots!(recipe.node_kinds, bind_node_kind);
        bind_slots!(recipe.occurrence_roles, bind_occurrence_role);
        bind_slots!(recipe.fact_kinds, bind_fact_kind);
        bind_slots!(recipe.fact_roles, bind_fact_role);
        Ok(Self {
            prototype,
            key,
            node_kind_count,
            occurrence_role_count,
        })
    }

    pub(crate) const fn prototype_identity(&self) -> DocumentSchemaPrototypeId {
        self.prototype
    }

    /// Returns an opaque prepared node-kind handle for one recipe slot.
    #[must_use]
    pub fn node_kind(&self, slot: usize) -> Option<PreparedNodeKind> {
        u32::try_from(slot)
            .is_ok_and(|slot| slot < self.node_kind_count)
            .then(|| NodeKindBindingId::from_index(slot))
            .flatten()
            .map(|id| PreparedNodeKind {
                prototype: self.prototype,
                key: self.key,
                id,
            })
    }
    /// Returns an opaque prepared occurrence-role handle for one recipe slot.
    #[must_use]
    pub fn occurrence_role(&self, slot: usize) -> Option<PreparedOccurrenceRole> {
        u32::try_from(slot)
            .is_ok_and(|slot| slot < self.occurrence_role_count)
            .then(|| OccurrenceRoleBindingId::from_index(slot))
            .flatten()
            .map(|id| PreparedOccurrenceRole {
                prototype: self.prototype,
                key: self.key,
                id,
            })
    }
    /// Re-checks that a handle's prototype and document key match this prepared set. Every typed `verify_*` entry calls
    /// this before its slot-bound check.
    pub(crate) fn verify(
        &self,
        prototype: Option<DocumentSchemaPrototypeId>,
        key: DocumentId,
    ) -> Result<(), DataError> {
        if prototype == Some(self.prototype) && self.key == key {
            Ok(())
        } else {
            Err(DataError::InvalidDocument)
        }
    }

    /// The [`Self::verify`] law applied to node-kind slots: the handle must name a slot below this set's node-kind
    /// count.
    pub(crate) fn verify_node_kind(
        &self,
        value: PreparedNodeKind,
        prototype: Option<DocumentSchemaPrototypeId>,
        key: DocumentId,
    ) -> Result<NodeKindBindingId, DataError> {
        self.verify(prototype, key)?;
        if value.prototype == self.prototype
            && value.key == key
            && u32::try_from(value.id.index()).is_ok_and(|index| index < self.node_kind_count)
        {
            Ok(value.id)
        } else {
            Err(DataError::InvalidDocument)
        }
    }

    /// The [`Self::verify_node_kind`] law applied to occurrence-role slots.
    pub(crate) fn verify_occurrence_role(
        &self,
        value: PreparedOccurrenceRole,
        prototype: Option<DocumentSchemaPrototypeId>,
        key: DocumentId,
    ) -> Result<OccurrenceRoleBindingId, DataError> {
        self.verify(prototype, key)?;
        if value.prototype == self.prototype
            && value.key == key
            && u32::try_from(value.id.index()).is_ok_and(|index| index < self.occurrence_role_count)
        {
            Ok(value.id)
        } else {
            Err(DataError::InvalidDocument)
        }
    }
}
