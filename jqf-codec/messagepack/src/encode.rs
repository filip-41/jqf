//! The deterministic `MessagePack` encoder and the edit lane's three splice hooks.
//!
//! `messagepack.deterministic@1` emits the SHORTEST EXACT marker and length family for every value: nonnegative
//! integers → positive fixint then the narrowest unsigned field; negative integers → negative fixint then the
//! narrowest signed field; `str`/`bin`/array/map counts → the fix family then the narrowest explicit field; floats
//! → float32 exactly when converting to float32 and back recreates the exact finite value, infinity, or signed zero,
//! and float64 otherwise. **Every** NaN, any width/sign/payload, becomes `0xca 0x7fc0_0000`, with that observable loss
//! reported in the item's [`PreservationReport`] at completion (an item larger than the offer window has already
//! flushed earlier bytes by then). Map occurrence order is PRESERVED (no sorting, no permutation ledger).
//!
//! ## Value laws
//!
//! - `OffsetDateTime` → extension `-1`, the shortest of the three encodings (32-bit `0xd6 0xff`; 64-bit `0xd7 0xff`
//!   with `(nanoseconds << 34) | seconds`; 96-bit `0xc7 0x0c 0xff` with `u32` nanoseconds then signed `i64` seconds). A
//!   nonzero or unknown offset, a leap-second label 60, or a fraction over nine digits is Unrepresentable, never
//!   normalized.
//! - Extension reverse-mapping: `msgpack:ext:<n>` for every signed 8-bit `n` except `-1` chooses fixext 1/2/4/8/16 or
//!   the shortest ext8/16/32 with the byte-string payload; `-1` reverses ONLY from an object with exactly the ordered
//!   core Integer members `seconds` then `nanoseconds` whose instant is OUT of the core range — a wrapper
//!   representable as the core timestamp is contradictory non-core storage and is rejected. A foreign   `TagId` is
//!   never dropped.
//! - **A `Decimal` is refused terminally, naming the value and its path**: `MessagePack` has no decimal type and no
//!   standard ext for one, so the honest answer is a refusal, not a quiet rounding. The
//!   `messagepack.deterministic-float64@1` opt-in is the ONE deliberate divergence: it encodes a `Decimal` as its
//!   nearest IEEE-754 binary64 float (the precision loss is in the dialect's identity). Every other value keeps the
//!   deterministic encoding under both profiles. An `Integer` outside `[-2^63, 2^64-1]` is Unrepresentable, and the
//!   three local temporal kinds are Unrepresentable.
//! - The COMPLETE result is reparsed under `messagepack.utf8@1` before publication (the reparse-before-publish law): a
//!   result the decoder cannot read back as one valid object is unrepresentable.
//!
//! ## The edit tier — leaf re-encode, structural decline
//!
//! `MessagePack`'s splice policy is SIMPLER than text's: fixed-width headers, no whitespace, no comments, no
//! re-quoting. The three hooks follow the same binary pattern as CBOR — per-item span binding in the parse (every
//! item's header-through-payload extent), the hooks here, the policy in this doc.
//!
//! - [`EncoderFactoryImpl::render_leaf`] re-encodes the new value with the deterministic grammar: a leaf VALUE in
//!   `MessagePack` IS a complete self-delimiting item, so its bytes replace the authored span exactly.
//! - [`EncoderFactoryImpl::render_edit_append`] and [`EncoderFactoryImpl::render_edit_remove`] ALWAYS DECLINE: every
//!   map or array growth/shrink changes the container's count-bearing header (a fixmap `0x81` → `0x82`, a map16 count
//!   field), and the append seam can only INSERT bytes while the removal seam can only CUT them — the header rewrite
//!   is unexpressible at either seam. Returning empty hands the edit to the whole-document floor, which for the
//!   deterministic profile emits exactly the bytes a perfect splice would (occurrence order preserved, shortest forms).
//!   The bookkeeping IS the policy: a splice that cannot prove its header math never publishes.
//!
//! The verification law is the caller's: every patched byte string is re-decoded, and a declined or wrong splice falls
//! back to the whole-document floor rather than corrupting the file.

use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EncodeItem, EncodeRequest, EncoderFactoryImpl, EncoderSession, ErasedEncoderFactory,
    ErasedEncoderSession, PhysicalRouteId, PreservationOutcome, PreservationReport, PreservationRequest,
    RecycledSessionState,
};
use jqf_data::{
    Document, IntrinsicTagSemantics, Number, NumberView, Object, OffsetDateTime, ScalarView, UtcOffset, Value,
    ValueKind, ValueView,
};
use jqf_resource::{ResourceContext, WorkAdmission};

use crate::options::Dialect;
use crate::tag::parse_canonical_type;
use crate::{error, scan};

const OFFER_BYTES: usize = 16 * 1024;

/// Builds the encoder factory for `messagepack.deterministic@1` (the exact default) and
/// `messagepack.deterministic-float64@1` (the lossy opt-in that encodes a `Decimal` as its nearest binary64 float).
pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    if request.format.as_str() != crate::FORMAT_ID {
        return Err(CodecError::new(CodecFailureKind::RequirementMismatch));
    }
    let float64_decimals = match request.dialect.as_str() {
        crate::MESSAGEPACK_DETERMINISTIC_DIALECT_ID => false,
        crate::MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID => true,
        _ => return Err(CodecError::new(CodecFailureKind::RequirementMismatch)),
    };
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || {
        Ok(MessagepackEncoderFactory { float64_decimals })
    })
}

struct MessagepackEncoderFactory {
    /// `messagepack.deterministic-float64@1`: a `Decimal` encodes as its nearest binary64 float instead of refusing.
    float64_decimals: bool,
}

impl EncoderFactoryImpl for MessagepackEncoderFactory {
    fn physical_encoder(&self) -> PhysicalRouteId {
        crate::encode_route_id()
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        ErasedEncoderSession::try_new(item, preservation, || {
            Ok(MessagepackEncoder::with_float64_decimals(self.float64_decimals))
        })
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<MessagepackEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }

    fn render_leaf(
        &self,
        _document: &jqf_data::Document<'_>,
        _node: jqf_data::NodeId,
        path: &[String],
        _source: &[u8],
        value: &Value,
        _authored: Option<&[u8]>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        // A leaf VALUE is a complete self-delimiting item, so the deterministic grammar's bytes replace the authored
        // span exactly. The caller re-decodes the patched document and falls back to the floor on any doubt.
        let mut encoder = MessagepackEncoder::with_float64_decimals(self.float64_decimals);
        let mut crumbs: Vec<PathStep<'_>> = path.iter().map(|segment| PathStep::Key(segment.as_str())).collect();
        encoder.encode_value(value, &mut crumbs)?;
        Ok(encoder.bytes)
    }

    fn render_edit_append(
        &self,
        _document: &jqf_data::Document<'_>,
        _container: jqf_data::NodeId,
        _path: &[String],
        _source: &[u8],
        _members: EditAppendMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<EditInsertion>, CodecError> {
        // EVERY growth changes the container's count header (a fixmap 0x81 → 0x82, a map16 count field, …), and the
        // append seam can only INSERT bytes — the header rewrite is unexpressible, so the splice declines
        // unconditionally and the whole-document floor re-encodes (identical bytes for the deterministic profile:
        // occurrence order preserved, shortest forms).
        Ok(Vec::new())
    }

    fn render_edit_remove(
        &self,
        _document: &jqf_data::Document<'_>,
        _container: jqf_data::NodeId,
        _path: &[String],
        _source: &[u8],
        _members: EditRemoveMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<EditRemoval>, CodecError> {
        // The mirror of the append ruling: EVERY shrink changes the count header (0x82 → 0x81, a map16 count field,
        // …), and the removal seam can only CUT bytes — the splice declines unconditionally and the floor
        // re-encodes.
        Ok(Vec::new())
    }
}

struct MessagepackEncoder {
    bytes: Vec<u8>,
    root_done: bool,
    /// Whether a NaN was encoded: every NaN becomes `0xca 0x7fc0_0000`, and that observable loss is reported in the
    /// item's completion report.
    saw_nan: bool,
    /// `messagepack.deterministic-float64@1`: a `Decimal` encodes as its nearest binary64 float instead of refusing.
    float64_decimals: bool,
}

impl MessagepackEncoder {
    fn with_float64_decimals(float64_decimals: bool) -> Self {
        Self {
            bytes: Vec::new(),
            root_done: false,
            saw_nan: false,
            float64_decimals,
        }
    }

    fn reset(&mut self) {
        self.bytes.clear();
        self.root_done = false;
        self.saw_nan = false;
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn encode_item(&mut self, item: EncodeItem<'_, '_>, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        self.saw_nan = false;
        match item {
            EncodeItem::Owned(value) => {
                let mut path: Vec<PathStep<'_>> = Vec::new();
                self.encode_value(value, &mut path)?;
            }
            EncodeItem::Located { product, node } => {
                let document = product.document();
                let view = document.value_view(node).map_err(map_data)?;
                let mut path: Vec<PathStep<'_>> = Vec::new();
                self.encode_view(document, view, &mut path)?;
            }
        }
        self.reparse(resources)?;
        Ok(())
    }

    /// Reparses the staged bytes under `messagepack.utf8@1` before publication (the reparse-before-publish law). A
    /// result the decoder cannot read back as one valid object is unrepresentable.
    fn reparse(&mut self, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        let staged = self.bytes.as_slice();
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(88), jqf_source::SourceKind::Input),
            "messagepack.encode",
            staged,
            0,
        );
        let mut run = CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        scan::scan(source, Dialect::Utf8, &mut run).map(|_| ()).map_err(|_| {
            error::encode_unrepresentable("the encoded result cannot be read back under messagepack.utf8@1")
        })
    }

    /// Encodes one complete value with the deterministic grammar.
    fn encode_value<'a>(&mut self, value: &'a Value, path: &mut Vec<PathStep<'a>>) -> Result<(), CodecError> {
        match value {
            Value::Null => {
                self.push(&[0xc0]);
                Ok(())
            }
            Value::Bool(true) => {
                self.push(&[0xc3]);
                Ok(())
            }
            Value::Bool(false) => {
                self.push(&[0xc2]);
                Ok(())
            }
            Value::Number(number) => self.encode_number(number, path),
            Value::String(text) => self.encode_text(text.as_str()),
            Value::Bytes(bytes) => {
                self.push_len(LenFamily::Bin, bytes.len() as u64)?;
                self.push(bytes);
                Ok(())
            }
            Value::Array(array) => {
                self.push_len(LenFamily::Array, array.len() as u64)?;
                for (index, item) in array.iter().enumerate() {
                    path.push(PathStep::Index(index));
                    self.encode_value(item, path)?;
                    path.pop();
                }
                Ok(())
            }
            Value::Object(object) => {
                // Map occurrence order PRESERVED: no sorting, no permutation ledger.
                self.push_len(LenFamily::Map, object.len() as u64)?;
                for entry in object {
                    let key = entry.key();
                    self.encode_text(key)?;
                    path.push(PathStep::Key(key));
                    self.encode_value(entry.value(), path)?;
                    path.pop();
                }
                Ok(())
            }
            Value::OffsetDateTime(datetime) => self.encode_timestamp(datetime),
            Value::LocalDate(_) | Value::LocalTime(_) | Value::LocalDateTime(_) => Err(unrepresentable(
                "a local date, time, or date-time is unrepresentable in MessagePack",
            )),
            Value::Tagged { tag, payload } => {
                let Some(rest) = tag.as_str().strip_prefix("msgpack:ext:") else {
                    return Err(unrepresentable("a foreign tag is never dropped"));
                };
                let Some(ty) = parse_canonical_type(rest) else {
                    return Err(unrepresentable("a non-canonical msgpack extension spelling is refused"));
                };
                if ty == -1 {
                    // The timestamp reverse-mapping: ONLY from an object with exactly the ordered core Integer members
                    // `seconds` then `nanoseconds` whose instant is OUT of the core range.
                    let Value::Object(object) = &**payload else {
                        return Err(unrepresentable(
                            "a msgpack:ext:-1 wrapper must carry an object of exactly {seconds, nanoseconds}",
                        ));
                    };
                    let Some((seconds, nanoseconds)) = extract_timestamp_object(object) else {
                        return Err(unrepresentable(
                            "a msgpack:ext:-1 wrapper must carry exactly the ordered Integer members seconds then nanoseconds",
                        ));
                    };
                    if crate::materialize::epoch_to_offset(seconds, nanoseconds).is_some() {
                        return Err(unrepresentable(
                            "a msgpack:ext:-1 wrapper whose instant the core timestamp represents is contradictory non-core storage",
                        ));
                    }
                    self.push_timestamp_payload(seconds, nanoseconds);
                    return Ok(());
                }
                let Value::Bytes(bytes) = &**payload else {
                    return Err(unrepresentable("a msgpack extension payload must be a byte string"));
                };
                self.push_ext(ty, bytes);
                Ok(())
            }
        }
    }

    /// Encodes a located item directly from the document view.
    fn encode_view<'d, 's>(
        &mut self,
        document: &'d Document<'s>,
        view: ValueView<'d, 's>,
        path: &mut Vec<PathStep<'d>>,
    ) -> Result<(), CodecError> {
        if view.tag_semantics().map_err(map_data)? == Some(IntrinsicTagSemantics::Tagged) {
            let tag = view
                .tag()
                .map_err(map_data)?
                .ok_or_else(|| unrepresentable("a tagged view must carry a tag"))?;
            return self.encode_view_tag(document, tag.as_str(), view, path);
        }
        match view.kind().map_err(map_data)? {
            ValueKind::Null => {
                self.push(&[0xc0]);
                Ok(())
            }
            ValueKind::Bool => {
                let ScalarView::Bool(value) = view
                    .scalar()
                    .map_err(map_data)?
                    .ok_or_else(|| unrepresentable("bool view"))?
                else {
                    return Err(unrepresentable("bool view"));
                };
                self.push(if value { &[0xc3] } else { &[0xc2] });
                Ok(())
            }
            ValueKind::Number => {
                let ScalarView::Number(number) = view
                    .scalar()
                    .map_err(map_data)?
                    .ok_or_else(|| unrepresentable("number view"))?
                else {
                    return Err(unrepresentable("number view"));
                };
                self.encode_number_view(number, path)
            }
            ValueKind::String => {
                let ScalarView::String(text) = view
                    .scalar()
                    .map_err(map_data)?
                    .ok_or_else(|| unrepresentable("string view"))?
                else {
                    return Err(unrepresentable("string view"));
                };
                self.encode_text(text)
            }
            ValueKind::Bytes => {
                let ScalarView::Bytes(bytes) = view
                    .scalar()
                    .map_err(map_data)?
                    .ok_or_else(|| unrepresentable("bytes view"))?
                else {
                    return Err(unrepresentable("bytes view"));
                };
                self.push_len(LenFamily::Bin, bytes.len() as u64)?;
                self.push(bytes);
                Ok(())
            }
            ValueKind::Array => {
                let array = view
                    .array()
                    .map_err(map_data)?
                    .ok_or_else(|| unrepresentable("array view"))?;
                self.push_len(LenFamily::Array, array.len() as u64)?;
                for index in 0..array.len() {
                    let item = array.get(index).ok_or_else(|| unrepresentable("array index"))?;
                    path.push(PathStep::Index(index));
                    self.encode_view(document, item, path)?;
                    path.pop();
                }
                Ok(())
            }
            ValueKind::Object => {
                let object = view
                    .object()
                    .map_err(map_data)?
                    .ok_or_else(|| unrepresentable("object view"))?;
                self.push_len(LenFamily::Map, object.len() as u64)?;
                for index in 0..object.len() {
                    let entry = object
                        .get_index(index)
                        .map_err(map_data)?
                        .ok_or_else(|| unrepresentable("object index"))?;
                    let key = entry.key();
                    self.encode_text(key)?;
                    path.push(PathStep::Key(key));
                    self.encode_view(document, entry.value(), path)?;
                    path.pop();
                }
                Ok(())
            }
            ValueKind::OffsetDateTime => {
                let datetime = located_offset_datetime(view)?;
                self.encode_timestamp(&datetime)
            }
            ValueKind::LocalDate | ValueKind::LocalTime | ValueKind::LocalDateTime => Err(unrepresentable(
                "a local date, time, or date-time is unrepresentable in MessagePack",
            )),
        }
    }

    fn encode_view_tag<'d, 's>(
        &mut self,
        document: &'d Document<'s>,
        tag: &str,
        view: ValueView<'d, 's>,
        path: &mut Vec<PathStep<'d>>,
    ) -> Result<(), CodecError> {
        let Some(rest) = tag.strip_prefix("msgpack:ext:") else {
            return Err(unrepresentable("a foreign tag is never dropped"));
        };
        let Some(ty) = parse_canonical_type(rest) else {
            return Err(unrepresentable("a non-canonical msgpack extension spelling is refused"));
        };
        let payload = document
            .tag_payload(view.node())
            .map_err(map_data)?
            .ok_or_else(|| unrepresentable("tagged view payload"))?;
        let handle = document.node_handle(payload).map_err(map_data)?;
        let payload_view = document.value_view(handle).map_err(map_data)?;
        if ty == -1 {
            let object = payload_view.object().map_err(map_data)?.ok_or_else(|| {
                unrepresentable("a msgpack:ext:-1 wrapper must carry an object of exactly {seconds, nanoseconds}")
            })?;
            let Some((seconds, nanoseconds)) = extract_timestamp_object_view(object) else {
                return Err(unrepresentable(
                    "a msgpack:ext:-1 wrapper must carry exactly the ordered Integer members seconds then nanoseconds",
                ));
            };
            if crate::materialize::epoch_to_offset(seconds, nanoseconds).is_some() {
                return Err(unrepresentable(
                    "a msgpack:ext:-1 wrapper whose instant the core timestamp represents is contradictory non-core storage",
                ));
            }
            self.push_timestamp_payload(seconds, nanoseconds);
            return Ok(());
        }
        let ScalarView::Bytes(bytes) = payload_view
            .scalar()
            .map_err(map_data)?
            .ok_or_else(|| unrepresentable("a msgpack extension payload must be a byte string"))?
        else {
            return Err(unrepresentable("a msgpack extension payload must be a byte string"));
        };
        let _ = path;
        self.push_ext(ty, bytes);
        Ok(())
    }

    fn encode_number_view(&mut self, number: NumberView<'_>, path: &[PathStep<'_>]) -> Result<(), CodecError> {
        match number {
            NumberView::Number(number) => self.encode_number(number, path),
            NumberView::Integer(spelling) => {
                if let Ok(value) = spelling.parse::<i64>() {
                    self.push_signed(value);
                    return Ok(());
                }
                self.push_big_integer(spelling, path)
            }
            NumberView::Decimal { coefficient, scale } => {
                if self.float64_decimals {
                    // The float64 dialect's one divergence: the nearest binary64 (the precision loss is in the
                    // identity).
                    let value = jqf_data::decimal_parts_to_f64(coefficient, scale);
                    self.push_float(value.to_bits());
                    return Ok(());
                }
                Err(error::encode_unrepresentable(&alloc::format!(
                    "a decimal value {} at path {} is unrepresentable in messagepack.deterministic@1 (MessagePack has no decimal type)",
                    decimal_spelling(coefficient, scale),
                    render_path(path),
                )))
            }
            NumberView::Float(float) => {
                self.push_float(float.bits());
                Ok(())
            }
        }
    }

    /// Encodes one number: the shortest exact integer family, float32-when- exact / float64, or the Decimal refusal.
    fn encode_number(&mut self, number: &Number, path: &[PathStep<'_>]) -> Result<(), CodecError> {
        if let Some(machine) = number.as_machine() {
            self.push_signed(machine);
            return Ok(());
        }
        if let Some(value) = number.to_i64() {
            self.push_signed(value);
            return Ok(());
        }
        if let Some(integer) = number.as_integer() {
            return self.push_big_integer(integer.as_str(), path);
        }
        if let Some(float) = number.as_float() {
            self.push_float(float.bits());
            return Ok(());
        }
        if let Some(decimal) = number.as_decimal() {
            if self.float64_decimals {
                // The float64 dialect's one divergence: the nearest binary64 (the precision loss is in the identity).
                let value = decimal.to_f64();
                self.push_float(value.to_bits());
                return Ok(());
            }
            // MessagePack has no decimal type and no standard ext for one; a Decimal is refused terminally, naming the
            // value and its path — never a quiet rounding.
            let coefficient = decimal.coefficient().as_str();
            let scale = decimal.scale();
            return Err(error::encode_unrepresentable(&alloc::format!(
                "a decimal value {} at path {} is unrepresentable in messagepack.deterministic@1 (MessagePack has no decimal type)",
                decimal_spelling(coefficient, scale),
                render_path(path),
            )));
        }
        Err(unrepresentable("a number carrying no representation"))
    }

    /// Encodes a boxed integer beyond the machine range (an integer outside `[-2^63, 2^64-1]` is Unrepresentable).
    fn push_big_integer(&mut self, text: &str, path: &[PathStep<'_>]) -> Result<(), CodecError> {
        let value: i128 = text.parse().map_err(|_| {
            error::encode_unrepresentable(&alloc::format!(
                "integer {text} at path {} is not a valid integer",
                render_path(path),
            ))
        })?;
        if value >= 0 {
            if value <= u64::MAX as i128 {
                self.push_unsigned(value as u64);
                Ok(())
            } else {
                Err(error::encode_unrepresentable(&alloc::format!(
                    "integer {text} at path {} exceeds the MessagePack unsigned range",
                    render_path(path),
                )))
            }
        } else if value >= i64::MIN as i128 {
            self.push_signed(value as i64);
            Ok(())
        } else {
            Err(error::encode_unrepresentable(&alloc::format!(
                "integer {text} at path {} is below the MessagePack signed range",
                render_path(path),
            )))
        }
    }

    /// Encodes one float: float32 exactly when converting to float32 and back recreates the exact finite value,
    /// infinity, or signed zero; otherwise float64. Every NaN becomes `0xca 0x7fc0_0000` with the loss reported.
    fn push_float(&mut self, bits: u64) {
        let value = f64::from_bits(bits);
        if value.is_nan() {
            self.saw_nan = true;
            self.push(&[0xca, 0x7f, 0xc0, 0x00, 0x00]);
            return;
        }
        let narrowed = value as f32;
        if f64::from(narrowed).to_bits() == bits {
            self.push(&[0xca]);
            self.push(&narrowed.to_bits().to_be_bytes());
        } else {
            self.push(&[0xcb]);
            self.push(&bits.to_be_bytes());
        }
    }

    /// Encodes one signed integer with the shortest exact family: negative fixint, then the narrowest signed field;
    /// nonnegative, positive fixint then the narrowest unsigned field.
    fn push_signed(&mut self, value: i64) {
        if value >= 0 {
            self.push_unsigned(value as u64);
        } else if value >= -32 {
            self.push(&[(value + 32) as u8 | 0xe0]);
        } else if value >= i8::MIN as i64 {
            self.push(&[0xd0, value as i8 as u8]);
        } else if value >= i16::MIN as i64 {
            self.push(&[0xd1]);
            self.push(&(value as i16).to_be_bytes());
        } else if value >= i32::MIN as i64 {
            self.push(&[0xd2]);
            self.push(&(value as i32).to_be_bytes());
        } else {
            self.push(&[0xd3]);
            self.push(&value.to_be_bytes());
        }
    }

    fn push_unsigned(&mut self, value: u64) {
        if value <= 0x7f {
            self.push(&[value as u8]);
        } else if value <= 0xff {
            self.push(&[0xcc, value as u8]);
        } else if value <= 0xffff {
            self.push(&[0xcd]);
            self.push(&(value as u16).to_be_bytes());
        } else if u32::try_from(value).is_ok() {
            self.push(&[0xce]);
            self.push(&(value as u32).to_be_bytes());
        } else {
            self.push(&[0xcf]);
            self.push(&value.to_be_bytes());
        }
    }

    /// Encodes one text string: fixstr, str8, str16, or str32 (shortest).
    fn encode_text(&mut self, text: &str) -> Result<(), CodecError> {
        self.push_len(LenFamily::Str, text.len() as u64)?;
        self.push(text.as_bytes());
        Ok(())
    }

    /// Pushes the shortest marker + length field for one family.
    fn push_len(&mut self, family: LenFamily, count: u64) -> Result<(), CodecError> {
        if count > u32::MAX as u64 {
            return Err(unrepresentable("a count exceeding 2^32-1 is unrepresentable"));
        }
        match family {
            LenFamily::Str => {
                if count <= 31 {
                    self.push(&[0xa0 | count as u8]);
                } else if count <= 0xff {
                    self.push(&[0xd9, count as u8]);
                } else if count <= 0xffff {
                    self.push(&[0xda]);
                    self.push(&(count as u16).to_be_bytes());
                } else {
                    self.push(&[0xdb]);
                    self.push(&(count as u32).to_be_bytes());
                }
            }
            LenFamily::Bin => {
                if count <= 0xff {
                    self.push(&[0xc4, count as u8]);
                } else if count <= 0xffff {
                    self.push(&[0xc5]);
                    self.push(&(count as u16).to_be_bytes());
                } else {
                    self.push(&[0xc6]);
                    self.push(&(count as u32).to_be_bytes());
                }
            }
            LenFamily::Array => {
                if count <= 15 {
                    self.push(&[0x90 | count as u8]);
                } else if count <= 0xffff {
                    self.push(&[0xdc]);
                    self.push(&(count as u16).to_be_bytes());
                } else {
                    self.push(&[0xdd]);
                    self.push(&(count as u32).to_be_bytes());
                }
            }
            LenFamily::Map => {
                if count <= 15 {
                    self.push(&[0x80 | count as u8]);
                } else if count <= 0xffff {
                    self.push(&[0xde]);
                    self.push(&(count as u16).to_be_bytes());
                } else {
                    self.push(&[0xdf]);
                    self.push(&(count as u32).to_be_bytes());
                }
            }
        }
        Ok(())
    }

    /// Encodes one `OffsetDateTime` as extension `-1`, the shortest of the three forms.
    fn encode_timestamp(&mut self, datetime: &OffsetDateTime) -> Result<(), CodecError> {
        match datetime.offset {
            UtcOffset::KnownSeconds(known) if known.seconds() == 0 => {}
            _ => {
                return Err(unrepresentable(
                    "a timestamp with a nonzero or unknown offset is unrepresentable",
                ));
            }
        }
        if datetime.local.time.second() == 60 {
            return Err(unrepresentable(
                "a leap-second label 60 is unrepresentable in a MessagePack timestamp",
            ));
        }
        let fraction = datetime.local.time.fraction().digits();
        let nanoseconds = fraction_nanoseconds(fraction).ok_or_else(|| {
            unrepresentable("a fraction over nine digits is unrepresentable in a MessagePack timestamp")
        })?;
        // The checked twin: an out-of-domain civil field is a typed refusal here, never a silently wrapped timestamp.
        let seconds = jqf_data::try_epoch_seconds_from_civil_parts(
            i64::from(datetime.local.date.year()),
            i64::from(datetime.local.date.month()),
            i64::from(datetime.local.date.day()),
            i64::from(datetime.local.time.hour()),
            i64::from(datetime.local.time.minute()),
            i64::from(datetime.local.time.second()),
            0,
        )
        .ok_or_else(|| unrepresentable("a timestamp outside the representable civil window"))?;
        self.push_timestamp_payload(seconds, nanoseconds);
        Ok(())
    }

    /// Pushes extension `-1`'s payload for `(seconds, nanoseconds)`, the shortest of the three encodings.
    fn push_timestamp_payload(&mut self, seconds: i64, nanoseconds: u32) {
        if nanoseconds == 0 && (0..=u32::MAX as i64).contains(&seconds) {
            self.push(&[0xd6, 0xff]);
            self.push(&(seconds as u32).to_be_bytes());
        } else if (0..=0x3_ffff_ffff).contains(&seconds) {
            self.push(&[0xd7, 0xff]);
            let value = (u64::from(nanoseconds) << 34) | seconds as u64;
            self.push(&value.to_be_bytes());
        } else {
            self.push(&[0xc7, 0x0c, 0xff]);
            self.push(&nanoseconds.to_be_bytes());
            self.push(&seconds.to_be_bytes());
        }
    }

    /// Pushes one extension: fixext 1/2/4/8/16 or the shortest ext8/16/32, then the payload.
    fn push_ext(&mut self, ty: i8, payload: &[u8]) {
        let len = payload.len();
        match len {
            1 => self.push(&[0xd4, ty as u8]),
            2 => self.push(&[0xd5, ty as u8]),
            4 => self.push(&[0xd6, ty as u8]),
            8 => self.push(&[0xd7, ty as u8]),
            16 => self.push(&[0xd8, ty as u8]),
            0..=255 => self.push(&[0xc7, len as u8, ty as u8]),
            256..=65535 => {
                self.push(&[0xc8]);
                self.push(&(len as u16).to_be_bytes());
                self.push(&[ty as u8]);
            }
            _ => {
                self.push(&[0xc9]);
                self.push(&(len as u32).to_be_bytes());
                self.push(&[ty as u8]);
            }
        }
        self.push(payload);
    }
}

impl EncoderSession for MessagepackEncoder {
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
                return Ok(report(self.saw_nan));
            }
            if self.bytes.len() >= OFFER_BYTES {
                sink.write_all(&self.bytes, context.resources())?;
                self.bytes.clear();
                continue;
            }
            let remaining = context.resources().remaining_work() as usize;
            match context.resources().admit_work_transitions(remaining.max(1))? {
                WorkAdmission::Pending => context.replenish_work()?,
                WorkAdmission::Granted(granted) => {
                    for _ in 0..granted {
                        if self.root_done || self.bytes.len() >= OFFER_BYTES {
                            break;
                        }
                        self.encode_item(item, context.resources())?;
                        self.root_done = true;
                    }
                }
            }
        }
    }
}

/// The `MessagePack` preservation evidence: semantic values are exact unless a NaN was collapsed to `0xca 0x7fc0_0000`
/// (then Normalized, reported with the item's completion); ordering is exact (occurrence order preserved); the result
/// is a semantic re-encode, never a source echo.
const fn report(saw_nan: bool) -> PreservationReport {
    PreservationReport::new(
        if saw_nan {
            PreservationOutcome::Normalized
        } else {
            PreservationOutcome::Exact
        },
        PreservationOutcome::Exact,
        PreservationOutcome::Exact,
        PreservationOutcome::Omitted,
    )
}

/// Which length family a header belongs to.
#[derive(Clone, Copy)]
enum LenFamily {
    Str,
    Bin,
    Array,
    Map,
}

/// Extracts the ordered `{seconds, nanoseconds}` Integer members of an out-of-range timestamp object.
fn located_offset_datetime(view: ValueView<'_, '_>) -> Result<OffsetDateTime, CodecError> {
    let ScalarView::OffsetDateTime(value) = view
        .scalar()
        .map_err(map_data)?
        .ok_or_else(|| unrepresentable("datetime view"))?
    else {
        return Err(unrepresentable("datetime view"));
    };
    let time = jqf_data::LocalTime::new(
        value.local.time.hour,
        value.local.time.minute,
        value.local.time.second,
        jqf_data::FractionalSecond::parse(value.local.time.fraction)
            .map_err(|_| unrepresentable("datetime fraction"))?,
    )
    .ok_or_else(|| unrepresentable("datetime time"))?;
    Ok(OffsetDateTime {
        local: jqf_data::LocalDateTime {
            date: value.local.date,
            time,
        },
        offset: value.offset,
    })
}

fn extract_timestamp_object_view(object: jqf_data::ObjectView<'_, '_>) -> Option<(i64, u32)> {
    if object.len() != 2 {
        return None;
    }
    let first = object.get_index(0).ok()??;
    let second = object.get_index(1).ok()??;
    if first.key() != "seconds" || second.key() != "nanoseconds" {
        return None;
    }
    let seconds = integer_i64_view(first.value())?;
    let nanoseconds = u32::try_from(integer_i64_view(second.value())?).ok()?;
    (nanoseconds <= 999_999_999).then_some((seconds, nanoseconds))
}

fn integer_i64_view(view: ValueView<'_, '_>) -> Option<i64> {
    let ScalarView::Number(number) = view.scalar().ok()?? else {
        return None;
    };
    match number {
        NumberView::Number(number) => number.to_i64(),
        NumberView::Integer(text) => text.parse().ok(),
        _ => None,
    }
}

fn extract_timestamp_object(object: &Object) -> Option<(i64, u32)> {
    let mut entries = object.iter();
    let first = entries.next()?;
    let second = entries.next()?;
    if entries.next().is_some() {
        return None;
    }
    if first.key() != "seconds" || second.key() != "nanoseconds" {
        return None;
    }
    let seconds = integer_i64(first.value())?;
    let nanoseconds = integer_u32(second.value())?;
    (nanoseconds <= 999_999_999).then_some((seconds, nanoseconds))
}

fn integer_i64(value: &Value) -> Option<i64> {
    let Value::Number(number) = value else {
        return None;
    };
    number.to_i64()
}

fn integer_u32(value: &Value) -> Option<u32> {
    let value = integer_i64(value)?;
    u32::try_from(value).ok()
}

/// The nanoseconds a fraction's digits name: the digits are zero-padded to nine places (`"5"` is `500_000_000`
/// nanoseconds). `None` for more than nine digits or a non-decimal fraction.
fn fraction_nanoseconds(digits: &str) -> Option<u32> {
    if digits.is_empty() {
        return Some(0);
    }
    if digits.len() > 9 {
        return None;
    }
    let value: u64 = digits.parse().ok()?;
    let nanos = value.checked_mul(10u64.pow(u32::try_from(9 - digits.len()).ok()?))?;
    u32::try_from(nanos).ok()
}

/// Renders one value path as `.a[0].b` for the refusal diagnostics. One diagnostic-path crumb. Built only for the
/// failure site — a successful encode never formats a segment.
enum PathStep<'a> {
    Index(usize),
    Key(&'a str),
}

/// The decimal's scientific spelling per jqf-data's law (`value = coefficient × 10^−scale`, so the exponent is
/// −scale): the refusal prose names the value the way jqf-data itself renders it, never with an inverted magnitude.
fn decimal_spelling(coefficient: &str, scale: i64) -> String {
    let exponent = if scale > 0 {
        alloc::format!("-{scale}")
    } else {
        alloc::format!("{}", scale.unsigned_abs())
    };
    alloc::format!("{coefficient}e{exponent}")
}

fn render_path(path: &[PathStep<'_>]) -> String {
    let mut out = String::from(".");
    for segment in path {
        match segment {
            PathStep::Index(index) => {
                let mut digits = [0u8; 20];
                let mut value = *index;
                let mut start = digits.len();
                if value == 0 {
                    start -= 1;
                    digits[start] = b'0';
                } else {
                    while value > 0 {
                        start -= 1;
                        digits[start] = b'0' + (value % 10) as u8;
                        value /= 10;
                    }
                }
                out.push_str(core::str::from_utf8(&digits[start..]).unwrap_or(""));
            }
            PathStep::Key(key) => out.push_str(key),
        }
    }
    out
}

fn unrepresentable(message: &'static str) -> CodecError {
    error::encode_unrepresentable(message)
}

fn map_data(error: jqf_data::DataError) -> CodecError {
    jqf_codec_core::map_data(error, "MessagePack encoder rejected document construction")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use jqf_codec_core::{CodecFailureKind, DiagnosticPolicy};
    use jqf_data::{Decimal, Float, Integer, Number, TagId, Value};
    fn encode_owned(value: &Value) -> Result<Vec<u8>, CodecFailureKind> {
        encode_owned_with(value, false)
    }

    fn encode_owned_with(value: &Value, float64_decimals: bool) -> Result<Vec<u8>, CodecFailureKind> {
        let mut encoder = MessagepackEncoder::with_float64_decimals(float64_decimals);
        let mut path: Vec<PathStep<'_>> = Vec::new();
        encoder.encode_value(value, &mut path).map_err(|e| e.kind())?;
        Ok(encoder.bytes)
    }

    fn integer(text: &str) -> Value {
        Value::Number(Number::try_integer_unaccounted(Integer::parse(text).expect("integer")).expect("number"))
    }

    fn float(value: f64) -> Value {
        Value::Number(Number::float(Float::new(value)))
    }

    /// Shortest exact marker at each integer-family boundary.
    #[test]
    fn deterministic_encode_uses_the_shortest_integer_marker() {
        let cases: &[(&str, &[u8])] = &[
            ("0", &[0x00]),
            ("127", &[0x7f]),
            ("128", &[0xcc, 0x80]),
            ("255", &[0xcc, 0xff]),
            ("256", &[0xcd, 0x01, 0x00]),
            ("65535", &[0xcd, 0xff, 0xff]),
            ("65536", &[0xce, 0x00, 0x01, 0x00, 0x00]),
            ("4294967295", &[0xce, 0xff, 0xff, 0xff, 0xff]),
            ("4294967296", &[0xcf, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]),
            ("-1", &[0xff]),
            ("-32", &[0xe0]),
            ("-33", &[0xd0, 0xdf]),
            ("-128", &[0xd0, 0x80]),
            ("-129", &[0xd1, 0xff, 0x7f]),
            ("-32768", &[0xd1, 0x80, 0x00]),
            ("-32769", &[0xd2, 0xff, 0xff, 0x7f, 0xff]),
            ("-2147483648", &[0xd2, 0x80, 0x00, 0x00, 0x00]),
            ("-2147483649", &[0xd3, 0xff, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xff, 0xff]),
        ];
        for (text, expected) in cases {
            let encoded = encode_owned(&integer(text)).unwrap_or_else(|e| panic!("{text}: {e:?}"));
            assert_eq!(encoded, *expected, "integer {text}");
        }
    }

    /// A float32-exact finite value shrinks; a value that cannot round-trip through float32 stays float64.
    #[test]
    fn deterministic_encode_shrinks_lossless_floats_to_float32() {
        assert_eq!(
            encode_owned(&float(1.0)).expect("1.0"),
            vec![0xca, 0x3f, 0x80, 0x00, 0x00]
        );
        let encoded = encode_owned(&float(0.1)).expect("0.1");
        assert_eq!(encoded[0], 0xcb, "0.1 cannot shrink");
        assert_eq!(&encoded[1..], &0.1f64.to_bits().to_be_bytes());
    }

    #[test]
    fn a_decimal_is_refused() {
        let number = Number::try_decimal_unaccounted(Decimal::parse("1.5").expect("decimal")).expect("number");
        let error = encode_owned(&Value::Number(number)).expect_err("decimal refuses");
        assert_eq!(error, CodecFailureKind::UnsupportedRepresentation);
    }

    /// The refusal names the value with jqf-data's decimal law (`value = coefficient × 10^-scale`), so `1.5` reads as
    /// `15e-1` and `1e3` reads as `1e3` — never the inverted magnitude.
    #[test]
    fn the_decimal_refusal_spells_the_true_magnitude() {
        let spell = |text: &str| -> alloc::string::String {
            let number = Number::try_decimal_unaccounted(Decimal::parse(text).expect("decimal")).expect("number");
            let mut encoder = MessagepackEncoder::with_float64_decimals(false);
            let mut path: Vec<PathStep<'_>> = Vec::new();
            let error = encoder
                .encode_value(&Value::Number(number), &mut path)
                .expect_err("decimal refuses");
            error
                .diagnostic()
                .map(|d| alloc::string::String::from(d.message()))
                .unwrap_or_default()
        };
        assert!(
            spell("1.5").contains("a decimal value 15e-1 "),
            "1.5 must read as 15e-1: {}",
            spell("1.5")
        );
        assert!(
            spell("1e3").contains("a decimal value 1e3 "),
            "1e3 must keep its own magnitude: {}",
            spell("1e3")
        );
        assert!(
            spell("0.125").contains("a decimal value 125e-3 "),
            "0.125 must read as 125e-3: {}",
            spell("0.125")
        );
        // The `i64::MIN` scale is a reachable decimal (jqf-data's own boundary case): the exponent is 2^63, and
        // negating the scale would wrap (a debug-build panic) instead of naming it.
        assert!(
            spell("1e9223372036854775808").contains("a decimal value 1e9223372036854775808 "),
            "the i64::MIN scale must name the exponent 2^63 without wrapping: {}",
            spell("1e9223372036854775808")
        );
    }

    /// The float64 dialect's one divergence: a Decimal encodes as its nearest binary64 (float32 when the value is
    /// float32-exact, the deterministic shortest-form law), and the exact dialect still refuses.
    #[test]
    fn the_float64_dialect_encodes_a_decimal_as_a_float() {
        let decimal = |text: &str| {
            Value::Number(Number::try_decimal_unaccounted(Decimal::parse(text).expect("decimal")).expect("number"))
        };
        // 0.1 is not float32-exact: float64, the exact f64 bits.
        let encoded = encode_owned_with(&decimal("0.1"), true).expect("0.1 encodes under float64");
        assert_eq!(encoded[0], 0xcb, "0.1 travels as float64");
        assert_eq!(&encoded[1..], &0.1f64.to_bits().to_be_bytes());
        // 0.75 IS float32-exact: the shortest-form law still applies.
        let encoded = encode_owned_with(&decimal("0.75"), true).expect("0.75 encodes under float64");
        assert_eq!(
            encoded,
            vec![0xca, 0x3f, 0x40, 0x00, 0x00],
            "a float32-exact decimal shrinks"
        );
        // The exact dialect refuses the same value with the same class.
        let error = encode_owned(&decimal("0.75")).expect_err("deterministic refuses");
        assert_eq!(error, CodecFailureKind::UnsupportedRepresentation);
        // Integers keep their existing encodings under both profiles.
        let one = Value::Number(Number::try_integer_unaccounted(Integer::from_i64(7)).expect("number"));
        assert_eq!(
            encode_owned_with(&one, true).expect("integer"),
            encode_owned(&one).expect("integer")
        );
    }

    /// The factory accepts both output profiles and nothing else.
    #[test]
    fn the_factory_accepts_both_output_dialects() {
        let mut resources = crate::test_support::resources();
        let registration = crate::registration().expect("registration");
        let format = jqf_data::FormatId::try_new(crate::FORMAT_ID).expect("fmt");
        for dialect in [
            crate::MESSAGEPACK_DETERMINISTIC_DIALECT_ID,
            crate::MESSAGEPACK_DETERMINISTIC_FLOAT64_DIALECT_ID,
        ] {
            let id = jqf_data::DialectId::try_new(dialect).expect("dialect");
            registration
                .encoder()
                .expect("encoder")
                .create_factory(
                    EncodeRequest {
                        format: &format,
                        dialect: &id,
                        diagnostics: DiagnosticPolicy::ErrorsOnly,
                        preservation: PreservationRequest::None,
                        options: None,
                    },
                    &mut resources,
                )
                .unwrap_or_else(|e| panic!("{dialect}: {e:?}"));
        }
        let unknown = jqf_data::DialectId::try_new("messagepack.no-such@1").expect("dialect");
        let error = registration
            .encoder()
            .expect("encoder")
            .create_factory(
                EncodeRequest {
                    format: &format,
                    dialect: &unknown,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    preservation: PreservationRequest::None,
                    options: None,
                },
                &mut resources,
            )
            .expect_err("unknown output dialect refuses");
        assert_eq!(error.kind(), CodecFailureKind::RequirementMismatch);
    }

    #[test]
    fn an_integer_outside_the_messagepack_range_is_refused() {
        let too_big = integer("18446744073709551616");
        let error = encode_owned(&too_big).expect_err("2^64 refuses");
        assert_eq!(error, CodecFailureKind::UnsupportedRepresentation);
        let too_small = integer("-9223372036854775809");
        let error = encode_owned(&too_small).expect_err("i64::MIN-1 refuses");
        assert_eq!(error, CodecFailureKind::UnsupportedRepresentation);
    }

    #[test]
    fn an_in_range_timestamp_uses_the_shortest_extension() {
        let datetime = jqf_data::parse_rfc3339("1970-01-01T00:00:01Z").expect("datetime");
        let encoded = encode_owned(&Value::OffsetDateTime(datetime)).expect("timestamp");
        assert_eq!(encoded, vec![0xd6, 0xff, 0x00, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn an_unknown_ext_round_trips_as_tagged_bytes() {
        let value = Value::try_tagged(
            TagId::try_new_unaccounted("msgpack:ext:7").expect("tag"),
            Value::try_bytes(&[0xaa, 0xbb]).expect("bytes"),
        )
        .expect("tagged");
        let encoded = encode_owned(&value).expect("ext");
        // Two payload bytes take the shortest form: fixext2, not ext8.
        assert_eq!(encoded, vec![0xd5, 0x07, 0xaa, 0xbb]);
    }

    #[test]
    fn factory_encode_is_a_decode_encode_decode_fixed_point() {
        let value = integer("128");
        let mut resources = crate::test_support::resources();
        let registration = crate::registration().expect("registration");
        let factory = registration
            .encoder()
            .expect("encoder")
            .create_factory(
                EncodeRequest {
                    format: &jqf_data::FormatId::try_new(crate::FORMAT_ID).expect("fmt"),
                    dialect: &jqf_data::DialectId::try_new(crate::MESSAGEPACK_DETERMINISTIC_DIALECT_ID)
                        .expect("dialect"),
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    preservation: PreservationRequest::None,
                    options: None,
                },
                &mut resources,
            )
            .expect("factory");
        let mut session = factory
            .start(EncodeItem::owned(&value), PreservationRequest::None, &mut resources)
            .expect("session");
        let mut out = Vec::new();
        {
            let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
            let mut run = CodecRunContext::new(&mut resources);
            session.encode(&mut sink, &mut run).expect("encode");
        }
        assert_eq!(out, vec![0xcc, 0x80]);
    }
}
