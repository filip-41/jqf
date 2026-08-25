//! Authoritative decoded document products and checked located results.
//!
//! [`DocumentProduct`] holds one document. [`EncodeItem`] is owned or located. Sibling: [`crate::access`].

use jqf_data::{DataError, Document, NodeHandle, Value};
use jqf_resource::ResourceContext;
use jqf_source::ResolvedSource;

use crate::schedule::SelectionOrigin;
use crate::{CodecError, CodecFailureKind};

/// One authoritative decoded document.
#[derive(Debug)]
pub struct DocumentProduct<'source> {
    document: Document<'source>,
}

impl<'source> DocumentProduct<'source> {
    /// Anchors one complete authoritative document.
    pub fn try_new(document: Document<'source>, _resources: &ResourceContext<'_>) -> Result<Self, CodecError> {
        Ok(Self { document })
    }

    /// Borrows the authoritative document.
    #[must_use]
    pub const fn document(&self) -> &Document<'source> {
        &self.document
    }

    /// Retains another cheap owner of the same document.
    pub fn try_clone(&self) -> Result<Self, CodecError> {
        Ok(Self {
            document: self.document.try_clone().map_err(|_| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "document product clone",
                })
            })?,
        })
    }

    /// Installs borrowed backing already proved by this access session without repeating generic digest and
    /// retained-reference validation.
    ///
    /// # Safety
    ///
    /// The caller must be the codec access session that continuously owns the exact immutable `source` authority used
    /// to construct the document's canonical seal, parse every retained source reference, and finalize the document. No
    /// different or mutably aliased bytes may have existed between those operations and this call.
    #[doc(hidden)]
    pub unsafe fn attach_borrowed_source_from_access_session<'attached>(
        self,
        source: ResolvedSource<'attached>,
        resources: &ResourceContext<'_>,
    ) -> Result<DocumentProduct<'attached>, CodecError>
    where
        'source: 'attached,
    {
        // SAFETY: forwarded unchanged from this method's caller contract.
        let document = unsafe {
            self.document
                .with_borrowed_source_from_bound_authority(source, resources)
        }
        .map_err(map_source_attachment_error)?;
        Ok(DocumentProduct { document })
    }
}

fn map_source_attachment_error(error: DataError) -> CodecError {
    match error {
        DataError::Resource(error) => error.into(),
        DataError::Control(error) => error.into(),
        _ => CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "decoded document source attachment",
        }),
    }
}

/// A validated node retained with its authoritative document.
#[derive(Debug)]
pub struct LocatedProduct<'source> {
    product: DocumentProduct<'source>,
    node: NodeHandle,
}

/// Exact selection result retaining complete document authority in every case.
#[derive(Debug)]
pub struct LocatedOutcome<'source> {
    product: DocumentProduct<'source>,
    result: ExactSelectionRecord,
}

/// One exact-path observation interpreted by engine-owned semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExactSelectionRecord {
    /// The path resolved to a checked semantic node.
    Node {
        /// Checked selected node.
        node: NodeHandle,
        /// Authored emission identity.
        origin: SelectionOrigin,
    },
    /// The path was absent at this zero-based step.
    Missing {
        /// Zero-based failing path step.
        step_index: usize,
        /// Authored emission identity.
        origin: SelectionOrigin,
    },
    /// A step addressed the wrong payload-transparent semantic category.
    TypeMismatch {
        /// Zero-based failing path step.
        step_index: usize,
        /// Observed payload-transparent value kind.
        actual_type: jqf_data::ValueKind,
        /// Authored emission identity.
        origin: SelectionOrigin,
        /// The markup accessor hint (a missed member step whose name matches an attribute or the element's own name),
        /// rendered into the mismatch message by the engine. `None` for every format-neutral site — only the XML/HTML
        /// locate arms carry it, which is what keeps the pushed-down route's message byte-identical to the engine
        /// floor's (both hint, or neither does).
        hint: Option<alloc::string::String>,
    },
}

impl<'source> LocatedOutcome<'source> {
    /// Constructs one checked exact observation retaining complete authority.
    pub fn try_new(product: &DocumentProduct<'source>, result: ExactSelectionRecord) -> Result<Self, CodecError> {
        if let ExactSelectionRecord::Node { node, .. } = &result {
            // One validation spelling for checked-node-with-authority: the same constructor every direct Located
            // consumer uses.
            LocatedProduct::try_new(product, *node)?;
        }
        Ok(Self {
            product: product.try_clone()?,
            result,
        })
    }
    /// Borrows the complete authority retained by this observation.
    #[must_use]
    pub const fn product(&self) -> &DocumentProduct<'source> {
        &self.product
    }
    /// Returns the exact observation.
    #[must_use]
    pub fn result(&self) -> &ExactSelectionRecord {
        &self.result
    }
}

impl<'source> LocatedProduct<'source> {
    /// Validates `node` and cheaply retains the product.
    pub fn try_new(product: &DocumentProduct<'source>, node: NodeHandle) -> Result<Self, CodecError> {
        product
            .document()
            .resolve_node_handle(node)
            .map_err(|_| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
        Ok(Self {
            product: product.try_clone()?,
            node,
        })
    }

    /// Borrows the authoritative product.
    #[must_use]
    pub const fn product(&self) -> &DocumentProduct<'source> {
        &self.product
    }

    /// Returns the validated node.
    #[must_use]
    pub const fn node(&self) -> NodeHandle {
        self.node
    }

    /// Fallibly retains another checked cheap owner.
    pub fn try_clone(&self) -> Result<Self, CodecError> {
        Self::try_new(&self.product, self.node)
    }
}

/// Borrowed semantic authority supplied to one encoder session.
///
/// `PartialEq` is the identity comparison the encoder cursor's member cache keys on: two items are equal when they name
/// the same product and node (or the same owned value pointer), so the cache can tell one object from another even at
/// the same stack depth.
#[derive(Clone, Copy, Debug)]
pub enum EncodeItem<'item, 'source> {
    /// A node located in one authoritative source-aware document.
    Located {
        /// Complete authoritative document product.
        product: &'item DocumentProduct<'source>,
        /// Revision-scoped node to encode.
        node: NodeHandle,
    },
    /// An engine-owned semantic value without document authority.
    Owned(&'item Value),
}

/// Pointer identity: the same product reference names the same document and the same owned reference names the same
/// value, so equality is exact and allocation-free. Two items from different products (or different owned values) are
/// never equal even when they describe the same shape.
impl PartialEq for EncodeItem<'_, '_> {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Owned(left), Self::Owned(right)) => core::ptr::eq(*left, *right),
            (
                Self::Located {
                    product: left_product,
                    node: left_node,
                },
                Self::Located {
                    product: right_product,
                    node: right_node,
                },
            ) => core::ptr::eq(*left_product, *right_product) && left_node == right_node,
            _ => false,
        }
    }
}

impl<'item, 'source> EncodeItem<'item, 'source> {
    /// This item with every tag LAYER stripped, in both authorities.
    ///
    /// Encode navigation is payload-transparent, exactly as engine navigation is: a tagged array IS an array and its
    /// members are the payload's members. The two authorities spell a tag two ways and BOTH have to be seen through.
    /// The owned model wraps (`Value::Tagged`). A document holds an intrinsic tag as a fact ON the node when the format
    /// resolves one tag per value (YAML), but a format whose tags NEST and can tag a container (CBOR's uninterpreted
    /// tags) cannot say that in one node, so it builds a kindless tag-LAYER node owning its payload as its single
    /// occurrence — jqf-data's tag-layer law, descended here through
    /// [`Document::tag_payload`](jqf_data::Document::tag_payload), the same primitive the engine's navigation steps
    /// already use. A layer is KINDLESS: an encoder that stops at one asks it for a kind and gets a document error for
    /// a perfectly valid document.
    ///
    /// A CHAIN strips whole. Two nested tags publish ONE payload, so the layers are not counted one by one;
    /// [`crate::tag_layer`] reports the outermost, which is the one a native tag spelling would write.
    ///
    /// Reading the tag itself stays a separate, deliberate act ([`crate::tag_layer`]), so an encoder with a native tag
    /// spelling still sees it.
    ///
    /// # Errors
    ///
    /// Returns [`CodecFailureKind::InternalContractViolation`] when the item's own document cannot resolve it. The item
    /// was validated at construction, so this is a broken document and never a program's doing.
    pub fn untagged(self) -> Result<Self, CodecError> {
        match self {
            Self::Owned(value) => Ok(Self::Owned(value.untagged())),
            Self::Located { product, node } => {
                let document = product.document();
                let mut current = document.resolve_node_handle(node).map_err(|_| unresolvable_layer())?;
                while let Some(payload) = document.tag_payload(current).map_err(|_| unresolvable_layer())? {
                    current = payload;
                }
                let node = document.node_handle(current).map_err(|_| unresolvable_layer())?;
                Ok(Self::Located { product, node })
            }
        }
    }

    /// Constructs a located item after validating the node against its document.
    pub fn try_located(product: &'item DocumentProduct<'source>, node: NodeHandle) -> Result<Self, CodecError> {
        product
            .document()
            .resolve_node_handle(node)
            .map_err(|_| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
        Ok(Self::Located { product, node })
    }

    /// Constructs an item from an owned semantic value borrow.
    #[must_use]
    pub const fn owned(value: &'item Value) -> Self {
        Self::Owned(value)
    }
}

/// The failure of a tag-layer descent over an item its own document rejects.
fn unresolvable_layer() -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation {
        contract: "encode item tag-layer descent over its own document",
    })
}

#[cfg(test)]
mod tests {
    use jqf_resource::{ControlError, ResourceError};

    use super::*;

    #[test]
    fn source_attachment_preserves_resource_and_control_failures() {
        let resource = map_source_attachment_error(DataError::Resource(ResourceError::AccountingInvariantViolation));
        assert!(matches!(
            resource.kind(),
            CodecFailureKind::Resource(ResourceError::AccountingInvariantViolation)
        ));

        let control = map_source_attachment_error(DataError::Control(ControlError::DeadlineExceeded));
        assert!(matches!(
            control.kind(),
            CodecFailureKind::Control(ControlError::DeadlineExceeded)
        ));

        let contract = map_source_attachment_error(DataError::InvalidDocument);
        assert!(matches!(
            contract.kind(),
            CodecFailureKind::InternalContractViolation {
                contract: "decoded document source attachment"
            }
        ));
    }
}
