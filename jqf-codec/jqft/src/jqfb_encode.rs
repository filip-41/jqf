//! The canonical `jqfb` encoder — the machine image of a value or document.
//!
//! Builds the flattened preorder node table (with subtree sizes), the deduplicated string and number pools, the
//! attached-facts chunk (from a located document), the provenance header (always), and — under the level-composition
//! flags — the retained-source chunk. The level-composition law : `with_source` request retention the run may not have;
//! a request the run cannot supply is a clean typed error naming the missing retention, never a silently thinner file.

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::Write as _;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EncodeItem, EncodeRequest, EncoderFactoryImpl, EncoderSession, ErasedEncoderFactory,
    ErasedEncoderSession, PreservationOutcome, PreservationReport, PreservationRequest, RecycledSessionState,
};
use jqf_data::{
    Document, FactPayload, IntrinsicTagSemantics, NodeId, NumberView, ScalarView, Value, ValueKind, ValueView,
};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::{Namespace, Severity};

use crate::jqfb::{self, kinds};
use crate::jqfb_decode::number_entry_end;
use crate::options::JqfbEncodeOptions;

const OFFER_BYTES: usize = 16 * 1024;

pub(crate) fn create_jqfb_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    request.expect_target(crate::FORMAT_ID_JQFB, &[crate::JQFB_CANONICAL_DIALECT_ID])?;
    let options = match request.options {
        None => JqfbEncodeOptions::default(),
        Some(options) => options
            .downcast_ref::<JqfbEncodeOptions>()
            .copied()
            .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))?,
    };
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || {
        Ok(JqfbEncoderFactory {
            options,
            leaf_pool_index: RefCell::new(None),
        })
    })
}

struct JqfbEncoderFactory {
    options: JqfbEncodeOptions,
    /// The leaf splice's dedup index, built over the ORIGINAL pool on the first cross-pool lookup and reused for every
    /// later leaf edit in the cycle (the factory lives for exactly one document edit).
    leaf_pool_index: RefCell<Option<LeafPoolIndex>>,
}

/// One pool chunk's dedup index: entry bytes -> entry index, the same shape `preload_pools` builds for the append
/// splice. Keyed by the chunk's type and byte range so a different source never reuses a stale index.
struct LeafPoolIndex {
    chunk_type: u32,
    offset: usize,
    length: usize,
    entries: BTreeMap<alloc::vec::Vec<u8>, u32>,
}

impl LeafPoolIndex {
    /// Looks up a full entry's index in the chunk's pool, building the index on first use — one pool walk per edit
    /// cycle instead of one per edited leaf.
    fn find(
        cache: &RefCell<Option<Self>>,
        chunk: ChunkRegion,
        source: &[u8],
        needle: &[u8],
    ) -> Result<Option<u32>, CodecError> {
        let mut cache = cache.borrow_mut();
        let fresh = matches!(
            cache.as_ref(),
            Some(built)
                if built.chunk_type == chunk.chunk_type
                    && built.offset == chunk.offset
                    && built.length == chunk.length
        );
        if !fresh {
            let pool = &source[chunk.offset..chunk.end()];
            let count = jqfb::pool_count(pool)?;
            let is_strg = chunk.chunk_type == jqfb::CHUNK_STRG;
            let mut entries = BTreeMap::new();
            let mut offset = 8usize;
            for index in 0..count {
                let next = if is_strg {
                    jqfb::pool_entry(pool, offset)?.1
                } else {
                    number_entry_end(pool, offset)?
                };
                entries.insert(
                    pool[offset..next].to_vec(),
                    u32::try_from(index).map_err(|_| jqfb::invalid("pool index overflows"))?,
                );
                offset = next;
            }
            *cache = Some(Self {
                chunk_type: chunk.chunk_type,
                offset: chunk.offset,
                length: chunk.length,
                entries,
            });
        }
        Ok(cache.as_ref().and_then(|built| built.entries.get(needle).copied()))
    }
}

impl EncoderFactoryImpl for JqfbEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        crate::JQFB_ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        ErasedEncoderSession::try_new(item, PreservationRequest::None, || {
            Ok(JqfbEncoder {
                bytes: Vec::new(),
                root_done: false,
                options: self.options,
            })
        })
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<JqfbEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }

    /// The T5 leaf seam: a changed scalar splices its authored tail — the node's table entry through EOF, the span the
    /// decoder bound — which carries the value's pool entry and the footer-directory words with it. The policy lives in
    /// `jqfb.rs`'s module docs.
    fn render_leaf(
        &self,
        document: &Document<'_>,
        node: NodeId,
        path: &[String],
        source: &[u8],
        value: &Value,
        authored: Option<&[u8]>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        render_leaf(
            document,
            node,
            path,
            source,
            value,
            authored,
            &self.leaf_pool_index,
            resources,
        )
    }

    fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        render_edit_append(document, container, path, source, members, resources)
    }

    fn render_edit_remove(
        &self,
        document: &Document<'_>,
        container: NodeId,
        path: &[String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        render_edit_remove(document, container, path, source, members, resources)
    }
}

/// The image-building scratch, owned by one item. Every pool is a plain `Vec<u8>`; the whole image — the pools, the
/// chunk payloads, and the assembled output — charges the ambient allocator.
struct ImageBuilder {
    nodes: Vec<u8>,
    strg: Vec<u8>,
    strg_index: BTreeMap<alloc::vec::Vec<u8>, u32>,
    strg_count: u32,
    numb: Vec<u8>,
    numb_index: BTreeMap<String, u32>,
    numb_count: u32,
    numb_scratch: String,
    facts: Vec<u8>,
    fact_count: u64,
    prov: Vec<u8>,
    sour: Option<Vec<u8>>,
    source_label: Option<String>,
}

struct JqfbEncoder {
    bytes: Vec<u8>,
    root_done: bool,
    options: JqfbEncodeOptions,
}

fn unrepresentable(message: &str) -> CodecError {
    // The plain carrier builds fallibly; on refusal the bare failure survives, so the error path never makes an
    // unrepresentable document worse.
    let base = CodecError::new(CodecFailureKind::UnsupportedRepresentation);
    let Some(diagnostic) =
        jqf_source::Diagnostic::try_new(Namespace::new("jqfb").code("representation"), Severity::Error, message)
    else {
        return base;
    };
    base.with_diagnostic(diagnostic)
}

fn map_data(error: jqf_data::DataError) -> CodecError {
    match error {
        jqf_data::DataError::Resource(error) => error.into(),
        jqf_data::DataError::Control(error) => error.into(),
        jqf_data::DataError::ArithmeticOverflow => CodecError::new(CodecFailureKind::Overflow),
        jqf_data::DataError::Allocation => CodecError::new(CodecFailureKind::AllocationFailure),
        _ => CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "jqfb encoder document read",
        }),
    }
}

fn data_contract() -> CodecError {
    CodecError::new(CodecFailureKind::InternalContractViolation {
        contract: "jqfb encoder state",
    })
}

impl ImageBuilder {
    fn new() -> Self {
        Self {
            nodes: Vec::new(),
            strg: Vec::new(),
            strg_index: BTreeMap::new(),
            strg_count: 0,
            numb: Vec::new(),
            numb_index: BTreeMap::new(),
            numb_count: 0,
            numb_scratch: String::new(),
            facts: Vec::new(),
            fact_count: 0,
            prov: Vec::new(),
            sour: None,
            source_label: None,
        }
    }

    fn strg(&mut self, text: &[u8]) -> Result<u32, CodecError> {
        // Key the pool by the RAW bytes: `from_utf8_lossy` is non-injective (any two invalid-UTF-8 sequences collide —
        // `0xff` and `0xefbfbd` both map to U+FFFD), so a lossy key would silently merge distinct Bytes values into one
        // pool entry.
        if let Some(index) = self.strg_index.get(text) {
            return Ok(*index);
        }
        let index = self.strg_count;
        jqfb::push_pool_entry(&mut self.strg, text)?;
        self.strg_count += 1;
        self.strg_index.insert(text.to_vec(), index);
        Ok(index)
    }

    #[allow(
        clippy::cast_sign_loss,
        reason = "a decimal scale is stored as its two's-complement u64 bits; the reader                   recovers the i64 with the inverse byte cast"
    )]
    fn numb_integer(&mut self, text: &str) -> Result<u32, CodecError> {
        self.numb_scratch.clear();
        self.numb_scratch.push('0');
        self.numb_scratch.push_str(text);
        self.intern_numb_scratch()
    }

    fn numb_decimal(&mut self, coefficient: &str, scale: i64) -> Result<u32, CodecError> {
        self.numb_scratch.clear();
        let _ = write!(self.numb_scratch, "1{coefficient}|{scale}");
        self.intern_numb_scratch()
    }

    fn numb_float(&mut self, bits: u64) -> Result<u32, CodecError> {
        self.numb_scratch.clear();
        let _ = write!(self.numb_scratch, "2{bits:016x}");
        self.intern_numb_scratch()
    }

    fn intern_numb_scratch(&mut self) -> Result<u32, CodecError> {
        if let Some(&index) = self.numb_index.get(self.numb_scratch.as_str()) {
            return Ok(index);
        }
        let key = self.numb_scratch.clone();
        let index = self.insert_numb(&key)?;
        self.numb_index.insert(key, index);
        Ok(index)
    }

    fn insert_numb(&mut self, key: &str) -> Result<u32, CodecError> {
        let index = self.numb_count;
        // The pool entry's first byte is the CATEGORY TAG (0 integer, 1 decimal, 2 float) exactly as the decoder reads
        // it; the remainder of the key is the tag-specific body.
        match key.as_bytes().first() {
            Some(b'0') => {
                self.numb.extend_from_slice(&[0]);
                jqfb::push_pool_entry(&mut self.numb, &key.as_bytes()[1..])?;
            }
            Some(b'1') => {
                self.numb.extend_from_slice(&[1]);
                let rest = &key.as_bytes()[1..];
                let split = rest.iter().position(|byte| *byte == b'|').ok_or_else(data_contract)?;
                jqfb::push_pool_entry(&mut self.numb, &rest[..split])?;
                let scale = core::str::from_utf8(&rest[split + 1..])
                    .ok()
                    .and_then(|text| text.parse::<i64>().ok())
                    .ok_or_else(data_contract)?;
                jqfb::push_u64(&mut self.numb, scale.cast_unsigned());
            }
            Some(b'2') => {
                self.numb.extend_from_slice(&[2]);
                let bits = core::str::from_utf8(&key.as_bytes()[1..])
                    .ok()
                    .and_then(|text| u64::from_str_radix(text, 16).ok())
                    .ok_or_else(data_contract)?;
                jqfb::push_u64(&mut self.numb, bits);
            }
            _ => return Err(data_contract()),
        }
        self.numb_count += 1;
        Ok(index)
    }

    fn emit(&mut self, kind: u8, payload: u32) -> u32 {
        let index = u32::try_from(self.nodes.len() / kinds::ENTRY_LEN).unwrap_or(u32::MAX);
        self.nodes.extend_from_slice(&[kind]);
        self.nodes.extend_from_slice(&0u32.to_le_bytes());
        self.nodes.extend_from_slice(&payload.to_le_bytes());
        index
    }

    /// Emits a LEAF node (subtree size 1).
    fn emit_leaf(&mut self, kind: u8, payload: u32) -> u32 {
        let start = self.emit(kind, payload);
        self.seal(start, true);
        start
    }

    /// Emits a CONTAINER node (subtree size patched once its children land).
    fn emit_container(&mut self, kind: u8, payload: u32) -> u32 {
        self.emit(kind, payload)
    }

    /// Marks a node as a complete leaf (subtree size 1) or patches a container's subtree size once its whole subtree
    /// has been emitted.
    fn seal(&mut self, start: u32, leaf: bool) {
        let size = if leaf {
            1
        } else {
            let end = self.nodes.len() / kinds::ENTRY_LEN;
            u32::try_from(end - start as usize).unwrap_or(u32::MAX)
        };
        let offset = start as usize * kinds::ENTRY_LEN + 1;
        self.nodes.as_mut_slice()[offset..offset + 4].copy_from_slice(&size.to_le_bytes());
    }

    fn add_fact(
        &mut self,
        node_index: u32,
        role: &str,
        kind: &str,
        revision: u32,
        payload: &FactPayload,
    ) -> Result<(), CodecError> {
        jqfb::push_u32(&mut self.facts, node_index);
        jqfb::push_pool_entry(&mut self.facts, role.as_bytes())?;
        jqfb::push_pool_entry(&mut self.facts, kind.as_bytes())?;
        jqfb::push_u32(&mut self.facts, revision);
        write_fact_payload(&mut self.facts, payload)?;
        self.fact_count += 1;
        Ok(())
    }
}

/// The preorder walk over an OWNED value: emits the node table for the subtree, returning its start index.
fn walk_owned(builder: &mut ImageBuilder, value: &Value, resources: &ResourceContext<'_>) -> Result<u32, CodecError> {
    let _depth = resources.enter_nesting().map_err(CodecError::from)?;
    match value {
        Value::Null => Ok(builder.emit_leaf(kinds::NULL, 0)),
        Value::Bool(true) => Ok(builder.emit_leaf(kinds::BOOL, 1)),
        Value::Bool(false) => Ok(builder.emit_leaf(kinds::BOOL, 0)),
        Value::Number(number) => {
            // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained one.
            if let Some(machine) = number.as_machine() {
                let integer = jqf_data::Integer::from_i64(machine);
                let index = builder.numb_integer(integer.as_str())?;
                Ok(builder.emit_leaf(kinds::INTEGER, index))
            } else if let Some(integer) = number.as_integer() {
                let index = builder.numb_integer(integer.as_str())?;
                Ok(builder.emit_leaf(kinds::INTEGER, index))
            } else if let Some(decimal) = number.as_decimal() {
                let index = builder.numb_decimal(decimal.coefficient().as_str(), decimal.scale())?;
                Ok(builder.emit_leaf(kinds::DECIMAL, index))
            } else if let Some(float) = number.as_float() {
                let index = builder.numb_float(float.get().to_bits())?;
                Ok(builder.emit_leaf(kinds::FLOAT, index))
            } else {
                Err(data_contract())
            }
        }
        Value::String(text) => {
            let index = builder.strg(text.as_bytes())?;
            Ok(builder.emit_leaf(kinds::STRING, index))
        }
        Value::Bytes(bytes) => {
            let index = builder.strg(bytes.as_ref())?;
            Ok(builder.emit_leaf(kinds::BYTES, index))
        }
        Value::LocalDate(date) => {
            let mut text = String::new();
            date.write_text(&mut text).map_err(|_| data_contract())?;
            let index = builder.strg(text.as_bytes())?;
            Ok(builder.emit_leaf(kinds::LOCAL_DATE, index))
        }
        Value::LocalTime(time) => {
            let mut text = String::new();
            time.write_text(&mut text).map_err(|_| data_contract())?;
            let index = builder.strg(text.as_bytes())?;
            Ok(builder.emit_leaf(kinds::LOCAL_TIME, index))
        }
        Value::LocalDateTime(datetime) => {
            let mut text = String::new();
            datetime.write_text(&mut text).map_err(|_| data_contract())?;
            let index = builder.strg(text.as_bytes())?;
            Ok(builder.emit_leaf(kinds::LOCAL_DATE_TIME, index))
        }
        Value::OffsetDateTime(datetime) => {
            let mut text = String::new();
            datetime.write_text(&mut text).map_err(|_| data_contract())?;
            let index = builder.strg(text.as_bytes())?;
            Ok(builder.emit_leaf(kinds::OFFSET_DATE_TIME, index))
        }
        Value::Tagged { tag, payload } => {
            let tag_index = builder.strg(tag.as_str().as_bytes())?;
            let start = builder.emit_container(kinds::TAG, tag_index);
            walk_owned(builder, payload, resources)?;
            builder.seal(start, false);
            Ok(start)
        }
        Value::Array(array) => {
            let start = builder.emit_container(kinds::ARRAY, u32::try_from(array.len()).unwrap_or(u32::MAX));
            for item in array {
                walk_owned(builder, item, resources)?;
            }
            builder.seal(start, false);
            Ok(start)
        }
        Value::Object(object) => {
            let start = builder.emit_container(kinds::OBJECT, u32::try_from(object.len()).unwrap_or(u32::MAX));
            for entry in object {
                let key = builder.strg(entry.key().as_bytes())?;
                builder.emit_leaf(kinds::KEYTEXT, key);
                walk_owned(builder, entry.value(), resources)?;
            }
            builder.seal(start, false);
            Ok(start)
        }
    }
}

/// The preorder walk over a LOCATED document node: emits the node table and the attached facts for each node, returning
/// the subtree's start index.
#[allow(
    clippy::too_many_lines,
    reason = "one value-kind dispatch table: every kind's node-table emission sits beside the others"
)]
fn walk_located(
    builder: &mut ImageBuilder,
    document: &Document<'_>,
    view: ValueView<'_, '_>,
    facts: &BTreeMap<NodeId, Vec<DocumentFactSnapshot>>,
    resources: &ResourceContext<'_>,
) -> Result<u32, CodecError> {
    let _depth = resources.enter_nesting().map_err(CodecError::from)?;
    if view.tag_semantics().map_err(map_data)? == Some(IntrinsicTagSemantics::Tagged) {
        let tag = view.tag().map_err(map_data)?.ok_or_else(data_contract)?;
        let tag_index = builder.strg(tag.as_str().as_bytes())?;
        let start = builder.emit_container(kinds::TAG, tag_index);
        let payload = document
            .tag_payload(view.node())
            .map_err(map_data)?
            .ok_or_else(data_contract)?;
        let handle = document.node_handle(payload).map_err(map_data)?;
        let payload = document.value_view(handle).map_err(map_data)?;
        walk_located(builder, document, payload, facts, resources)?;
        builder.seal(start, false);
        emit_facts(builder, start, view.node(), facts)?;
        return Ok(start);
    }
    let kind = view.kind().map_err(map_data)?;
    let start = match kind {
        ValueKind::Null => builder.emit_leaf(kinds::NULL, 0),
        ValueKind::Bool => {
            let ScalarView::Bool(value) = view.scalar().map_err(map_data)?.ok_or_else(data_contract)? else {
                return Err(data_contract());
            };
            builder.emit_leaf(kinds::BOOL, u32::from(value))
        }
        ValueKind::Number => {
            let ScalarView::Number(number) = view.scalar().map_err(map_data)?.ok_or_else(data_contract)? else {
                return Err(data_contract());
            };
            match number {
                NumberView::Number(number) => {
                    // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its
                    // retained one.
                    if let Some(machine) = number.as_machine() {
                        let integer = jqf_data::Integer::from_i64(machine);
                        let index = builder.numb_integer(integer.as_str())?;
                        builder.emit_leaf(kinds::INTEGER, index)
                    } else if let Some(integer) = number.as_integer() {
                        let index = builder.numb_integer(integer.as_str())?;
                        builder.emit_leaf(kinds::INTEGER, index)
                    } else if let Some(decimal) = number.as_decimal() {
                        let index = builder.numb_decimal(decimal.coefficient().as_str(), decimal.scale())?;
                        builder.emit_leaf(kinds::DECIMAL, index)
                    } else if let Some(float) = number.as_float() {
                        let index = builder.numb_float(float.get().to_bits())?;
                        builder.emit_leaf(kinds::FLOAT, index)
                    } else {
                        return Err(data_contract());
                    }
                }
                NumberView::Integer(text) => {
                    let index = builder.numb_integer(text)?;
                    builder.emit_leaf(kinds::INTEGER, index)
                }
                NumberView::Decimal { coefficient, scale } => {
                    let index = builder.numb_decimal(coefficient, scale)?;
                    builder.emit_leaf(kinds::DECIMAL, index)
                }
                NumberView::Float(value) => {
                    let index = builder.numb_float(value.get().to_bits())?;
                    builder.emit_leaf(kinds::FLOAT, index)
                }
            }
        }
        ValueKind::String => {
            let ScalarView::String(text) = view.scalar().map_err(map_data)?.ok_or_else(data_contract)? else {
                return Err(data_contract());
            };
            let index = builder.strg(text.as_bytes())?;
            builder.emit_leaf(kinds::STRING, index)
        }
        ValueKind::Bytes => {
            let ScalarView::Bytes(bytes) = view.scalar().map_err(map_data)?.ok_or_else(data_contract)? else {
                return Err(data_contract());
            };
            let index = builder.strg(bytes)?;
            builder.emit_leaf(kinds::BYTES, index)
        }
        ValueKind::LocalDate => {
            let ScalarView::LocalDate(date) = view.scalar().map_err(map_data)?.ok_or_else(data_contract)? else {
                return Err(data_contract());
            };
            let mut text = String::new();
            date.write_text(&mut text).map_err(|_| data_contract())?;
            let index = builder.strg(text.as_bytes())?;
            builder.emit_leaf(kinds::LOCAL_DATE, index)
        }
        ValueKind::LocalTime => {
            let ScalarView::LocalTime(time) = view.scalar().map_err(map_data)?.ok_or_else(data_contract)? else {
                return Err(data_contract());
            };
            let mut text = String::new();
            time.write_text(&mut text).map_err(|_| data_contract())?;
            let index = builder.strg(text.as_bytes())?;
            builder.emit_leaf(kinds::LOCAL_TIME, index)
        }
        ValueKind::LocalDateTime => {
            let ScalarView::LocalDateTime(datetime) = view.scalar().map_err(map_data)?.ok_or_else(data_contract)?
            else {
                return Err(data_contract());
            };
            let mut text = String::new();
            datetime.write_text(&mut text).map_err(|_| data_contract())?;
            let index = builder.strg(text.as_bytes())?;
            builder.emit_leaf(kinds::LOCAL_DATE_TIME, index)
        }
        ValueKind::OffsetDateTime => {
            let ScalarView::OffsetDateTime(datetime) = view.scalar().map_err(map_data)?.ok_or_else(data_contract)?
            else {
                return Err(data_contract());
            };
            let mut text = String::new();
            datetime.write_text(&mut text).map_err(|_| data_contract())?;
            let index = builder.strg(text.as_bytes())?;
            builder.emit_leaf(kinds::OFFSET_DATE_TIME, index)
        }
        ValueKind::Array => {
            let array = view.array().map_err(map_data)?.ok_or_else(data_contract)?;
            let items: Vec<ValueView<'_, '_>> = array.iter().collect();
            let start = builder.emit_container(kinds::ARRAY, u32::try_from(items.len()).unwrap_or(u32::MAX));
            for item in items {
                walk_located(builder, document, item, facts, resources)?;
            }
            builder.seal(start, false);
            start
        }
        ValueKind::Object => {
            let object = view.object().map_err(map_data)?.ok_or_else(data_contract)?;
            let entries: Vec<(String, ValueView<'_, '_>)> = object
                .iter()
                .map(|entry| {
                    let entry = entry.map_err(map_data)?;
                    Ok((String::from(entry.key()), entry.value()))
                })
                .collect::<Result<_, CodecError>>()?;
            let start = builder.emit_container(kinds::OBJECT, u32::try_from(entries.len()).unwrap_or(u32::MAX));
            for (key, value) in entries {
                let key_index = builder.strg(key.as_bytes())?;
                builder.emit_leaf(kinds::KEYTEXT, key_index);
                walk_located(builder, document, value, facts, resources)?;
            }
            builder.seal(start, false);
            start
        }
    };
    emit_facts(builder, start, view.node(), facts)?;
    Ok(start)
}

/// A fact snapshot copied out of the document's fact reader.
struct DocumentFactSnapshot {
    role: String,
    kind: String,
    revision: u32,
    payload: FactPayload,
}

/// Attaches one node's facts to the image's FACT chunk.
fn emit_facts(
    builder: &mut ImageBuilder,
    node_index: u32,
    node: NodeId,
    facts: &BTreeMap<NodeId, Vec<DocumentFactSnapshot>>,
) -> Result<(), CodecError> {
    let Some(snapshots) = facts.get(&node) else {
        return Ok(());
    };
    for snapshot in snapshots {
        builder.add_fact(
            node_index,
            &snapshot.role,
            &snapshot.kind,
            snapshot.revision,
            &snapshot.payload,
        )?;
    }
    Ok(())
}

/// Serializes one owned fact payload into the FACT chunk.
#[allow(
    clippy::cast_sign_loss,
    reason = "a decimal scale is stored as its two's-complement u64 bits; the reader               recovers the i64 with the inverse byte cast"
)]
fn write_fact_payload(out: &mut Vec<u8>, payload: &FactPayload) -> Result<(), CodecError> {
    match payload {
        FactPayload::Null => {
            out.extend_from_slice(&[0]);
            Ok(())
        }
        FactPayload::Bool(value) => {
            out.extend_from_slice(&[1]);
            out.extend_from_slice(&[u8::from(*value)]);
            Ok(())
        }
        FactPayload::Integer(value) => {
            out.extend_from_slice(&[2]);
            jqfb::push_pool_entry(out, value.as_str().as_bytes())
        }
        FactPayload::Decimal(value) => {
            out.extend_from_slice(&[3]);
            jqfb::push_pool_entry(out, value.coefficient().as_str().as_bytes())?;
            jqfb::push_u64(out, value.scale() as u64);
            Ok(())
        }
        FactPayload::Text(text) => {
            out.extend_from_slice(&[4]);
            jqfb::push_pool_entry(out, text.as_bytes())
        }
        FactPayload::Bytes(bytes) => {
            out.extend_from_slice(&[5]);
            jqfb::push_pool_entry(out, bytes)
        }
        FactPayload::List(list) => {
            out.extend_from_slice(&[6]);
            jqfb::push_u64(out, list.len() as u64);
            for item in list {
                write_fact_payload(out, item)?;
            }
            Ok(())
        }
        FactPayload::Map(map) => {
            out.extend_from_slice(&[7]);
            jqfb::push_u64(out, map.len() as u64);
            for (key, value) in map {
                jqfb::push_pool_entry(out, key.as_bytes())?;
                write_fact_payload(out, value)?;
            }
            Ok(())
        }
        FactPayload::OpaqueBytes(bytes) => {
            out.extend_from_slice(&[8]);
            jqfb::push_pool_entry(out, bytes)
        }
    }
}

/// Converts a borrowed payload view into the owned portable payload.
///
/// A fact payload whose Integer/Decimal spelling does not parse is a malformed document shape, not a zero: the encoder
/// fails with a named representation error rather than silently publishing a corrupted number.
fn owned_fact_payload(view: &jqf_data::FactPayloadView<'_>) -> Result<FactPayload, CodecError> {
    match view {
        jqf_data::FactPayloadView::Null => Ok(FactPayload::Null),
        jqf_data::FactPayloadView::Bool(value) => Ok(FactPayload::Bool(*value)),
        jqf_data::FactPayloadView::Integer(text) => {
            let integer = jqf_data::Integer::parse(text)
                .map_err(|_| unrepresentable("an attached fact Integer payload does not parse as an integer"))?;
            Ok(FactPayload::Integer(integer))
        }
        jqf_data::FactPayloadView::Decimal { coefficient, scale } => {
            let integer = jqf_data::Integer::parse(coefficient).map_err(|_| {
                unrepresentable("an attached fact Decimal payload's coefficient does not parse as an integer")
            })?;
            let decimal = jqf_data::Decimal::from_literal_parts(integer, *scale)
                .map_err(|_| unrepresentable("an attached fact Decimal payload is out of range"))?;
            Ok(FactPayload::Decimal(decimal))
        }
        jqf_data::FactPayloadView::Text(text) => Ok(FactPayload::Text((*text).to_owned())),
        jqf_data::FactPayloadView::Bytes(bytes) => Ok(FactPayload::Bytes(bytes.to_vec())),
        jqf_data::FactPayloadView::List(list) => Ok(FactPayload::List(
            list.iter()
                .map(|item| owned_fact_payload(&item))
                .collect::<Result<Vec<_>, _>>()?,
        )),
        jqf_data::FactPayloadView::Map(map) => Ok(FactPayload::Map(
            map.iter()
                .map(|(key, value)| -> Result<(String, FactPayload), CodecError> {
                    Ok(((*key).to_owned(), owned_fact_payload(&value)?))
                })
                .collect::<Result<Vec<_>, _>>()?,
        )),
        jqf_data::FactPayloadView::OpaqueBytes(bytes) => Ok(FactPayload::OpaqueBytes(bytes.to_vec())),
    }
}

/// Reads every attached fact of a document into a per-node snapshot map.
fn snapshot_facts(
    document: &Document<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<BTreeMap<NodeId, Vec<DocumentFactSnapshot>>, CodecError> {
    let mut out: BTreeMap<NodeId, Vec<DocumentFactSnapshot>> = BTreeMap::new();
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => return Ok(out),
        Err(_) => return Err(data_contract()),
    };
    let limit = jqf_data::unbounded_batch_limit();
    loop {
        let poll = reader.poll_batch(limit, resources).map_err(|_| data_contract())?;
        match poll {
            jqf_data::ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    let owner = fact.owner();
                    let jqf_data::LocalOwnerRef::Node(node) = owner else {
                        continue;
                    };
                    out.entry(node).or_default().push(DocumentFactSnapshot {
                        role: String::from(fact.role().as_str()),
                        kind: String::from(fact.kind().as_str()),
                        revision: fact.schema_version(),
                        payload: owned_fact_payload(&fact.payload())?,
                    });
                }
            }
            jqf_data::ReaderPoll::Pending => {
                // The cooperative reader shares the encoder's work budget; refresh it exactly as the XML serializer
                // does between its own internal phases, so a fact-bearing document's pass completes (a no-op here would
                // spin forever).
                resources
                    .try_begin_next_cooperative_entry(4_096)
                    .map_err(|error| match error {
                        // A host control stop here is an internal seam violation on this path; the memory trip is the
                        // accounting refusal and surfaces as such.
                        jqf_resource::CooperativeError::Control(_) => data_contract(),
                        jqf_resource::CooperativeError::Memory(error) => CodecError::from(error),
                    })?;
            }
            jqf_data::ReaderPoll::End(_) => break,
        }
    }
    Ok(out)
}

impl JqfbEncoder {
    /// Reinitializes one recycled encoder for one more ordered item: drops every byte and flag a previous item may have
    /// left behind — including one that aborted mid-offer, whose partial staging must never reach the next item —
    /// leaving exactly the state a fresh [`EncoderFactoryImpl::start`] would have produced.
    fn reset(&mut self) {
        self.bytes.clear();
        self.root_done = false;
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let mut builder = ImageBuilder::new();
        let mut source: Option<(&[u8], String)> = None;
        let _document_facts = match item {
            EncodeItem::Owned(value) => {
                walk_owned(&mut builder, value, resources)?;
                BTreeMap::new()
            }
            EncodeItem::Located { product, node } => {
                let document = product.document();
                let view = document.value_view(node).map_err(map_data)?;
                let facts = snapshot_facts(document, resources)?;
                walk_located(&mut builder, document, view, &facts, resources)?;
                if let Some(segment) = document.source_segment() {
                    source = Some((segment, String::from("jqfb-source")));
                }
                facts
            }
        };
        // Provenance header (ignorable, always): the producing codec, the producing dialect, and the producing jqf
        // version.
        let mut prov = Vec::new();
        jqfb::push_pool_entry(&mut prov, b"jqfb")?;
        jqfb::push_pool_entry(&mut prov, crate::JQFB_CANONICAL_DIALECT_ID.as_bytes())?;
        jqfb::push_pool_entry(&mut prov, env!("CARGO_PKG_VERSION").as_bytes())?;
        builder.prov = prov;
        builder.source_label = source.as_ref().map(|(_, label)| label.clone());
        // Level-composition law: `with_source` request the retained source; a run without it fails cleanly, never
        // publishing a thinner file.
        if self.options.with_source {
            let Some((segment, _)) = source else {
                return Err(unrepresentable(
                    "this run cannot supply the source level (with_source): \
                     the document carries no retained source — encode a document produced by \
                     a source-backed decode",
                ));
            };
            let mut sour = Vec::new();
            let label = builder.source_label.clone().unwrap_or_default();
            jqfb::push_pool_entry(&mut sour, label.as_bytes())?;
            sour.extend_from_slice(segment);
            builder.sour = Some(sour);
        }
        self.assemble(builder);
        Ok(())
    }

    /// Assembles the image: header, chunks in a deterministic order, footer directory, footer length.
    fn assemble(&mut self, mut builder: ImageBuilder) {
        let mut image = Vec::new();
        image.extend_from_slice(jqfb::MAGIC);
        jqfb::push_u16(&mut image, jqfb::VERSION);
        jqfb::push_u32(&mut image, 0);
        // Each chunk payload is a fresh Vec<u8> so its digest is computed over the exact bytes that are appended.
        let mut chunks: Vec<(u32, Vec<u8>)> = Vec::new();
        let _ = &mut builder.nodes;
        chunks.push((jqfb::CHUNK_NODE, core::mem::take(&mut builder.nodes)));
        let mut strg_payload = Vec::new();
        jqfb::push_u64(&mut strg_payload, u64::from(builder.strg_count));
        strg_payload.extend_from_slice(builder.strg.as_slice());
        chunks.push((jqfb::CHUNK_STRG, strg_payload));
        let mut numb_payload = Vec::new();
        jqfb::push_u64(&mut numb_payload, u64::from(builder.numb_count));
        numb_payload.extend_from_slice(builder.numb.as_slice());
        chunks.push((jqfb::CHUNK_NUMB, numb_payload));
        let mut fact_payload = Vec::new();
        jqfb::push_u64(&mut fact_payload, builder.fact_count);
        fact_payload.extend_from_slice(builder.facts.as_slice());
        chunks.push((jqfb::CHUNK_FACT, fact_payload));
        chunks.push((jqfb::CHUNK_PROV, core::mem::take(&mut builder.prov)));
        if let Some(sour) = builder.sour.take() {
            chunks.push((jqfb::CHUNK_SOUR, sour));
        }
        let mut entries: Vec<(u32, u64, u64, [u8; 32])> = Vec::new();
        for (chunk_type, payload) in chunks {
            let offset = image.len() as u64;
            let length = payload.len() as u64;
            let digest = *blake3::hash(payload.as_slice()).as_bytes();
            entries.push((chunk_type, offset, length, digest));
            image.extend_from_slice(payload.as_slice());
        }
        let footer_start = image.len() as u64;
        jqfb::push_u64(&mut image, entries.len() as u64);
        for (chunk_type, offset, length, digest) in &entries {
            jqfb::push_u32(&mut image, *chunk_type);
            jqfb::push_u64(&mut image, *offset);
            jqfb::push_u64(&mut image, *length);
            image.extend_from_slice(digest);
        }
        let footer_len = image.len() as u64 - footer_start + 8;
        jqfb::push_u64(&mut image, footer_len);
        self.push(image.as_slice());
    }

    fn report() -> PreservationReport {
        PreservationReport::new(
            PreservationOutcome::Exact,
            PreservationOutcome::Exact,
            PreservationOutcome::Exact,
            PreservationOutcome::Normalized,
        )
    }
}

impl EncoderSession for JqfbEncoder {
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn encode(
        &mut self,
        item: EncodeItem<'_, '_>,
        sink: &mut dyn ByteSink,
        context: &mut CodecRunContext<'_, '_>,
    ) -> Result<PreservationReport, CodecError> {
        loop {
            if self.root_done {
                if !self.bytes.is_empty() {
                    sink.write_all(&self.bytes, context.resources())?;
                    self.bytes.clear();
                }
                return Ok(Self::report());
            }
            if self.bytes.len() >= OFFER_BYTES {
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
            }
            let remaining = context.resources().remaining_work() as usize;
            match context.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => context.replenish_work()?,
                WorkAdmission::Granted(_granted) => {
                    self.encode_item(item, context.resources())?;
                    self.root_done = true;
                }
            }
        }
    }
}

// --------------------------------------------------------------------------- The T5 splice machinery (`render_leaf` /
// `render_edit_append` / `render_edit_remove`), the binary-splice policy's implementation. The policy is written in the
// format docs (`jqfb.rs`): a leaf splice is the tail from the changed item through EOF — the node entry, the value's
// pool entry, and the footer-directory words — with every byte between them copied verbatim; a structural splice
// re-derives the ONE container's counts and its ancestors' subtree_size fields, and rewrites the footer. Every walk
// here is deterministic over the codec's own round-tripped bytes; a run that cannot name a region declines to the
// whole-document floor (the SDK's re-decode law), never wrong bytes.

/// One chunk's recorded extent and digest (one footer directory entry).
#[derive(Clone, Copy)]
struct ChunkRegion {
    chunk_type: u32,
    offset: usize,
    length: usize,
    digest: [u8; 32],
}

impl ChunkRegion {
    fn end(self) -> usize {
        self.offset + self.length
    }
}

/// Parses the footer directory out of the source and returns the chunk regions in file order. The footer was validated
/// at decode; a source that no longer parses declines the splice (the SDK's re-decode law).
fn chunk_regions(source: &[u8]) -> Result<Vec<ChunkRegion>, CodecError> {
    let footer = jqfb::read_footer(source)?;
    Ok(footer
        .entries
        .iter()
        .map(|entry| ChunkRegion {
            chunk_type: entry.chunk_type,
            offset: entry.offset,
            length: entry.length,
            digest: entry.digest,
        })
        .collect())
}

fn region_by(regions: &[ChunkRegion], chunk_type: u32) -> Option<ChunkRegion> {
    regions.iter().copied().find(|region| region.chunk_type == chunk_type)
}

/// Whether the image carries at least one attached-fact record. A FACT record names its node by TABLE INDEX; a
/// structural splice shifts those indices, so appending or removing members over a fact-bearing image without rewriting
/// the FACT chunk would silently repoint every fact at whatever node lands on its old index.
fn has_fact_records(regions: &[ChunkRegion], source: &[u8]) -> Result<bool, CodecError> {
    let Some(chunk) = region_by(regions, jqfb::CHUNK_FACT) else {
        return Ok(false);
    };
    let fact = &source[chunk.offset..chunk.end()];
    Ok(jqfb::read_u64(fact, 0).ok_or_else(|| jqfb::invalid("truncated FACT count"))? > 0)
}

/// Writes one 9-byte node-table entry: kind, subtree size, payload.
fn node_entry_bytes(kind: u8, subtree_size: u32, payload: u32) -> [u8; kinds::ENTRY_LEN] {
    let mut out = [0u8; kinds::ENTRY_LEN];
    out[0] = kind;
    out[1..5].copy_from_slice(&subtree_size.to_le_bytes());
    out[5..9].copy_from_slice(&payload.to_le_bytes());
    out
}

/// Applies edits (relative to the chunk start, sorted, non-overlapping) to one chunk's payload and returns the new
/// bytes — the digest's input. An edit with empty replacement bytes is a cut.
fn splice_chunk(original: &[u8], edits: &[(usize, usize, &[u8])]) -> Vec<u8> {
    let mut out = Vec::with_capacity(original.len() + 16);
    let mut cursor = 0;
    for (start, end, bytes) in edits {
        out.extend_from_slice(&original[cursor..*start]);
        out.extend_from_slice(bytes);
        cursor = *end;
    }
    out.extend_from_slice(&original[cursor..]);
    out
}

/// Applies the edits falling inside `region` (absolute positions) to the region's bytes, returning the length delta and
/// the new digest. `None` when no edit touches the region — its digest and length are unchanged.
fn region_edit_digest(
    source: &[u8],
    region: ChunkRegion,
    edits: &[(usize, usize, Vec<u8>)],
) -> Option<(i64, [u8; 32])> {
    let relative: Vec<(usize, usize, &[u8])> = edits
        .iter()
        // Half-open intersection, PLUS one carve-out: an empty-range INSERTION exactly at `region.end()` (a grown
        // pool's new entry) belongs to THIS region — appended at its tail. Without the carve-out it is attributed to no
        // region (the directory keeps the old length); with a closed-interval test instead it satisfied both neighbors
        // and corrupted the NEXT chunk's directory entry.
        .filter(|(start, end, _)| {
            (*end > region.offset && *start < region.end()) || (*start == *end && *start == region.end())
        })
        .map(|(start, end, bytes)| (start - region.offset, end - region.offset, bytes.as_slice()))
        .collect();

    if relative.is_empty() {
        return None;
    }
    let original = &source[region.offset..region.end()];
    let new_bytes = splice_chunk(original, &relative);
    let delta = match (i64::try_from(new_bytes.len()), i64::try_from(original.len())) {
        (Ok(new_len), Ok(original_len)) => new_len.checked_sub(original_len)?,
        _ => return None,
    };
    Some((delta, *blake3::hash(&new_bytes).as_bytes()))
}

/// Builds the new footer directory bytes from the original regions, the per-region length deltas, and the changed
/// chunks' new digests. The chunk order and count are unchanged; the footer's own length word is fixed (count x 52 +
/// 16), so the footer replacement is always same-length.
fn new_footer(
    regions: &[ChunkRegion],
    deltas: &[i64],
    new_digests: &[(usize, [u8; 32])],
) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    let mut before: i64 = 0;
    let mut entries: Vec<(u32, u64, u64, [u8; 32])> = Vec::new();
    for (index, region) in regions.iter().enumerate() {
        let offset = i64::try_from(region.offset)
            .ok()
            .and_then(|value| value.checked_add(before))
            .ok_or_else(|| jqfb::invalid("chunk offset overflows"))?;
        let length = i64::try_from(region.length)
            .ok()
            .and_then(|value| value.checked_add(deltas[index]))
            .ok_or_else(|| jqfb::invalid("chunk length overflows"))?;
        if length < 0 {
            return Err(jqfb::invalid("chunk length underflows"));
        }
        let digest = new_digests
            .iter()
            .find(|(target, _)| *target == index)
            .map_or(region.digest, |(_, digest)| *digest);
        entries.push((
            region.chunk_type,
            u64::try_from(offset).map_err(|_| jqfb::invalid("chunk offset overflows"))?,
            u64::try_from(length).map_err(|_| jqfb::invalid("chunk length overflows"))?,
            digest,
        ));
        before = before
            .checked_add(deltas[index])
            .ok_or_else(|| jqfb::invalid("chunk offset overflows"))?;
    }
    jqfb::push_u64(&mut out, entries.len() as u64);
    for (chunk_type, offset, length, digest) in entries {
        jqfb::push_u32(&mut out, chunk_type);
        jqfb::push_u64(&mut out, offset);
        jqfb::push_u64(&mut out, length);
        out.extend_from_slice(&digest);
    }
    let footer_len = out.len() as u64 + 8;
    jqfb::push_u64(&mut out, footer_len);
    Ok(out)
}

/// The pool a node kind draws its value bytes from: `Some(true)` is the STRG pool, `Some(false)` the NUMB pool, `None`
/// a no-pool kind (null/bool).
fn kind_pool(kind: u8) -> Option<bool> {
    match kind {
        kinds::NULL | kinds::BOOL => None,
        kinds::INTEGER | kinds::DECIMAL | kinds::FLOAT => Some(false),
        _ => Some(true),
    }
}

/// Renders one owned scalar into its node kind and, for a pool-backed value, its FULL pool entry bytes (length word
/// included for STRG; tag and body for NUMB) — the entry a pool scan compares against. `None` for the no-pool kinds
/// (null/bool).
fn leaf_entry(value: &Value) -> Result<(u8, Option<Vec<u8>>), CodecError> {
    match value {
        // Null and bool carry their value in the node entry itself (kind plus payload), never a pool entry.
        Value::Null => Ok((kinds::NULL, None)),
        Value::Bool(_) => Ok((kinds::BOOL, None)),
        Value::Number(number) => {
            let mut entry = Vec::new();
            let kind = if let Some(machine) = number.as_machine() {
                let integer = jqf_data::Integer::from_i64(machine);
                entry.push(0);
                jqfb::push_pool_entry(&mut entry, integer.as_str().as_bytes())?;
                kinds::INTEGER
            } else if let Some(integer) = number.as_integer() {
                entry.push(0);
                jqfb::push_pool_entry(&mut entry, integer.as_str().as_bytes())?;
                kinds::INTEGER
            } else if let Some(decimal) = number.as_decimal() {
                entry.push(1);
                jqfb::push_pool_entry(&mut entry, decimal.coefficient().as_str().as_bytes())?;
                jqfb::push_u64(&mut entry, u64::from_ne_bytes(decimal.scale().to_ne_bytes()));
                kinds::DECIMAL
            } else if let Some(float) = number.as_float() {
                entry.push(2);
                jqfb::push_u64(&mut entry, float.get().to_bits());
                kinds::FLOAT
            } else {
                return Err(data_contract());
            };
            Ok((kind, Some(entry)))
        }
        Value::String(text) => {
            let mut entry = Vec::new();
            jqfb::push_pool_entry(&mut entry, text.as_bytes())?;
            Ok((kinds::STRING, Some(entry)))
        }
        Value::Bytes(bytes) => {
            let mut entry = Vec::new();
            jqfb::push_pool_entry(&mut entry, bytes.as_ref())?;
            Ok((kinds::BYTES, Some(entry)))
        }
        Value::LocalDate(date) => {
            let mut text = String::new();
            date.write_text(&mut text).map_err(|_| data_contract())?;
            let mut entry = Vec::new();
            jqfb::push_pool_entry(&mut entry, text.as_bytes())?;
            Ok((kinds::LOCAL_DATE, Some(entry)))
        }
        Value::LocalTime(time) => {
            let mut text = String::new();
            time.write_text(&mut text).map_err(|_| data_contract())?;
            let mut entry = Vec::new();
            jqfb::push_pool_entry(&mut entry, text.as_bytes())?;
            Ok((kinds::LOCAL_TIME, Some(entry)))
        }
        Value::LocalDateTime(datetime) => {
            let mut text = String::new();
            datetime.write_text(&mut text).map_err(|_| data_contract())?;
            let mut entry = Vec::new();
            jqfb::push_pool_entry(&mut entry, text.as_bytes())?;
            Ok((kinds::LOCAL_DATE_TIME, Some(entry)))
        }
        Value::OffsetDateTime(datetime) => {
            let mut text = String::new();
            datetime.write_text(&mut text).map_err(|_| data_contract())?;
            let mut entry = Vec::new();
            jqfb::push_pool_entry(&mut entry, text.as_bytes())?;
            Ok((kinds::OFFSET_DATE_TIME, Some(entry)))
        }
        Value::Tagged { .. } => Err(unrepresentable(
            "a tagged value cannot splice as a jqfb leaf (the tag is a container layer)",
        )),
        Value::Array(_) | Value::Object(_) => Err(data_contract()),
    }
}

/// The absolute byte range of the STRG pool entry at index `index`.
fn strg_entry_range(chunk: ChunkRegion, source: &[u8], index: u32) -> Result<(usize, usize), CodecError> {
    let pool = &source[chunk.offset..chunk.end()];
    let mut offset = 8usize;
    for _ in 0..index {
        let (_, next) = jqfb::pool_entry(pool, offset)?;
        offset = next;
    }
    let (_, next) = jqfb::pool_entry(pool, offset)?;
    Ok((chunk.offset + offset, chunk.offset + next))
}

/// The absolute byte range of the NUMB pool entry at index `index`.
fn numb_entry_range(chunk: ChunkRegion, source: &[u8], index: u32) -> Result<(usize, usize), CodecError> {
    let pool = &source[chunk.offset..chunk.end()];
    let mut offset = 8usize;
    for _ in 0..index {
        offset = number_entry_end(pool, offset)?;
    }
    let next = number_entry_end(pool, offset)?;
    Ok((chunk.offset + offset, chunk.offset + next))
}

/// Reconstructs one NUMB pool entry's canonical index key (the same string `ImageBuilder::numb` keys its dedup map by)
/// from its tag and body bytes.
fn number_key(pool: &[u8], offset: usize) -> Result<String, CodecError> {
    match *pool
        .get(offset)
        .ok_or_else(|| jqfb::invalid("truncated number entry tag"))?
    {
        0 => {
            let (text, _) = jqfb::pool_entry(pool, offset + 1)?;
            let text = core::str::from_utf8(text).map_err(|_| jqfb::invalid("number text is not UTF-8"))?;
            Ok(format!("0{text}"))
        }
        1 => {
            let (coefficient, after) = jqfb::pool_entry(pool, offset + 1)?;
            let coefficient =
                core::str::from_utf8(coefficient).map_err(|_| jqfb::invalid("number text is not UTF-8"))?;
            let scale = jqfb::read_u64(pool, after).ok_or_else(|| jqfb::invalid("truncated decimal scale"))?;
            Ok(format!("1{coefficient}|{}", i64::from_ne_bytes(scale.to_ne_bytes())))
        }
        2 => {
            let bits = jqfb::read_u64(pool, offset + 1).ok_or_else(|| jqfb::invalid("truncated float bits"))?;
            Ok(format!("2{bits:016x}"))
        }
        _ => Err(jqfb::invalid("unknown number pool tag")),
    }
}

/// Preloads a fresh image builder's pool indexes with the ORIGINAL pool contents, so the splice's value emission
/// deduplicates against the existing entries and appends only genuinely-new ones. The builder's `strg`/`numb` byte
/// buffers stay empty — they collect the appends.
fn preload_pools(builder: &mut ImageBuilder, source: &[u8], regions: &[ChunkRegion]) -> Result<(), CodecError> {
    if let Some(chunk) = region_by(regions, jqfb::CHUNK_STRG) {
        let pool = &source[chunk.offset..chunk.end()];
        let count = jqfb::pool_count(pool)?;
        builder.strg_count = u32::try_from(count).map_err(|_| jqfb::invalid("pool count overflows"))?;
        let mut offset = 8usize;
        for index in 0..count {
            let (entry, next) = jqfb::pool_entry(pool, offset)?;
            builder.strg_index.insert(
                entry.to_vec(),
                u32::try_from(index).map_err(|_| jqfb::invalid("pool index overflows"))?,
            );
            offset = next;
        }
    }
    if let Some(chunk) = region_by(regions, jqfb::CHUNK_NUMB) {
        let pool = &source[chunk.offset..chunk.end()];
        let count = jqfb::pool_count(pool)?;
        builder.numb_count = u32::try_from(count).map_err(|_| jqfb::invalid("pool count overflows"))?;
        let mut offset = 8usize;
        for index in 0..count {
            let key = number_key(pool, offset)?;
            builder.numb_index.insert(
                key,
                u32::try_from(index).map_err(|_| jqfb::invalid("pool index overflows"))?,
            );
            offset = number_entry_end(pool, offset)?;
        }
    }
    Ok(())
}

/// The table indexes of the containers enclosing `target`, root first — every container whose subtree contains it. Each
/// ancestor's `subtree_size` moves with a member splice below it, so the splice rewrites them all.
fn ancestor_chain(table: &[u8], target: usize) -> Result<Vec<usize>, CodecError> {
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for index in 0..target {
        while let Some(&(_, end)) = stack.last() {
            if end <= index {
                stack.pop();
            } else {
                break;
            }
        }
        let entry = jqfb::read_node(table, index)?;
        if matches!(entry.kind, kinds::ARRAY | kinds::OBJECT | kinds::TAG) {
            stack.push((index, index + entry.subtree_size as usize));
        }
    }
    while let Some(&(_, end)) = stack.last() {
        if end <= target {
            stack.pop();
        } else {
            break;
        }
    }
    Ok(stack.into_iter().map(|(index, _)| index).collect())
}

/// The shared splice prelude: the container's node-entry position, the validated NODE chunk, and the container's table
/// entry. `None` declines.
#[allow(clippy::type_complexity)]
fn splice_prelude<'a>(
    document: &Document<'_>,
    node: NodeId,
    source: &'a [u8],
) -> Result<Option<(usize, ChunkRegion, Vec<ChunkRegion>, &'a [u8], usize, jqfb::NodeEntry)>, CodecError> {
    let Some(span) = document.node_source_span(node).map_err(map_data)? else {
        return Ok(None);
    };
    let node_entry = span.start() as usize;
    if span.end() as usize != source.len() {
        return Ok(None);
    }
    let regions = chunk_regions(source)?;
    let node_region = region_by(&regions, jqfb::CHUNK_NODE).ok_or_else(data_contract)?;
    let table_index = node_entry
        .checked_sub(node_region.offset)
        .ok_or_else(|| jqfb::invalid("node entry lies before the NODE chunk"))?
        / kinds::ENTRY_LEN;
    let table = &source[node_region.offset..node_region.end()];
    let entry = jqfb::read_node(table, table_index)?;
    Ok(Some((node_entry, node_region, regions, table, table_index, entry)))
}

/// The footer's own byte length (its final u64 word).
fn footer_len(source: &[u8]) -> Result<usize, CodecError> {
    let length = jqfb::read_u64(source, source.len() - 8).ok_or_else(|| jqfb::invalid("truncated footer"))?;
    usize::try_from(length).map_err(|_| jqfb::invalid("footer overflows"))
}

/// The footer replacement: the last `footer_len` bytes, replaced with the re-derived directory. Same length by
/// construction.
fn footer_insertion(
    source: &[u8],
    regions: &[ChunkRegion],
    deltas: &[i64],
    new_digests: &[(usize, [u8; 32])],
) -> Result<EditInsertion, CodecError> {
    let footer_start = source.len() - footer_len(source)?;
    Ok(EditInsertion {
        at: footer_start,
        bytes: new_footer(regions, deltas, new_digests)?,
        replace: Some((footer_start, source.len())),
    })
}

/// Renders the splice for a changed LEAF (the T5 ruling): the replacement is the tail from the node's table entry
/// through EOF — the node entry (when the value's kind or pool home changed), the value's pool entry (replaced in place
/// when the new value keeps the old pool, appended — deduplicated against the pool — when it changes pools), and the
/// footer directory with every changed chunk's digest and shifted offset; every byte between them is copied verbatim.
/// The authored span bound at decode is exactly this tail, so the caller's replacement is the splice.
fn pool_entry_referents(table: &[u8], is_strg: bool, payload: u32) -> usize {
    table
        .chunks_exact(kinds::ENTRY_LEN)
        .filter(|entry| kind_pool(entry[0]) == Some(is_strg) && entry[5..9] == payload.to_le_bytes())
        .count()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the splice signature mirrors the encode seam's eight-part contract; splitting it would scatter one decision"
)]
fn render_leaf(
    document: &Document<'_>,
    node: NodeId,
    _path: &[String],
    source: &[u8],
    value: &Value,
    _authored: Option<&[u8]>,
    leaf_pool_index: &RefCell<Option<LeafPoolIndex>>,
    _resources: &mut ResourceContext<'_>,
) -> Result<Vec<u8>, CodecError> {
    let Some((node_entry, _node_region, regions, table, table_index, old)) = splice_prelude(document, node, source)?
    else {
        return Err(unrepresentable("this node has no authored tail span"));
    };
    let (new_kind, pool_entry) = leaf_entry(value)?;
    let mut edits: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    // The no-pool kinds' value lives in the node entry's payload field: a bool's 0/1, a null's 0.
    let mut new_payload = match new_kind {
        kinds::NULL => 0,
        kinds::BOOL => match value {
            Value::Bool(flag) => u32::from(*flag),
            _ => return Err(data_contract()),
        },
        _ => old.payload,
    };
    if let Some(entry_bytes) = pool_entry {
        let is_strg = kind_pool(new_kind).ok_or_else(data_contract)?;
        // Equal scalars share ONE pool entry (the builder dedups strg/numb by raw bytes), so an in-place rewrite of the
        // old entry would silently change every sibling whose payload names the same index. Rewrite in place only when
        // this node is that entry's sole referent; a shared entry takes the append/reuse path below.
        let sole_referent = pool_entry_referents(table, is_strg, old.payload) == 1;
        if kind_pool(old.kind) == Some(is_strg) && sole_referent {
            // Same pool, no sibling shares the entry: replace it in place; the node entry's payload (the pool index) is
            // unchanged.
            let chunk = region_by(&regions, if is_strg { jqfb::CHUNK_STRG } else { jqfb::CHUNK_NUMB })
                .ok_or_else(data_contract)?;
            let (start, end) = if is_strg {
                strg_entry_range(chunk, source, old.payload)?
            } else {
                numb_entry_range(chunk, source, old.payload)?
            };
            edits.push((start, end, entry_bytes));
        } else {
            // A different pool, a no-pool kind, or a shared same-pool entry: append the entry — or reuse an existing
            // one — and repoint the node.
            let chunk = region_by(&regions, if is_strg { jqfb::CHUNK_STRG } else { jqfb::CHUNK_NUMB })
                .ok_or_else(data_contract)?;
            if let Some(existing) = LeafPoolIndex::find(leaf_pool_index, chunk, source, &entry_bytes)? {
                new_payload = existing;
            } else {
                let pool = &source[chunk.offset..chunk.end()];
                let count = jqfb::pool_count(pool)?;
                let mut new_count = Vec::new();
                jqfb::push_u64(
                    &mut new_count,
                    u64::try_from(count).map_err(|_| jqfb::invalid("pool count overflows"))? + 1,
                );
                edits.push((chunk.offset, chunk.offset + 8, new_count));
                edits.push((chunk.end(), chunk.end(), entry_bytes));
                new_payload = u32::try_from(count).map_err(|_| jqfb::invalid("pool count overflows"))?;
            }
        }
    }
    let new_entry = node_entry_bytes(new_kind, 1, new_payload);
    let old_entry_bytes = &table[table_index * kinds::ENTRY_LEN..(table_index + 1) * kinds::ENTRY_LEN];
    if new_entry != old_entry_bytes {
        edits.insert(0, (node_entry, node_entry + kinds::ENTRY_LEN, new_entry.to_vec()));
    }
    // The footer and the per-chunk deltas/digests.
    let mut deltas = vec![0i64; regions.len()];
    let mut new_digests: Vec<(usize, [u8; 32])> = Vec::new();
    for (index, region) in regions.iter().enumerate() {
        if let Some((delta, digest)) = region_edit_digest(source, *region, &edits) {
            deltas[index] = delta;
            new_digests.push((index, digest));
        }
    }
    let footer = new_footer(&regions, &deltas, &new_digests)?;
    let footer_start = source.len() - footer_len(source)?;
    edits.sort_by_key(|(start, _, _)| *start);
    // The tail walk: copy from the earliest edit (a pool chunk may sit before NODE), splicing in ascending position,
    // then the new footer.
    let mut tail = Vec::new();
    let first = edits.first().map_or(footer_start, |(start, _, _)| *start);
    let origin = node_entry.min(first);
    tail.extend_from_slice(&source[origin..first]);
    for (index, (_start, end, bytes)) in edits.iter().enumerate() {
        tail.extend_from_slice(bytes);
        let next = edits
            .get(index + 1)
            .map_or(footer_start, |(next_start, _, _)| *next_start);
        tail.extend_from_slice(&source[*end..next]);
    }
    tail.extend_from_slice(&footer);
    Ok(tail)
}

/// Renders the splice for a container the edit lane GREW (the T5 ruling): the new members' node entries are inserted at
/// the end of the container's subtree, the container's own `payload`/`subtree_size` and every ancestor's `subtree_size`
/// are re-derived, genuinely-new pool entries are appended after the existing pools (deduplicated against them), and
/// the footer directory is rewritten with the changed lengths and digests. Returns an empty insertion set (the floor)
/// when the container's region cannot be named.
#[allow(
    clippy::too_many_lines,
    reason = "one container splice keeps its policy, pool emission, and footer bookkeeping explicit"
)]
fn render_edit_append(
    document: &Document<'_>,
    container: NodeId,
    _path: &[String],
    source: &[u8],
    members: EditAppendMembers<'_>,
    resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let Some((node_entry, node_region, regions, table, table_index, entry)) =
        splice_prelude(document, container, source)?
    else {
        return Ok(alloc::vec::Vec::new());
    };
    // Fact-bearing images take the whole-document floor: the splice shifts node-table indices and would repoint the
    // FACT records at wrong nodes.
    if has_fact_records(&regions, source)? {
        return Ok(alloc::vec::Vec::new());
    }
    let (is_object, added) = match members {
        EditAppendMembers::Table(members) => (true, members.len()),
        EditAppendMembers::Array(items) => (false, items.len()),
    };
    let expected_kind = if is_object { kinds::OBJECT } else { kinds::ARRAY };
    if entry.kind != expected_kind {
        return Ok(alloc::vec::Vec::new());
    }
    let subtree_size = entry.subtree_size as usize;
    let old_count = entry.payload as usize;
    // Emit the added members: preload the pools so the emission reuses existing entries, then walk the values into node
    // entries.
    let mut builder = ImageBuilder::new();
    preload_pools(&mut builder, source, &regions)?;
    match members {
        EditAppendMembers::Table(members) => {
            for (key, value) in members {
                let key_index = builder.strg(key.as_bytes())?;
                builder.emit_leaf(kinds::KEYTEXT, key_index);
                walk_owned(&mut builder, value, resources)?;
            }
        }
        EditAppendMembers::Array(items) => {
            for item in items {
                walk_owned(&mut builder, item, resources)?;
            }
        }
    }
    let added_entries = builder.nodes.len() / kinds::ENTRY_LEN;
    let new_subtree = subtree_size
        .checked_add(added_entries)
        .ok_or_else(|| jqfb::invalid("subtree size overflows"))?;
    let new_count = old_count
        .checked_add(added)
        .ok_or_else(|| jqfb::invalid("member count overflows"))?;
    let mut insertions: Vec<EditInsertion> = Vec::new();
    let mut edits: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    let added_entries_u32 = u32::try_from(added_entries).map_err(|_| jqfb::invalid("subtree size overflows"))?;
    // Ancestors' subtree_size fields, then the container's own entry.
    for ancestor in ancestor_chain(table, table_index)? {
        let entry = jqfb::read_node(table, ancestor)?;
        let position = node_region.offset + ancestor * kinds::ENTRY_LEN + 1;
        let new_size = entry
            .subtree_size
            .checked_add(added_entries_u32)
            .ok_or_else(|| jqfb::invalid("subtree size overflows"))?;
        let bytes = new_size.to_le_bytes().to_vec();
        edits.push((position, position + 4, bytes.clone()));
        insertions.push(EditInsertion {
            at: position,
            bytes,
            replace: Some((position, position + 4)),
        });
    }
    let subtree_bytes = u32::try_from(new_subtree)
        .map_err(|_| jqfb::invalid("subtree size overflows"))?
        .to_le_bytes()
        .to_vec();
    let count_bytes = u32::try_from(new_count)
        .map_err(|_| jqfb::invalid("member count overflows"))?
        .to_le_bytes()
        .to_vec();
    edits.push((node_entry + 1, node_entry + 5, subtree_bytes.clone()));
    edits.push((node_entry + 5, node_entry + 9, count_bytes.clone()));
    insertions.push(EditInsertion {
        at: node_entry + 1,
        bytes: subtree_bytes,
        replace: Some((node_entry + 1, node_entry + 5)),
    });
    insertions.push(EditInsertion {
        at: node_entry + 5,
        bytes: count_bytes,
        replace: Some((node_entry + 5, node_entry + 9)),
    });
    // The new members' node entries land at the container's subtree end.
    let insert_at = node_entry + subtree_size * kinds::ENTRY_LEN;
    edits.push((insert_at, insert_at, builder.nodes.clone()));
    insertions.push(EditInsertion {
        at: insert_at,
        bytes: builder.nodes,
        replace: None,
    });
    // The pool appends: count words and entry bytes. The preloaded builder's `strg`/`numb` hold ONLY the genuinely-new
    // entries.
    for (chunk_type, appended, count) in [
        (jqfb::CHUNK_STRG, builder.strg.as_slice(), builder.strg_count),
        (jqfb::CHUNK_NUMB, builder.numb.as_slice(), builder.numb_count),
    ] {
        if appended.is_empty() {
            continue;
        }
        let chunk = region_by(&regions, chunk_type).ok_or_else(data_contract)?;
        let mut new_count = Vec::new();
        jqfb::push_u64(&mut new_count, u64::from(count));
        edits.push((chunk.offset, chunk.offset + 8, new_count.clone()));
        insertions.push(EditInsertion {
            at: chunk.offset,
            bytes: new_count,
            replace: Some((chunk.offset, chunk.offset + 8)),
        });
        edits.push((chunk.end(), chunk.end(), appended.to_vec()));
        insertions.push(EditInsertion {
            at: chunk.end(),
            bytes: appended.to_vec(),
            replace: None,
        });
    }
    // The footer: every changed chunk's digest and every shifted offset.
    let mut deltas = vec![0i64; regions.len()];
    let mut new_digests: Vec<(usize, [u8; 32])> = Vec::new();
    for (index, region) in regions.iter().enumerate() {
        if let Some((delta, digest)) = region_edit_digest(source, *region, &edits) {
            deltas[index] = delta;
            new_digests.push((index, digest));
        }
    }
    insertions.push(footer_insertion(source, &regions, &deltas, &new_digests)?);
    Ok(insertions)
}

/// Renders the cuts for a container the edit lane SHRANK (the T5 ruling): each removed member's node entries are cut —
/// the KEYTEXT entry plus the value's whole subtree for an object member, the item's subtree for an array item — and
/// the container's own `payload`/`subtree_size` and every ancestor's `subtree_size` are re-derived downward. Orphaned
/// pool entries stay (the pools are dedup stores; an unreferenced entry is never read), so no pool chunk is touched.
/// Returns an empty removal set (the floor) when any span the walk needs is missing.
#[allow(
    clippy::too_many_lines,
    reason = "one container splice keeps its policy, member cuts, and footer bookkeeping explicit"
)]
fn render_edit_remove(
    document: &Document<'_>,
    container: NodeId,
    _path: &[String],
    source: &[u8],
    members: EditRemoveMembers<'_>,
    _resources: &mut ResourceContext<'_>,
) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
    let Some((node_entry, node_region, regions, table, table_index, entry)) =
        splice_prelude(document, container, source)?
    else {
        return Ok(alloc::vec::Vec::new());
    };
    // Fact-bearing images take the whole-document floor: the splice shifts node-table indices and would repoint the
    // FACT records at wrong nodes.
    if has_fact_records(&regions, source)? {
        return Ok(alloc::vec::Vec::new());
    }
    let is_object = match members {
        EditRemoveMembers::Table(_) => true,
        EditRemoveMembers::Array(_) => false,
    };
    let expected_kind = if is_object { kinds::OBJECT } else { kinds::ARRAY };
    if entry.kind != expected_kind {
        return Ok(alloc::vec::Vec::new());
    }
    let old_count = entry.payload as usize;
    let mut removals: Vec<EditRemoval> = Vec::new();
    let mut cut_edits: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    let mut removed_entries = 0usize;
    let removed_members = match members {
        EditRemoveMembers::Table(members) => members.len(),
        EditRemoveMembers::Array(items) => items.len(),
    };
    match members {
        EditRemoveMembers::Table(members) => {
            for (_, node) in members {
                let Some(member_span) = document.node_source_span(*node).map_err(map_data)? else {
                    return Ok(alloc::vec::Vec::new());
                };
                let value_entry = member_span.start() as usize;
                if member_span.end() as usize != source.len() {
                    return Ok(alloc::vec::Vec::new());
                }
                let value_index = value_entry
                    .checked_sub(node_region.offset)
                    .ok_or_else(|| jqfb::invalid("member entry lies before the NODE chunk"))?
                    / kinds::ENTRY_LEN;
                if value_index == 0 {
                    return Ok(alloc::vec::Vec::new());
                }
                let value = jqfb::read_node(table, value_index)?;
                let end = value_entry + value.subtree_size as usize * kinds::ENTRY_LEN;
                // Object members are KEYTEXT then value in preorder: the cut covers the key entry through the value
                // subtree's last byte.
                let key_start = value_entry - kinds::ENTRY_LEN;
                cut_edits.push((key_start, end, Vec::new()));
                removals.push(EditRemoval {
                    start: key_start,
                    end,
                    replacement: Vec::new(),
                });
                removed_entries += 1 + value.subtree_size as usize;
            }
        }
        EditRemoveMembers::Array(items) => {
            for (_, node) in items {
                let Some(member_span) = document.node_source_span(*node).map_err(map_data)? else {
                    return Ok(alloc::vec::Vec::new());
                };
                let item_entry = member_span.start() as usize;
                if member_span.end() as usize != source.len() {
                    return Ok(alloc::vec::Vec::new());
                }
                let item = jqfb::read_node(
                    table,
                    item_entry
                        .checked_sub(node_region.offset)
                        .ok_or_else(|| jqfb::invalid("member entry lies before the NODE chunk"))?
                        / kinds::ENTRY_LEN,
                )?;
                let end = item_entry + item.subtree_size as usize * kinds::ENTRY_LEN;
                cut_edits.push((item_entry, end, Vec::new()));
                removals.push(EditRemoval {
                    start: item_entry,
                    end,
                    replacement: Vec::new(),
                });
                removed_entries += item.subtree_size as usize;
            }
        }
    }
    let removed_entries_u32 = u32::try_from(removed_entries).map_err(|_| jqfb::invalid("subtree size underflows"))?;
    let new_subtree = entry
        .subtree_size
        .checked_sub(removed_entries_u32)
        .ok_or_else(|| jqfb::invalid("subtree size underflows"))?;
    let new_count = old_count
        .checked_sub(removed_members)
        .ok_or_else(|| jqfb::invalid("member count underflows"))?;
    // The edits vector must be SORTED for `splice_chunk`: the count-bearing entry updates land before the member cuts
    // (the cuts sit deeper in the container's subtree), so they are built first.
    let mut edits: Vec<(usize, usize, Vec<u8>)> = Vec::new();
    for ancestor in ancestor_chain(table, table_index)? {
        let entry = jqfb::read_node(table, ancestor)?;
        let position = node_region.offset + ancestor * kinds::ENTRY_LEN + 1;
        let new_size = entry
            .subtree_size
            .checked_sub(removed_entries_u32)
            .ok_or_else(|| jqfb::invalid("subtree size underflows"))?;
        let bytes = new_size.to_le_bytes().to_vec();
        edits.push((position, position + 4, bytes.clone()));
        removals.push(EditRemoval {
            start: position,
            end: position + 4,
            replacement: bytes,
        });
    }
    let subtree_bytes = new_subtree.to_le_bytes().to_vec();
    let count_bytes = u32::try_from(new_count)
        .map_err(|_| jqfb::invalid("member count underflows"))?
        .to_le_bytes()
        .to_vec();
    edits.push((node_entry + 1, node_entry + 5, subtree_bytes.clone()));
    edits.push((node_entry + 5, node_entry + 9, count_bytes.clone()));
    removals.push(EditRemoval {
        start: node_entry + 1,
        end: node_entry + 5,
        replacement: subtree_bytes,
    });
    removals.push(EditRemoval {
        start: node_entry + 5,
        end: node_entry + 9,
        replacement: count_bytes,
    });
    edits.extend(cut_edits);
    // The footer: the NODE chunk shrank by the removed entries; every later chunk's offset shifts with it.
    let mut deltas = vec![0i64; regions.len()];
    let mut new_digests: Vec<(usize, [u8; 32])> = Vec::new();
    for (index, region) in regions.iter().enumerate() {
        if let Some((delta, digest)) = region_edit_digest(source, *region, &edits) {
            deltas[index] = delta;
            new_digests.push((index, digest));
        }
    }
    removals.push(EditRemoval {
        start: source.len() - footer_len(source)?,
        end: source.len(),
        replacement: new_footer(&regions, &deltas, &new_digests)?,
    });
    Ok(removals)
}

#[cfg(test)]
mod tests {
    use super::owned_fact_payload;
    use jqf_codec_core::CodecFailureKind;
    use jqf_data::FactPayloadView;

    #[test]
    fn an_unparseable_fact_number_payload_is_a_clean_error() {
        // A fact Integer/Decimal whose spelling does not parse is a malformed document shape: the encoder fails with a
        // named representation error instead of silently publishing zero.
        let error = owned_fact_payload(&FactPayloadView::Integer("not-a-number")).expect_err("unparseable integer");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
        let error = owned_fact_payload(&FactPayloadView::Decimal {
            coefficient: "12x",
            scale: 2,
        })
        .expect_err("unparseable decimal");
        assert_eq!(error.kind(), CodecFailureKind::UnsupportedRepresentation);
        // The valid spellings still convert.
        assert!(owned_fact_payload(&FactPayloadView::Integer("42")).is_ok());
        assert!(
            owned_fact_payload(&FactPayloadView::Decimal {
                coefficient: "12",
                scale: 2,
            })
            .is_ok()
        );
    }
}
