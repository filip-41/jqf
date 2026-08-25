//! Who owns a topology or fact item.
//!
//! [`LocalOwnerRef`] names a document-local owner.

use super::{NodeId, OccurrenceId};

/// Document-local owner of topology or facts.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LocalOwnerRef {
    /// The complete document root edge.
    DocumentRoot,
    /// One logical node.
    Node(NodeId),
    /// One ordered topology occurrence.
    Occurrence(OccurrenceId),
}
