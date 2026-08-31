//! Immutable documents: nodes, topology, tags, facts, and retained source.
//!
//! One [`Document`] is one immutable document. A decoder builds it here. A caller reads it through handles, bounded
//! readers, and the count / element / lazy helpers. Format rules stay with the decoder.
//!
//! Owned values come out of [`crate::value`] only when you materialize. Source bytes come from `jqf-source`.

mod adjacency;
mod builder;
mod count;
mod coverage;
mod element;
mod fact;
mod handle;
mod lazy;
mod name;
mod owner;
mod publish;
mod reader;
mod schema;
mod storage;
mod text;
mod transients;

pub use builder::{
    AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode, AccountedTextStage,
    DocumentCapacity, PreparedSemanticNode,
};
pub use count::{
    CountCompare, CountDemand, CountFilter, CountLiteral, CountMember, CountRow, CountStep, CountTest, CountVerdict,
};
/// One slice range after negative bounds have already been resolved.
///
/// `None` is the container edge. Both ends are non-negative or open. A strictly negative bound is refused before it
/// reaches this type.
pub type SliceRange = (Option<i64>, Option<i64>);
pub use coverage::{
    AuthoritativeEmptyFamilies, BuilderCoverage, DiagnosticCoverage, DocumentCapability, DocumentCapabilityFamily,
    DocumentCoverage,
};
pub use element::{ElementDemand, ElementProbe, ElementRow, ElementVerdict, owned_probe_value};
pub use fact::{DocumentFact, FactKindId, FactPayload, FactPayloadView, FactRoleId, OccurrenceRoleId};
pub(crate) use fact::{DocumentNodeKindId, StoredDocumentFact, StoredFactPayload};
pub use handle::{DocumentId, FactId, NodeHandle, NodeId, OccurrenceId};
pub use lazy::LazySpanMaterializer;
pub use name::ExpandedName;
pub use owner::LocalOwnerRef;
pub use publish::{AccountedDocumentFinalizer, DocumentFinalizationPoll};
pub use reader::{
    BatchLimit, DocumentNodeView, FactBatch, FactReader, NodeBatch, NodeIter, OccurrenceBatch, OccurrenceIter,
    OccurrenceView, ReaderCompletion, ReaderDemand, ReaderPoll, TopologyBatch, TopologyReader,
    UNBOUNDED_READER_REPLENISH, unbounded_batch_limit,
};
pub use schema::{
    DocumentSchemaPrototype, DocumentSchemaRecipe, FactKindBindingId, FactRoleBindingId, PreparedDocumentSchema,
    PreparedNodeKind, PreparedOccurrenceRole,
};
#[cfg(feature = "benchmark-internals")]
#[doc(hidden)]
pub use storage::DocumentStorageLayoutStats;
pub use storage::{ContainerSpanKind, DataError, DataErrorClass, Document, IntrinsicTag, IntrinsicTagSemantics};
pub use text::{
    DocumentSourceBinding, DocumentSourceBindingPoll, DocumentSourceBindingStage, DocumentSourceText, DocumentTextId,
    DocumentTextStorageStats,
};
pub use transients::DocumentTransients;

#[cfg(feature = "benchmark-internals")]
pub(crate) use schema::SchemaExecution;
pub(crate) use schema::{
    AccountedSchemaBuilder, DialectBindingId, DocumentSchema, DocumentSchemaPrototypeId, FormatBindingId,
    NodeKindBindingId, OccurrenceRoleBindingId,
};
pub(crate) use storage::{
    AccountedLocalDateTime, AccountedLocalTime, AccountedOffsetDateTime, ArrayItems, DocumentStorage,
    DocumentStorageOwner, IntrinsicTagRef, NodeRecord, NodeSemantic, ObjectEntries, OccurrenceRecord, PlacedSemantic,
    SMALL_OBJECT_WINNER_LIMIT, StoredOccurrenceKey, StoredSemanticNode, WidePayload, WidePayloadId, place_semantic,
};
#[cfg(feature = "benchmark-internals")]
pub(crate) use text::SourceSnapshotSeal;
pub(crate) use text::{DocumentTextStorage, TextRef, ValidatedSourceBacking, fresh_builder_generation};
