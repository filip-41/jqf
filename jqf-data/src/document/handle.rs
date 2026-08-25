//! Process-scoped document handles.
//!
//! [`DocumentId`] is a process-local opaque nonzero counter starting at 1; every published document is immutable, so
//! the id alone is the complete identity (no revision dimension exists — a successor would mint a fresh document).
//! The `*Handle` types pair a document id with a dense document-local `u32` id, so a handle is valid only for the exact
//! document it was minted under. The `NonZeroU64` representation makes the zero id unrepresentable.

use core::num::NonZeroU64;
use core::sync::atomic::{AtomicU64, Ordering};

static NEXT_DOCUMENT_ID: AtomicU64 = AtomicU64::new(1);

/// Process-local id of one document.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(transparent)]
pub struct DocumentId(NonZeroU64);

impl DocumentId {
    /// Atomically bumps the process-local counter for one fresh document; `None` only when the counter exhausts.
    pub(crate) fn try_fresh() -> Option<Self> {
        // fetch_update refuses to advance at `u64::MAX`: a wrapping fetch_add would keep issuing ids that fold back
        // onto previously issued ones (an ABA reuse of a document identity). Exhaustion is terminal instead. Same shape
        // as `fresh_builder_generation`.
        NEXT_DOCUMENT_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                (value != 0 && value != u64::MAX).then_some(value + 1)
            })
            .ok()
            .and_then(NonZeroU64::new)
            .map(Self)
    }

    /// The nonzero id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

macro_rules! local_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u32);

        impl $name {
            /// Creates a dense document-local identity from a portable index.
            #[must_use]
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the preceding bound check proves the index fits u32"
            )]
            pub const fn try_from_index(index: usize) -> Option<Self> {
                if index <= u32::MAX as usize {
                    Some(Self(index as u32))
                } else {
                    None
                }
            }

            /// Returns the document-local numeric representation.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0 as u64
            }

            pub(crate) const fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

local_id!(NodeId, "Document-local logical node identity.");
local_id!(OccurrenceId, "Document-local topology occurrence identity.");
local_id!(FactId, "Document-local attached-fact identity.");

impl NodeId {
    /// The dense u32 packing used by the arena emission.
    pub(crate) const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Document-scoped logical node handle.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NodeHandle {
    key: DocumentId,
    local: NodeId,
}

impl NodeHandle {
    pub(crate) const fn new(key: DocumentId, local: NodeId) -> Self {
        Self { key, local }
    }

    /// Returns the immutable document identity.
    #[must_use]
    pub const fn document(self) -> DocumentId {
        self.key
    }

    /// Returns the document-local identity.
    #[must_use]
    pub const fn local(self) -> NodeId {
        self.local
    }
}
