//! Attached facts: ordered portable metadata on a document.
//!
//! [`FactPayload`] is the payload. [`StoredDocumentFact`] is one record the document owns. [`FactPayloadView`] is the
//! borrowed read. Facts cannot change a node's meaning.

use alloc::{boxed::Box, string::String, vec::Vec};
use core::fmt;
use jqf_resource::{ResourceContext, ResourceError};
use jqf_source::Span;

use crate::identity::{IdentityText, try_copy_str};
use crate::{Decimal, Integer};

use super::{DataError, FactId, FactKindBindingId, FactRoleBindingId, LocalOwnerRef};

macro_rules! namespaced_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(IdentityText);

        impl $name {
            /// Creates a nonempty namespaced identity.
            pub fn try_new(value: &str) -> Result<Self, NamespacedIdError> {
                crate::identity::validate(value).map_err(NamespacedIdError::from)?;
                IdentityText::try_new(value)
                    .map(Self)
                    .map_err(|_| NamespacedIdError::Allocation)
            }

            /// Returns the canonical text identity.
            #[must_use]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }

            /// Copies this exact identity into request-accounted storage.
            pub fn try_clone_accounted(&self) -> Result<Self, ResourceError> {
                IdentityText::try_new(self.as_str()).map(Self)
            }

            /// Wraps an identity the schema admission that interned it already ran through the identity grammar.
            pub(crate) fn from_accounted(value: IdentityText) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

namespaced_id!(FactKindId, "Namespaced attached-fact schema identity.");
namespaced_id!(FactRoleId, "Namespaced logical attached-fact role.");
namespaced_id!(DocumentNodeKindId, "Namespaced logical document-node kind.");
namespaced_id!(OccurrenceRoleId, "Namespaced topology occurrence role.");

/// Invalid namespaced identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespacedIdError {
    /// The identity was empty.
    Empty,
    /// The identity contained ASCII control or whitespace bytes.
    InvalidCharacter,
    /// Allocation failed while retaining the identity.
    Allocation,
}

impl fmt::Display for NamespacedIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "namespaced identity must not be empty",
            Self::InvalidCharacter => "namespaced identity must not contain ASCII whitespace or control bytes",
            Self::Allocation => "namespaced identity allocation failed",
        })
    }
}

impl core::error::Error for NamespacedIdError {}

impl From<crate::identity::IdentityError> for NamespacedIdError {
    fn from(value: crate::identity::IdentityError) -> Self {
        match value {
            crate::identity::IdentityError::Empty => Self::Empty,
            crate::identity::IdentityError::InvalidCharacter => Self::InvalidCharacter,
        }
    }
}

/// Fact payload. Not a [`crate::Value`].
#[derive(Clone, Debug)]
pub enum FactPayload {
    /// Null payload.
    Null,
    /// Boolean payload.
    Bool(bool),
    /// Exact integer payload.
    Integer(Integer),
    /// Exact decimal payload.
    Decimal(Decimal),
    /// UTF-8 text payload.
    Text(String),
    /// Interpreted bytes.
    Bytes(Vec<u8>),
    /// Ordered nested payloads.
    List(Vec<FactPayload>),
    /// Ordered unique-key map payload. Uniqueness is enforced when the payload moves into accounted storage, at
    /// `StoredFactPayload::try_accounted_copy`.
    Map(Vec<(String, FactPayload)>),
    /// Schema-owned bytes preserved without interpretation.
    OpaqueBytes(Vec<u8>),
}

/// Stored fact payload. Built from a plain [`FactPayload`] by the iterative copy. The input type is [`FactPayload`].
#[derive(Debug)]
pub(crate) struct StoredFactPayload(Box<AccountedFactPayload>);

/// Flat arena form of a fact payload: node, list-item, and map-item vectors linked by index, with one root node.
#[derive(Debug)]
pub(crate) struct AccountedFactPayload {
    nodes: Vec<AccountedFactNode>,
    list_items: Vec<u32>,
    map_items: Vec<AccountedFactMapEntry>,
    root: u32,
}

#[derive(Debug)]
enum AccountedFactNode {
    Null,
    Bool(bool),
    Integer(String),
    Decimal { coefficient: String, scale: i64 },
    Text(String),
    Bytes(Vec<u8>),
    List { start: usize, len: usize },
    Map { start: usize, len: usize },
    OpaqueBytes(Vec<u8>),
}

#[derive(Debug)]
struct AccountedFactMapEntry {
    key: String,
    value: u32,
}

#[derive(Clone, Copy)]
enum FactCopyDestination {
    Root,
    List(usize),
    Map(usize),
}

enum FactCopyTask<'payload> {
    Visit {
        payload: &'payload FactPayload,
        destination: FactCopyDestination,
    },
    LeaveContainer,
}

impl StoredFactPayload {
    pub(crate) fn try_accounted_copy(source: &FactPayload, resources: &ResourceContext<'_>) -> Result<Self, DataError> {
        // Shallow fast path: a scalar payload copies into ONE node with no task machine and no nesting ledger — the
        // iterative machinery below exists for arbitrarily nested payloads, and most attached facts (XML
        // name/attrs/content, comment text) are scalars, so the deep machine's two Working vecs are pure overhead
        // there.
        if let Some(node) = scalar_node(source)? {
            let mut output = AccountedFactPayload {
                nodes: Vec::new(),
                list_items: Vec::new(),
                map_items: Vec::new(),
                root: 0,
            };
            output.nodes.push(node);
            return Ok(Self(Box::new(output)));
        }
        Self::try_accounted_copy_deep(source, resources)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one iterative task machine keeps arbitrarily nested payload copying and depth accounting non-recursive"
    )]
    /// Copy a [`FactPayload`] into arena storage without recursion. `InvalidDocument` on a duplicate key or an empty
    /// nesting stack.
    fn try_accounted_copy_deep(source: &FactPayload, resources: &ResourceContext<'_>) -> Result<Self, DataError> {
        let mut output = AccountedFactPayload {
            nodes: Vec::new(),
            list_items: Vec::new(),
            map_items: Vec::new(),
            root: u32::MAX,
        };
        let mut tasks = Vec::new();
        let mut nesting = Vec::new();
        tasks.push(FactCopyTask::Visit {
            payload: source,
            destination: FactCopyDestination::Root,
        });
        while let Some(task) = tasks.pop() {
            let FactCopyTask::Visit { payload, destination } = task else {
                let _guard = nesting.pop().ok_or(DataError::InvalidDocument)?;
                continue;
            };
            let node_index = u32::try_from(output.nodes.len()).map_err(|_| ResourceError::ArithmeticOverflow)?;
            match destination {
                FactCopyDestination::Root => output.root = node_index,
                FactCopyDestination::List(index) => {
                    *output
                        .list_items
                        .as_mut_slice()
                        .get_mut(index)
                        .ok_or(DataError::InvalidDocument)? = node_index;
                }
                FactCopyDestination::Map(index) => {
                    output
                        .map_items
                        .as_mut_slice()
                        .get_mut(index)
                        .ok_or(DataError::InvalidDocument)?
                        .value = node_index;
                }
            }
            let node = match payload {
                FactPayload::List(values) => {
                    let start = output.list_items.len();
                    for _ in values {
                        output.list_items.push(0);
                    }
                    nesting.push(resources.enter_nesting()?);
                    tasks.push(FactCopyTask::LeaveContainer);
                    for (index, value) in values.iter().enumerate().rev() {
                        tasks.push(FactCopyTask::Visit {
                            payload: value,
                            destination: FactCopyDestination::List(start + index),
                        });
                    }
                    AccountedFactNode::List {
                        start,
                        len: values.len(),
                    }
                }
                FactPayload::Map(values) => {
                    // Duplicate-key rejection is O(n²) over the map's own entries; every in-tree map fact is
                    // single-entry (the engine's attribute facts), so the scan is O(1) today. If a codec ever attaches
                    // a large map, sort or hash the keys once instead of re-scanning the tail per entry.
                    for (index, (key, _)) in values.iter().enumerate() {
                        if values[index + 1..].iter().any(|(candidate, _)| candidate == key) {
                            return Err(DataError::InvalidDocument);
                        }
                    }
                    let start = output.map_items.len();
                    for (key, _) in values {
                        output.map_items.push(AccountedFactMapEntry {
                            key: try_copy_str(key)?,
                            value: 0,
                        });
                    }
                    nesting.push(resources.enter_nesting()?);
                    tasks.push(FactCopyTask::LeaveContainer);
                    for (index, (_, value)) in values.iter().enumerate().rev() {
                        tasks.push(FactCopyTask::Visit {
                            payload: value,
                            destination: FactCopyDestination::Map(start + index),
                        });
                    }
                    AccountedFactNode::Map {
                        start,
                        len: values.len(),
                    }
                }
                // `scalar_node` is total over every non-container variant, so the `ok_or` arm is unreachable; it stays
                // an error, not a panic, to keep the machine total.
                scalar => scalar_node(scalar)?.ok_or(DataError::InvalidDocument)?,
            };
            output.nodes.push(node);
        }
        if !nesting.is_empty() || output.root == u32::MAX {
            return Err(DataError::InvalidDocument);
        }
        Ok(Self(Box::new(output)))
    }

    fn view(&self) -> FactPayloadView<'_> {
        self.0.view(self.0.root)
    }
}

impl AccountedFactPayload {
    fn view(&self, node: u32) -> FactPayloadView<'_> {
        match &self.nodes.as_slice()[node as usize] {
            AccountedFactNode::Null => FactPayloadView::Null,
            AccountedFactNode::Bool(value) => FactPayloadView::Bool(*value),
            AccountedFactNode::Integer(value) => FactPayloadView::Integer(value.as_str()),
            AccountedFactNode::Decimal { coefficient, scale } => FactPayloadView::Decimal {
                coefficient: coefficient.as_str(),
                scale: *scale,
            },
            AccountedFactNode::Text(value) => FactPayloadView::Text(value.as_str()),
            AccountedFactNode::Bytes(value) => FactPayloadView::Bytes(value.as_slice()),
            AccountedFactNode::List { start, len } => FactPayloadView::List(FactPayloadList {
                payload: self,
                start: *start,
                len: *len,
            }),
            AccountedFactNode::Map { start, len } => FactPayloadView::Map(FactPayloadMap {
                payload: self,
                start: *start,
                len: *len,
            }),
            AccountedFactNode::OpaqueBytes(value) => FactPayloadView::OpaqueBytes(value.as_slice()),
        }
    }
}

/// Borrowed semantic view of one fact payload, independent of physical storage.
#[derive(Clone, Copy)]
pub enum FactPayloadView<'payload> {
    /// Null payload.
    Null,
    /// Boolean payload.
    Bool(bool),
    /// Canonical signed integer text.
    Integer(&'payload str),
    /// Exact decimal coefficient and scale.
    Decimal {
        /// Canonical signed coefficient text.
        coefficient: &'payload str,
        /// Base-ten scale.
        scale: i64,
    },
    /// UTF-8 text.
    Text(&'payload str),
    /// Interpreted bytes.
    Bytes(&'payload [u8]),
    /// Ordered nested payloads.
    List(FactPayloadList<'payload>),
    /// Ordered unique-key map.
    Map(FactPayloadMap<'payload>),
    /// Schema-owned opaque bytes.
    OpaqueBytes(&'payload [u8]),
}

/// Borrowed iterable fact list: one contiguous run of the payload's item table.
#[derive(Clone, Copy)]
pub struct FactPayloadList<'payload> {
    payload: &'payload AccountedFactPayload,
    start: usize,
    len: usize,
}

impl<'payload> FactPayloadList<'payload> {
    /// Returns the number of entries.
    #[must_use]
    pub fn len(self) -> usize {
        self.len
    }
    /// Returns whether there are no entries.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// Iterates semantic payload views.
    pub fn iter(self) -> impl Iterator<Item = FactPayloadView<'payload>> {
        self.payload.list_items.as_slice()[self.start..self.start + self.len]
            .iter()
            .map(move |item| self.payload.view(*item))
    }
}

/// Borrowed iterable fact map: one contiguous run of the payload's entry table.
#[derive(Clone, Copy)]
pub struct FactPayloadMap<'payload> {
    payload: &'payload AccountedFactPayload,
    start: usize,
    len: usize,
}

impl<'payload> FactPayloadMap<'payload> {
    /// Returns the number of entries.
    #[must_use]
    pub fn len(self) -> usize {
        self.len
    }
    /// Returns whether there are no entries.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    /// Iterates key and semantic payload views.
    pub fn iter(self) -> impl Iterator<Item = (&'payload str, FactPayloadView<'payload>)> {
        self.payload.map_items.as_slice()[self.start..self.start + self.len]
            .iter()
            .map(move |entry| (entry.key.as_str(), self.payload.view(entry.value)))
    }
}

/// The scalar payload arms shared by the shallow fast path and the deep copy machine; `None` for the container
/// variants.
fn scalar_node(payload: &FactPayload) -> Result<Option<AccountedFactNode>, DataError> {
    Ok(Some(match payload {
        FactPayload::Null => AccountedFactNode::Null,
        FactPayload::Bool(value) => AccountedFactNode::Bool(*value),
        FactPayload::Integer(value) => AccountedFactNode::Integer(try_copy_str(value.as_str())?),
        FactPayload::Decimal(value) => AccountedFactNode::Decimal {
            coefficient: try_copy_str(value.coefficient().as_str())?,
            scale: value.scale(),
        },
        FactPayload::Text(value) => AccountedFactNode::Text(try_copy_str(value)?),
        FactPayload::Bytes(value) => AccountedFactNode::Bytes(copy_bytes(value)?),
        FactPayload::OpaqueBytes(value) => AccountedFactNode::OpaqueBytes(copy_bytes(value)?),
        FactPayload::List(_) | FactPayload::Map(_) => return Ok(None),
    }))
}

fn copy_bytes(value: &[u8]) -> Result<Vec<u8>, ResourceError> {
    let mut output = Vec::new();
    output.try_reserve_exact(value.len())?;
    output.extend_from_slice(value);
    Ok(output)
}

/// One immutable ordered attached-fact record.
#[derive(Debug)]
pub(crate) struct StoredDocumentFact {
    id: FactId,
    pub(crate) owner: LocalOwnerRef,
    role: FactRoleBindingId,
    kind: FactKindBindingId,
    schema_version: u32,
    /// Authored source range this fact addresses (an XML attribute's quoted value bytes). Node-keyed `authored_spans`
    /// cannot hold it: attributes are facts, not nodes.
    source_span: Option<Span>,
    payload: StoredFactPayload,
}

impl StoredDocumentFact {
    pub(crate) fn new(
        id: FactId,
        owner: LocalOwnerRef,
        role: FactRoleBindingId,
        kind: FactKindBindingId,
        schema_version: u32,
        payload: StoredFactPayload,
    ) -> Self {
        Self {
            id,
            owner,
            role,
            kind,
            schema_version,
            source_span: None,
            payload,
        }
    }

    pub(crate) const fn role_binding(&self) -> FactRoleBindingId {
        self.role
    }

    pub(crate) const fn kind_binding(&self) -> FactKindBindingId {
        self.kind
    }

    pub(crate) fn payload_view(&self) -> FactPayloadView<'_> {
        self.payload.view()
    }

    pub(crate) const fn source_span(&self) -> Option<Span> {
        self.source_span
    }

    pub(crate) fn set_source_span(&mut self, span: Span) {
        self.source_span = Some(span);
    }
}

/// One attached fact on a document.
#[derive(Clone, Copy)]
pub struct DocumentFact<'document> {
    pub(crate) stored: &'document StoredDocumentFact,
    pub(crate) role: &'document FactRoleId,
    pub(crate) kind: &'document FactKindId,
}

impl<'document> DocumentFact<'document> {
    /// This fact's id.
    #[must_use]
    pub const fn id(&self) -> FactId {
        self.stored.id
    }

    /// Who this fact is attached to.
    #[must_use]
    pub const fn owner(&self) -> LocalOwnerRef {
        self.stored.owner
    }

    /// Attachment role.
    #[must_use]
    pub const fn role(&self) -> &'document FactRoleId {
        self.role
    }

    /// Compact interned role id for this document's schema.
    #[must_use]
    pub const fn role_binding(&self) -> FactRoleBindingId {
        self.stored.role_binding()
    }

    /// Returns the fact schema identity.
    #[must_use]
    pub const fn kind(&self) -> &'document FactKindId {
        self.kind
    }

    /// Compact interned kind id for this document's schema.
    #[must_use]
    pub const fn kind_binding(&self) -> FactKindBindingId {
        self.stored.kind_binding()
    }

    /// Returns the schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.stored.schema_version
    }

    /// Returns the payload.
    #[must_use]
    pub fn payload(&self) -> FactPayloadView<'_> {
        self.stored.payload.view()
    }

    /// Authored source range this fact addresses, when the codec bound one.
    ///
    /// Markup attributes store the quoted-value byte range here so `--edit` can splice without a dedicated Null node
    /// per attribute.
    #[must_use]
    pub const fn source_span(&self) -> Option<Span> {
        self.stored.source_span()
    }
}
