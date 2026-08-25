//! JSON-only encode cursor descent: the stack and member cache that only this codec's encoder uses. Format-neutral
//! [`EncodeInput`] carries the root item alone; every other codec recurses through its own value walk after one
//! [`EncodeInput::item`] call.

use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind, EncodeItem};
use jqf_data::Value;
use jqf_resource::ResourceError;

/// One object member resolved at the cursor's current position.
///
/// The JSON encoder reads a member's key and then descends to its value for the SAME index, and both calls used to
/// re-resolve the winner entry — including its key text — independently; the cache serves both from one resolution.
///
/// The cache is sound because `current` pins the exact item the member belongs to (the stack DEPTH alone cannot — two
/// different objects can sit at the same depth, so a depth-only key would serve one object's member `i` for another's),
/// the document data is immutable, and re-resolving the same `(current, index)` always yields the same entry; a hit is
/// a pure read and a miss merely re-resolves. The cache is never cleared explicitly — any descent or exit changes
/// `current` and misses.
struct MemberResolution<'item, 'source> {
    current: EncodeItem<'item, 'source>,
    index: usize,
    key: &'item str,
    child: EncodeItem<'item, 'source>,
}

const INLINE_FRAMES: usize = 16;

/// Descent stack: 16 inline frames, heap only past that.
struct DescentStack<'item, 'source> {
    inline: [EncodeItem<'item, 'source>; INLINE_FRAMES],
    len: usize,
    heap: Vec<EncodeItem<'item, 'source>>,
}

impl<'item, 'source> DescentStack<'item, 'source> {
    fn seeded(root: EncodeItem<'item, 'source>) -> Self {
        Self {
            inline: [root; INLINE_FRAMES],
            len: 1,
            heap: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn last(&self) -> Option<EncodeItem<'item, 'source>> {
        if self.heap.is_empty() {
            self.len
                .checked_sub(1)
                .and_then(|index| self.inline.get(index).copied())
        } else {
            self.heap.last().copied()
        }
    }

    fn push(&mut self, child: EncodeItem<'item, 'source>) -> Result<(), CodecError> {
        if self.heap.is_empty() && self.len < INLINE_FRAMES {
            self.inline[self.len] = child;
            self.len += 1;
            return Ok(());
        }
        if self.heap.is_empty() {
            self.heap
                .try_reserve(self.len.saturating_add(1))
                .map_err(ResourceError::from)?;
            self.heap.extend_from_slice(&self.inline[..self.len]);
        } else {
            self.heap.try_reserve(1).map_err(ResourceError::from)?;
        }
        self.heap.push(child);
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) {
        if !self.heap.is_empty() {
            self.heap.pop();
        }
        self.len = self.len.saturating_sub(1);
    }
}

/// Descent stack for the JSON encoder's constant-time container navigation.
pub(crate) struct JsonEncodeCursor<'item, 'source> {
    root: EncodeItem<'item, 'source>,
    values: Option<DescentStack<'item, 'source>>,
    member: Option<MemberResolution<'item, 'source>>,
}

impl<'item, 'source> JsonEncodeCursor<'item, 'source> {
    pub(crate) fn try_new(item: EncodeItem<'item, 'source>) -> Self {
        Self {
            root: item,
            values: None,
            member: None,
        }
    }

    /// Seeds 16 inline frames on first descent. Deeper growth is fallible on [`Self::push_descent`].
    fn ensure_values(&mut self) {
        if self.values.is_none() {
            self.values = Some(DescentStack::seeded(self.root));
        }
    }

    /// Descends onto one child, growing the stack past its seeded capacity on the fallible path: nesting is bounded by
    /// `max_nesting_depth`, but the seed reserves at most 16 frames, so a deeper document reallocates and a refused
    /// reservation must surface as a codec failure.
    #[inline]
    fn push_descent(&mut self, child: EncodeItem<'item, 'source>) -> Result<(), CodecError> {
        self.ensure_values();
        let values = self.values.as_mut().ok_or_else(|| {
            CodecError::new(CodecFailureKind::InternalContractViolation {
                contract: "encoder cursor descent stack",
            })
        })?;
        values.push(child)
    }

    #[inline]
    fn current(&self) -> Result<EncodeItem<'item, 'source>, CodecError> {
        match &self.values {
            Some(values) => values.last().ok_or_else(|| {
                CodecError::new(CodecFailureKind::InternalContractViolation {
                    contract: "encoder cursor root",
                })
            }),
            None => Ok(self.root),
        }
    }

    #[inline]
    fn enter_array(&mut self, index: usize) -> Result<(), CodecError> {
        let child = match self.current()?.untagged()? {
            EncodeItem::Owned(Value::Array(array)) => {
                array.get(index).map(EncodeItem::Owned).ok_or_else(cursor_value_error)?
            }
            EncodeItem::Located { product, node } => {
                let value = product.document().value_view(node).map_err(data_error)?;
                let child = value
                    .array()
                    .map_err(data_error)?
                    .ok_or_else(cursor_value_error)?
                    .get(index)
                    .ok_or_else(cursor_value_error)?;
                let child = product.document().node_handle(child.node()).map_err(data_error)?;
                EncodeItem::Located { product, node: child }
            }
            EncodeItem::Owned(_) => return Err(cursor_value_error()),
        };
        self.push_descent(child)
    }

    #[inline]
    fn enter_object(&mut self, index: usize) -> Result<(), CodecError> {
        let (_, child) = self.object_member(index)?;
        self.push_descent(child)
    }

    #[inline]
    fn object_key(&mut self, index: usize) -> Result<&'item str, CodecError> {
        let (key, _) = self.object_member(index)?;
        Ok(key)
    }

    /// Resolves one object member's key and value item, serving the second of an adjacent `object_key`/`enter_object`
    /// pair from the cache built by the first. See [`MemberResolution`] for the soundness argument.
    #[inline]
    fn object_member(&mut self, index: usize) -> Result<(&'item str, EncodeItem<'item, 'source>), CodecError> {
        let current = self.current()?;
        if let Some(member) = &self.member
            && member.current == current
            && member.index == index
        {
            return Ok((member.key, member.child));
        }
        let (key, child) = Self::resolve_object_member(current, index)?;
        self.member = Some(MemberResolution {
            current,
            index,
            key,
            child,
        });
        Ok((key, child))
    }

    /// Resolves one object member at `current` (the item the member belongs to): its key text and its value item (the
    /// untagged value node, exactly as the pre-dedup `object_key`/`enter_object` pair descended to it).
    #[inline]
    fn resolve_object_member(
        current: EncodeItem<'item, 'source>,
        index: usize,
    ) -> Result<(&'item str, EncodeItem<'item, 'source>), CodecError> {
        match current.untagged()? {
            EncodeItem::Owned(Value::Object(object)) => {
                let entry = object.get_index(index).ok_or_else(cursor_value_error)?;
                Ok((entry.key(), EncodeItem::Owned(entry.value())))
            }
            EncodeItem::Located { product, node } => {
                let entry = product
                    .document()
                    .value_view(node)
                    .map_err(data_error)?
                    .object()
                    .map_err(data_error)?
                    .ok_or_else(cursor_value_error)?
                    .get_index(index)
                    .map_err(data_error)?
                    .ok_or_else(cursor_value_error)?;
                let child = product
                    .document()
                    .node_handle(entry.value().node())
                    .map_err(data_error)?;
                Ok((entry.key(), EncodeItem::Located { product, node: child }))
            }
            EncodeItem::Owned(_) => Err(cursor_value_error()),
        }
    }

    #[inline]
    fn exit(&mut self) -> Result<(), CodecError> {
        match &mut self.values {
            Some(values) if values.len() > 1 => {
                values.pop();
                Ok(())
            }
            // No descent stack at all, or only the root left on it: exiting above the root is the same cursor violation
            // as before.
            _ => Err(cursor_value_error()),
        }
    }
}

/// Mutable traversal authority for the JSON encoder's constant-time descent.
pub(crate) struct JsonEncodeInput<'cursor, 'item, 'source>(&'cursor mut JsonEncodeCursor<'item, 'source>);

impl<'cursor, 'item, 'source> JsonEncodeInput<'cursor, 'item, 'source> {
    pub(crate) fn new(cursor: &'cursor mut JsonEncodeCursor<'item, 'source>) -> Self {
        Self(cursor)
    }
}

impl<'item, 'source> JsonEncodeInput<'_, 'item, 'source> {
    /// Returns the authoritative current located-or-owned item.
    #[inline]
    pub(crate) fn item(&self) -> Result<EncodeItem<'item, 'source>, CodecError> {
        self.0.current()
    }

    /// Descends to one array child in constant time.
    #[inline]
    pub(crate) fn enter_array(&mut self, index: usize) -> Result<(), CodecError> {
        self.0.enter_array(index)
    }

    /// Descends to one object value in constant time.
    #[inline]
    pub(crate) fn enter_object(&mut self, index: usize) -> Result<(), CodecError> {
        self.0.enter_object(index)
    }

    /// Returns one current object key without descending to its value.
    #[inline]
    pub(crate) fn object_key(&mut self, index: usize) -> Result<&'item str, CodecError> {
        self.0.object_key(index)
    }

    /// Returns to the parent container after finishing one child value.
    #[inline]
    pub(crate) fn exit(&mut self) -> Result<(), CodecError> {
        self.0.exit()
    }
}

fn data_error(_: jqf_data::DataError) -> CodecError {
    cursor_value_error()
}

fn cursor_value_error() -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation {
        contract: "encoder cursor authoritative value",
    })
}
