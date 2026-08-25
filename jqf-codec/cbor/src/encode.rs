//! Deterministic CBOR (RFC 8949) encoder with the four output profiles.
//!
//! The encoder preflights ONE complete acyclic item and stages its bytes, offering them in bounded chunks. Bare-core
//! rules: an integer in `[-2^64, 2^64-1]` encodes as major 0/1 (a basic-range integer is never rewritten as a bignum);
//! larger positives and smaller negatives become tag-2/3 bignums; a decimal becomes tag 4 `[exponent = -scale, mantissa
//! = coefficient]`; a float ALWAYS stays a CBOR float even when integral; an `OffsetDateTime` becomes tag 0 with the
//! fixed uppercase RFC 3339 spelling.
//!
//! The four profiles differ in presentation only. `cbor.preferred@1` uses the shortest argument widths, definite
//! lengths, and the shortest float width that reconstructs the exact value (a NaN narrows to the half `0xf97e00` only
//! when its payload is already the canonical quiet NaN), and retains map occurrence order. `cbor.core-deterministic@1`
//! and `cbor.length-first@1` add the canonical NaN (`0xf97e00`) and sort map keys (by encoded bytes, or by length then
//! bytes). Under the text-key-only model the two sorts agree: a definite text head byte is monotonic in the key's
//! length, so `length-first@1` is text-key-equivalent to `core-deterministic@1` today. The length-then-bytes arm is
//! kept for a future non-text-key model; mixed-length text keys still pin both profiles' bytes. `cbor.source@1` reuses
//! exact source bytes when a bound native source covers the item; an owned value has no span, so it falls back to
//! preferred bytes and reports the replacement.
//!
//! The COMPLETE result is reparsed under `cbor.rfc8949-generic@1` before publication (the reparse-before-publish law):
//! a wrapper or value that the generic decoder would read back differently is unrepresentable, never silently emitted.
//!
//! ## The splice policy — binary splice as byte-length bookkeeping
//!
//! `--edit` on CBOR preserves the user's BYTES, not their comments: a changed scalar at a span-bearing item splices
//! exactly that item's own header-through-payload bytes, and a container that gained or lost a member splices the
//! member bytes plus the ONE count-bearing head whose count changed. The length bookkeeping is deliberately small
//! because CBOR definite heads carry the ITEM/PAIR COUNT, never a byte length: a splice below a container changes the
//! container's own byte size but not its count, so the ENCLOSING containers' heads are unchanged — their counts did
//! not move. The whole bookkeeping is therefore: re-derive the spliced container's head from its new count in the
//! profile's shortest argument width. A count crossing an argument-width boundary grows or shrinks that head by one to
//! seven bytes; that byte shift is the entire length-header cost of any CBOR splice. An indefinite-length container has
//! NO count head to rewrite: a splice cuts or inserts its members' bytes before the BREAK byte and nothing else. Every
//! splice is re-verified by the SDK's re-decode law, so a span the source contradicts — a tagged container, a head
//! that does not parse — declines to the whole-document floor, never wrong bytes.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use jqf_codec_core::{
    ByteSink, CodecError, CodecFailureKind, CodecRunContext, EditAppendMembers, EditInsertion, EditRemoval,
    EditRemoveMembers, EncodeItem, EncodeRequest, EncoderFactoryImpl, EncoderSession, ErasedEncoderFactory,
    ErasedEncoderSession, PreservationOutcome, PreservationReport, RecycledSessionState,
};
use jqf_data::{Document, IntrinsicTagSemantics, NodeId, NumberView, ScalarView, TagId, Value, ValueKind, ValueView};
use jqf_resource::{ResourceContext, WorkAdmission};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::big::Big;
use crate::read;

const OFFER_BYTES: usize = 16 * 1024;

/// The canonical positive quiet NaN bits (the same constant the YAML codec pins in `jqf-codec/yaml/src/schema.rs`).
/// Half-precision `0x7e00` widens back to exactly these bits, so `0xf97e00` is the lossless minimal NaN.
const POSITIVE_QUIET_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

/// One output profile, selected by the request's dialect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Profile {
    Source,
    Preferred,
    CoreDeterministic,
    LengthFirst,
}

impl Profile {
    pub(crate) fn from_dialect(dialect: &str) -> Option<Self> {
        Some(match dialect {
            crate::CBOR_SOURCE_DIALECT_ID => Self::Source,
            crate::CBOR_PREFERRED_DIALECT_ID => Self::Preferred,
            crate::CBOR_CORE_DETERMINISTIC_DIALECT_ID => Self::CoreDeterministic,
            crate::CBOR_LENGTH_FIRST_DIALECT_ID => Self::LengthFirst,
            _ => return None,
        })
    }

    /// Whether map keys are sorted (the two deterministic profiles).
    fn sorts_keys(self) -> bool {
        matches!(self, Self::CoreDeterministic | Self::LengthFirst)
    }
}

/// The stable identity of the CBOR encoder factory.
pub(crate) fn create_factory(
    request: EncodeRequest<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    let profile = Profile::from_dialect(request.dialect.as_str())
        .filter(|_| request.format.as_str() == crate::FORMAT_ID)
        .ok_or_else(|| CodecError::new(CodecFailureKind::RequirementMismatch))?;
    create_factory_with_profile(request, profile, resources)
}

/// Creates the CBOR encoder factory for one OUTPUT PROFILE, selected by the caller instead of by the request's dialect.
/// This is the cbor-seq seam: the seq registration's single output dialect carries the payload profile as an OPTION, so
/// its render factory maps the option to a profile and calls this — no request rewrite, no second dialect dispatch.
pub(crate) fn create_factory_with_profile(
    request: EncodeRequest<'_, '_>,
    profile: Profile,
    resources: &mut ResourceContext<'_>,
) -> Result<ErasedEncoderFactory, CodecError> {
    ErasedEncoderFactory::try_new_factory(request.preservation, resources, || Ok(CborEncoderFactory { profile }))
}

struct CborEncoderFactory {
    profile: Profile,
}

impl EncoderFactoryImpl for CborEncoderFactory {
    fn physical_encoder(&self) -> jqf_codec_core::PhysicalRouteId {
        crate::ENCODE_PHYSICAL_ROUTE_ID
    }

    fn start<'item, 'source>(
        &self,
        item: EncodeItem<'item, 'source>,
        preservation: jqf_codec_core::PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<ErasedEncoderSession<'item, 'source>, CodecError> {
        ErasedEncoderSession::try_new(item, preservation, || {
            Ok(CborEncoder {
                bytes: Vec::new(),
                profile: self.profile,
                root_done: false,
            })
        })
    }

    fn try_restart(
        &self,
        state: &mut RecycledSessionState<'_>,
        _item: EncodeItem<'_, '_>,
        _preservation: jqf_codec_core::PreservationRequest,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<bool, CodecError> {
        let Some(encoder) = state.downcast_mut::<CborEncoder>() else {
            return Ok(false);
        };
        encoder.reset();
        Ok(true)
    }

    /// The leaf seam (the widened node-context form): a changed scalar re-encodes as ONE CBOR item in the profile's
    /// presentation, replacing the item's whole authored span (header through payload). The head is part of the spliced
    /// bytes, so a value whose marker width differs — a small int becoming a bignum, a short string a long one —
    /// needs no separate length bookkeeping here. `authored` (the item's retained bytes) is informational: CBOR has no
    /// style to preserve beyond the profile, and the diff never splices an unchanged leaf, so the authored spelling is
    /// never reused.
    fn render_leaf(
        &self,
        _document: &Document<'_>,
        _node: NodeId,
        _path: &[String],
        _source: &[u8],
        value: &Value,
        _authored: Option<&[u8]>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<Vec<u8>, CodecError> {
        let mut encoder = CborEncoder {
            bytes: Vec::new(),
            profile: self.profile,
            root_done: false,
        };
        encoder.encode_value(value)?;
        Ok(encoder.bytes)
    }

    fn render_edit_append(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditAppendMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
        render_edit_append(document, container, source, members, self.profile)
    }

    fn render_edit_remove(
        &self,
        document: &Document<'_>,
        container: NodeId,
        _path: &[String],
        source: &[u8],
        members: EditRemoveMembers<'_>,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
        render_edit_remove(document, container, source, members)
    }
}

struct CborEncoder {
    bytes: Vec<u8>,
    profile: Profile,
    root_done: bool,
}

impl CborEncoder {
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

    fn encode_item(&mut self, item: EncodeItem<'_, '_>) -> Result<(), CodecError> {
        match item {
            EncodeItem::Owned(value) => self.encode_value(value),
            EncodeItem::Located { product, node } => {
                let document = product.document();
                // The cbor.source@1 law: a located item whose subtree the decode deferred to ONE source span reuses
                // those EXACT bytes (widths, chunks, ordering, tags, float bits). The span is segment-relative; the
                // retained segment is the exact authority the decode sealed.
                if self.profile == Profile::Source
                    && let Some(span) = document.node_source_span(node.local()).map_err(map_data)?
                    && let Some(segment) = document.source_segment()
                    && let Some(bytes) = segment.get(span.start() as usize..span.end() as usize)
                {
                    self.push(bytes);
                    return Ok(());
                }
                // The located intrinsic-layer resolution: the item is encoded directly from the authoritative document
                // (reading intrinsic tag facts), never by first materializing an owned value.
                let view = document.value_view(node).map_err(map_data)?;
                self.encode_view(document, view)
            }
        }
    }

    /// Encodes a located item directly from the document view. The intrinsic tag law: a resolved CORE tag is
    /// presentation over an ordinary value (it is not emitted); a non-core TAG-LAYER is emitted, then its payload.
    fn encode_view<'d, 's>(&mut self, document: &'d Document<'s>, view: ValueView<'d, 's>) -> Result<(), CodecError> {
        if view.tag_semantics().map_err(map_data)? == Some(IntrinsicTagSemantics::Tagged) {
            let tag = view.tag().map_err(map_data)?.ok_or_else(unrepresentable)?;
            return self.encode_view_tag(document, tag, view);
        }
        match view.kind().map_err(map_data)? {
            ValueKind::Null => {
                self.push(&[0xf6]);
                Ok(())
            }
            ValueKind::Bool => {
                let ScalarView::Bool(value) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push(if value { &[0xf5] } else { &[0xf4] });
                Ok(())
            }
            ValueKind::Number => {
                let ScalarView::Number(number) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.encode_number_view(number)
            }
            ValueKind::String => {
                let ScalarView::String(text) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.encode_text(text);
                Ok(())
            }
            ValueKind::Bytes => {
                let ScalarView::Bytes(bytes) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
                    return Err(unrepresentable());
                };
                self.push_head(2, bytes.len() as u64);
                self.push(bytes);
                Ok(())
            }
            ValueKind::Array => {
                let array = view.array().map_err(map_data)?.ok_or_else(unrepresentable)?;
                self.push_head(4, array.len() as u64);
                for item in array.iter() {
                    self.encode_view(document, item)?;
                }
                Ok(())
            }
            ValueKind::Object => self.encode_object_view(document, view),
            ValueKind::OffsetDateTime => {
                let datetime = located_offset_datetime(view)?;
                self.encode_offset_datetime(&datetime)
            }
            ValueKind::LocalDate | ValueKind::LocalTime | ValueKind::LocalDateTime => Err(unrepresentable()),
        }
    }

    /// Emits one located non-core tag layer, then its payload.
    fn encode_view_tag<'d, 's>(
        &mut self,
        document: &'d Document<'s>,
        tag: &TagId,
        view: ValueView<'d, 's>,
    ) -> Result<(), CodecError> {
        let tag = tag.as_str();
        if let Some(number) = tag.strip_prefix("cbor:tag:") {
            let number = parse_tag_number(number)?;
            self.push_tag(number);
            let payload = document
                .tag_payload(view.node())
                .map_err(map_data)?
                .ok_or_else(unrepresentable)?;
            let handle = document.node_handle(payload).map_err(map_data)?;
            let payload = document.value_view(handle).map_err(map_data)?;
            return self.encode_view(document, payload);
        }
        if let Some(simple) = tag.strip_prefix("cbor:simple:") {
            let value = parse_simple_number(simple)?;
            return self.encode_simple(value);
        }
        Err(unrepresentable())
    }

    /// Encodes a located object: occurrence order for the non-deterministic profiles, sorted keys for the deterministic
    /// ones.
    fn encode_object_view<'d, 's>(
        &mut self,
        document: &'d Document<'s>,
        view: ValueView<'d, 's>,
    ) -> Result<(), CodecError> {
        let object = view.object().map_err(map_data)?.ok_or_else(unrepresentable)?;
        let len = object.len();
        if !self.profile.sorts_keys() {
            self.push_head(5, len as u64);
            for entry in object.iter() {
                let entry = entry.map_err(map_data)?;
                self.encode_text(entry.key());
                self.encode_view(document, entry.value())?;
            }
            return Ok(());
        }
        // Deterministic: encode every key's head+bytes into ONE shared buffer, then sort (start, len) ranges into that
        // buffer — see [`CborEncoder::encode_object`] for the law (no per-member re-lookup, no per-key allocation
        // beyond the shared buffer).
        let mut keys: Vec<u8> = Vec::with_capacity(len * 24);
        let mut entries: Vec<(usize, usize, ValueView<'d, 's>)> = Vec::with_capacity(len);
        for entry in object.iter() {
            let entry = entry.map_err(map_data)?;
            let key = entry.key();
            let start = keys.len();
            let mut head = [0u8; 9];
            let written = head_into(&mut head, 3, key.len() as u64);
            keys.extend_from_slice(&head[..written]);
            keys.extend_from_slice(key.as_bytes());
            entries.push((start, keys.len() - start, entry.value()));
        }
        if self.profile == Profile::LengthFirst {
            entries.sort_by(|left, right| {
                left.1
                    .cmp(&right.1)
                    .then_with(|| keys[left.0..left.0 + left.1].cmp(&keys[right.0..right.0 + right.1]))
            });
        } else {
            entries.sort_by(|left, right| keys[left.0..left.0 + left.1].cmp(&keys[right.0..right.0 + right.1]));
        }
        self.push_head(5, len as u64);
        for (start, key_len, value) in &entries {
            self.push(&keys[*start..*start + *key_len]);
            self.encode_view(document, *value)?;
        }
        Ok(())
    }

    /// Encodes one located number from its borrowed projection.
    fn encode_number_view(&mut self, number: NumberView<'_>) -> Result<(), CodecError> {
        match number {
            NumberView::Number(number) => {
                // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained
                // one.
                if let Some(machine) = number.as_machine() {
                    self.push_integer_i64(machine);
                    Ok(())
                } else if let Some(integer) = number.as_integer() {
                    self.encode_integer(integer.as_str())
                } else if let Some(float) = number.as_float() {
                    self.encode_float(float.bits());
                    Ok(())
                } else if let Some(decimal) = number.as_decimal() {
                    self.encode_decimal(decimal)
                } else {
                    Err(unrepresentable())
                }
            }
            NumberView::Integer(spelling) => self.encode_integer(spelling),
            NumberView::Decimal { coefficient, scale } => self.encode_decimal_parts(coefficient, scale),
            NumberView::Float(float) => {
                self.encode_float(float.bits());
                Ok(())
            }
        }
    }

    /// Encodes one located `cbor:simple:` value byte.
    fn encode_simple(&mut self, value: u8) -> Result<(), CodecError> {
        // 20..=22 are core Bool/Null identities, not retained wrappers; 24..=31 are reserved.
        match value {
            20..=22 | 24..=31 => Err(unrepresentable()),
            0..=19 | 23 | 32..=255 => {
                if value < 20 {
                    self.push(&[0xe0 | value]);
                } else if value == 23 {
                    self.push(&[0xf7]);
                } else {
                    self.push(&[0xf8, value]);
                }
                Ok(())
            }
        }
    }

    /// Encodes one complete value, following the profile's presentation laws.
    fn encode_value(&mut self, value: &Value) -> Result<(), CodecError> {
        match value {
            Value::Null => {
                self.push(&[0xf6]);
                Ok(())
            }
            Value::Bool(value) => {
                self.push(if *value { &[0xf5] } else { &[0xf4] });
                Ok(())
            }
            Value::Number(number) => {
                // The inline machine arm renders its canonical spelling on demand; the boxed arm borrows its retained
                // one.
                if let Some(machine) = number.as_machine() {
                    self.push_integer_i64(machine);
                    Ok(())
                } else if let Some(integer) = number.as_integer() {
                    self.encode_integer(integer.as_str())
                } else if let Some(float) = number.as_float() {
                    self.encode_float(float.bits());
                    Ok(())
                } else if let Some(decimal) = number.as_decimal() {
                    self.encode_decimal(decimal)
                } else {
                    Err(unrepresentable())
                }
            }
            Value::String(text) => {
                self.encode_text(text.as_str());
                Ok(())
            }
            Value::Bytes(bytes) => {
                self.push_head(2, bytes.len() as u64);
                self.push(bytes.as_ref());
                Ok(())
            }
            Value::Array(array) => {
                self.push_head(4, array.len() as u64);
                for item in array {
                    self.encode_value(item)?;
                }
                Ok(())
            }
            Value::Object(object) => self.encode_object(object),
            Value::Tagged { tag, payload } => self.encode_tagged(tag.as_str(), payload),
            Value::LocalDate(_) | Value::LocalTime(_) | Value::LocalDateTime(_) => Err(unrepresentable()),
            Value::OffsetDateTime(datetime) => self.encode_offset_datetime(datetime),
        }
    }

    /// Encodes a map: occurrence order for the non-deterministic profiles, sorted keys for the deterministic ones.
    fn encode_object(&mut self, object: &jqf_data::Object) -> Result<(), CodecError> {
        let len = object.len();
        if !self.profile.sorts_keys() {
            self.push_head(5, len as u64);
            for index in 0..len {
                let entry = object.get_index(index).ok_or_else(unrepresentable)?;
                self.encode_text(entry.key());
                self.encode_value(entry.value())?;
            }
            return Ok(());
        }
        // Deterministic: encode every key's head+bytes into ONE shared buffer, then sort (start, len) ranges into that
        // buffer. The sorted entry travels with its `&Value` — no per-member `object.get(key)` re-lookup (O(n) on an
        // unindexed object, O(n^2) total) — and no per-key allocation beyond the shared buffer.
        let mut keys: Vec<u8> = Vec::with_capacity(len * 24);
        let mut entries: Vec<(usize, usize, &jqf_data::Value)> = Vec::with_capacity(len);
        for index in 0..len {
            let entry = object.get_index(index).ok_or_else(unrepresentable)?;
            let key = entry.key();
            let start = keys.len();
            let mut head = [0u8; 9];
            let written = head_into(&mut head, 3, key.len() as u64);
            keys.extend_from_slice(&head[..written]);
            keys.extend_from_slice(key.as_bytes());
            entries.push((start, keys.len() - start, entry.value()));
        }
        if self.profile == Profile::LengthFirst {
            entries.sort_by(|left, right| {
                left.1
                    .cmp(&right.1)
                    .then_with(|| keys[left.0..left.0 + left.1].cmp(&keys[right.0..right.0 + right.1]))
            });
        } else {
            entries.sort_by(|left, right| keys[left.0..left.0 + left.1].cmp(&keys[right.0..right.0 + right.1]));
        }
        self.push_head(5, len as u64);
        for (start, key_len, value) in &entries {
            self.push(&keys[*start..*start + *key_len]);
            self.encode_value(value)?;
        }
        Ok(())
    }

    /// Resolves one owned tag wrapper. Recognized numeric tags whose payload the generic decoder would read back
    /// differently are handled by the reparse-before-publish check; this method writes the tag then the payload.
    fn encode_tagged(&mut self, tag: &str, payload: &Value) -> Result<(), CodecError> {
        if let Some(number) = tag.strip_prefix("cbor:tag:") {
            let number = parse_tag_number(number)?;
            self.push_tag(number);
            return self.encode_value(payload);
        }
        if let Some(simple) = tag.strip_prefix("cbor:simple:") {
            let value = parse_simple_number(simple)?;
            self.encode_simple(value)
        } else {
            Err(unrepresentable())
        }
    }

    /// Writes one tag head (`c` + the tag number).
    fn push_tag(&mut self, number: u64) {
        self.push_head(6, number);
    }

    fn encode_integer(&mut self, spelling: &str) -> Result<(), CodecError> {
        // Machine-range spellings skip the arbitrary-precision parser: the caller already holds an i64 on the
        // encode_value/encode_number_view machine arms, and a 20-or-fewer-digit spelling always fits i64/u64 or is the
        // one `-2^64` edge the Big path special-cases.
        if spelling.len() <= 20 {
            if let Ok(value) = spelling.parse::<i64>() {
                self.push_integer_i64(value);
                return Ok(());
            }
            if let Ok(value) = spelling.parse::<u64>() {
                self.push_head(0, value);
                return Ok(());
            }
        }
        let value = Big::from_decimal_str(spelling).ok_or_else(unrepresentable)?;
        if !value.is_negative() {
            if let Some(magnitude) = value.to_u64() {
                self.push_head(0, magnitude);
                return Ok(());
            }
            // Bignum: tag 2 + minimal big-endian magnitude.
            self.push_tag(2);
            let bytes = value.to_big_endian_bytes();
            self.push_head(2, bytes.len() as u64);
            self.push(&bytes);
            return Ok(());
        }
        // Negative: `-m`. Basic range when `m <= 2^64`: the major-1 argument is `m - 1`, which for `m == 2^64` is
        // `u64::MAX` (RFC 8949 §3.6 minimal encoding — `-2^64` is never a tag-3 bignum).
        let magnitude = value.negated();
        if let Some(m) = magnitude.to_u64() {
            self.push_head(1, m - 1);
            return Ok(());
        }
        if magnitude == Big::from_u64(u64::MAX).add_small(1) {
            self.push_head(1, u64::MAX);
            return Ok(());
        }
        // Bignum: tag 3 + `n = m - 1` big-endian.
        self.push_tag(3);
        let n = magnitude.sub_small(1);
        let bytes = n.to_big_endian_bytes();
        self.push_head(2, bytes.len() as u64);
        self.push(&bytes);
        Ok(())
    }

    fn encode_decimal(&mut self, decimal: &jqf_data::Decimal) -> Result<(), CodecError> {
        let coefficient = decimal.coefficient().as_str();
        let scale = decimal.scale();
        self.encode_decimal_parts(coefficient, scale)
    }

    /// Encodes a decimal as tag 4 plus a definite two-item array `[exponent = -scale, mantissa = coefficient]`.
    fn encode_decimal_parts(&mut self, coefficient: &str, scale: i64) -> Result<(), CodecError> {
        let exponent = scale.checked_neg().ok_or_else(unrepresentable)?;
        self.push_tag(4);
        self.push_head(4, 2);
        self.push_integer_i64(exponent);
        self.encode_integer(coefficient)
    }

    /// Encodes a float: the shortest width that reconstructs the exact value. A NaN narrows to the half `0xf97e00`
    /// exactly when its bits ARE the canonical positive quiet NaN — the one payload that half decodes back to —
    /// which the deterministic profiles canonicalized to above and preferred/source carry when they already hold it.
    fn encode_float(&mut self, bits: u64) {
        let bits = self.canonicalize_nan(bits);
        let exponent = (bits >> 52) & 0x7ff;
        let fraction = bits & 0x000f_ffff_ffff_ffff;
        let is_nan = exponent == 0x7ff && fraction != 0;
        if is_nan {
            if bits == POSITIVE_QUIET_NAN_BITS {
                self.push(&[0xf9, 0x7e, 0x00]);
                return;
            }
        } else {
            if let Some(f16) = read::narrow_f16(bits) {
                self.push(&[0xf9, (f16 >> 8) as u8, (f16 & 0xff) as u8]);
                return;
            }
            if let Some(f32) = read::narrow_f32(bits) {
                self.push(&[0xfa]);
                self.push(&f32.to_be_bytes());
                return;
            }
        }
        self.push(&[0xfb]);
        self.push(&bits.to_be_bytes());
    }

    /// The profile's NaN law: preferred/source keep the exact bits (a NaN stays f64 unless its payload is already the
    /// canonical quiet NaN, in which case it narrows); the deterministic profiles canonicalize every NaN to the single
    /// positive quiet NaN `0x7ff8_0000_0000_0000` (the same bits the YAML codec pins), which then encodes as
    /// `0xf97e00`.
    fn canonicalize_nan(&self, bits: u64) -> u64 {
        let exponent = (bits >> 52) & 0x7ff;
        let fraction = bits & 0x000f_ffff_ffff_ffff;
        if exponent == 0x7ff && fraction != 0 && self.profile.sorts_keys() {
            return POSITIVE_QUIET_NAN_BITS;
        }
        bits
    }

    fn encode_text(&mut self, text: &str) {
        self.push_head(3, text.len() as u64);
        self.push(text.as_bytes());
    }

    fn encode_offset_datetime(&mut self, datetime: &jqf_data::OffsetDateTime) -> Result<(), CodecError> {
        // A leap second IS spellable here: RFC 3339's `time-second` is `00-60` and tag 0 carries an RFC 3339 string, so
        // the decoder that accepted `23:59:60Z` must be able to write it back. Refusing it made a leap-second timestamp
        // the one CBOR input jqf could read and then not re-emit AS CBOR.
        let mut buffer = String::with_capacity(32);
        push_fixed_date(&mut buffer, datetime);
        buffer.push('T');
        push_fixed_time(&mut buffer, datetime);
        let fraction = datetime.local.time.fraction().digits();
        if !fraction.is_empty() {
            buffer.push('.');
            buffer.push_str(fraction);
        }
        // A nonzero offset must be minute-aligned (tag 0 carries an RFC 3339 text, which has minute granularity); the
        // guard belongs here, before the shared writer.
        if let jqf_data::UtcOffset::KnownSeconds(offset) = datetime.offset
            && offset.seconds().rem_euclid(60) != 0
        {
            return Err(unrepresentable());
        }
        push_offset_suffix(&mut buffer, datetime.offset);
        self.push_tag(0);
        self.encode_text(&buffer);
        Ok(())
    }

    /// Writes the shortest head for `major` (0..=6) and a `u64` argument.
    #[allow(
        clippy::checked_conversions,
        reason = "the RFC 8949 argument-width selection is a deliberate range ladder"
    )]
    fn push_head(&mut self, major: u8, argument: u64) {
        let mut buffer = [0u8; 9];
        let written = head_into(&mut buffer, major, argument);
        self.push(&buffer[..written]);
    }

    /// Writes a machine integer as a basic-range major-0/1 head.
    fn push_integer_i64(&mut self, value: i64) {
        if value >= 0 {
            self.push_head(0, value as u64);
        } else {
            // RFC 8949: the major-1 argument is `-1 - value`. `i64::MIN` is `-2^63`, whose argument is `2^63 - 1` and
            // fits a u64.
            self.push_head(1, (value as u64).wrapping_neg().wrapping_sub(1));
        }
    }

    fn report(&self) -> PreservationReport {
        let ordering = if self.profile.sorts_keys() {
            PreservationOutcome::Normalized
        } else {
            PreservationOutcome::Exact
        };
        PreservationReport::new(
            PreservationOutcome::Exact,
            PreservationOutcome::Exact,
            ordering,
            PreservationOutcome::Normalized,
        )
    }
}

// --------------------------------------------------------------------------- The splice machinery
// (`render_edit_append` / `render_edit_remove`), the binary-splice policy's implementation. The policy itself is
// written in the module docs above: a splice rewrites the member bytes plus the ONE count-bearing head whose count
// changed; every other head is untouched because CBOR heads carry counts, never byte lengths. Every walk here is
// deterministic over the codec's own round-tripped bytes; a run that cannot name the container's region declines to the
// whole-document floor (the SDK's re-decode law), never wrong bytes.

/// Writes the shortest head for `major` (0..=6) and a `u64` argument into a 9-byte stack buffer, returning the written
/// length. Shared spine of [`CborEncoder::push_head`] and the splice's count-head re-derivation.
#[allow(
    clippy::checked_conversions,
    reason = "the RFC 8949 argument-width selection is a deliberate range ladder"
)]
fn head_into(buffer: &mut [u8; 9], major: u8, argument: u64) -> usize {
    let base = major << 5;
    if argument < 24 {
        buffer[0] = base | argument as u8;
        return 1;
    }
    if argument <= u64::from(u8::MAX) {
        buffer[0] = base | 0x18;
        buffer[1] = argument as u8;
        return 2;
    }
    if argument <= u64::from(u16::MAX) {
        buffer[0] = base | 0x19;
        buffer[1..3].copy_from_slice(&(argument as u16).to_be_bytes());
        return 3;
    }
    if argument <= u64::from(u32::MAX) {
        buffer[0] = base | 0x1a;
        buffer[1..5].copy_from_slice(&(argument as u32).to_be_bytes());
        return 5;
    }
    buffer[0] = base | 0x1b;
    buffer[1..9].copy_from_slice(&argument.to_be_bytes());
    9
}

/// Owned head bytes for the splice sites that insert a replacement head.
fn head_bytes(major: u8, argument: u64) -> Vec<u8> {
    let mut buffer = [0u8; 9];
    let written = head_into(&mut buffer, major, argument);
    buffer[..written].to_vec()
}

/// The container's own count-bearing head: its byte position and one past its argument. A tagged container's authored
/// span begins at the FIRST tag head of the tag chain, so the splice skips tag heads (major 6) to reach the head whose
/// argument is the item/pair count.
fn container_head(source: &[u8], span_start: usize) -> Option<(usize, usize)> {
    let mut pos = span_start;
    loop {
        let (head, after) = read::head(source, pos).ok()?;
        if head.major == read::Major::Tag {
            pos = after;
            continue;
        }
        return Some((pos, after));
    }
}

/// The deepest nesting the splice extent walk follows. The walk runs only over wire a validating decode already
/// accepted under the governed nesting ceiling, so the bound is unreachable in practice; past it the walk fails and the
/// splice declines to the whole-document floor.
const MAX_SPLICE_DEPTH: u32 = 10_000;

/// One item's byte extent: (first byte, one past the last byte). Walks through tag layers and container payloads, so
/// the extent of ANY item — scalar, string, container, tagged — is exact.
///
/// The descent carries an explicit depth budget instead of native recursion: the crate's discipline is that a walk
/// bounded by the governed ceiling must not become one native frame per level.
fn item_extent(source: &[u8], pos: usize) -> Option<(usize, usize)> {
    item_extent_depth(source, pos, MAX_SPLICE_DEPTH)
}

fn item_extent_depth(source: &[u8], pos: usize, depth: u32) -> Option<(usize, usize)> {
    if depth == 0 {
        return None;
    }
    let child = depth - 1;
    let (head, after) = read::head(source, pos).ok()?;
    let end = match head.major {
        read::Major::UInt | read::Major::NegInt | read::Major::Simple => after,
        read::Major::Bytes | read::Major::Text => match head.arg {
            read::Arg::UInt(len) => after.checked_add(len as usize).filter(|&end| end <= source.len())?,
            // An indefinite-length string: definite chunks until the BREAK byte (RFC 8949 §3.2.3's chunk discipline,
            // walked permissively).
            read::Arg::Indef => indefinite_end(source, after, child)?,
        },
        read::Major::Array | read::Major::Map => match head.arg {
            read::Arg::UInt(count) => {
                let pairs = head.major == read::Major::Map;
                let mut pos = after;
                for _ in 0..count {
                    for _ in 0..(if pairs { 2 } else { 1 }) {
                        pos = item_extent_depth(source, pos, child)?.1;
                    }
                }
                pos
            }
            read::Arg::Indef => indefinite_end(source, after, child)?,
        },
        read::Major::Tag => item_extent_depth(source, after, child)?.1,
    };
    Some((pos, end))
}

/// One past the BREAK byte terminating an indefinite-length item (a string's chunk run or a container's member run).
fn indefinite_end(source: &[u8], mut pos: usize, depth: u32) -> Option<usize> {
    loop {
        match source.get(pos) {
            Some(0xff) => return Some(pos + 1),
            Some(_) => pos = item_extent_depth(source, pos, depth)?.1,
            None => return None,
        }
    }
}

/// The container's head facts the splice needs: its own head's byte position, one past it, and the definite count.
/// Returns `None` when the container's span does not name an array/map head (a tag chain is skipped) — the caller
/// declines to the floor.
fn container_head_facts(source: &[u8], span_start: usize) -> Option<(usize, usize, u8, Option<u64>)> {
    let (head_pos, head_end) = container_head(source, span_start)?;
    let (head, _) = read::head(source, head_pos).ok()?;
    let count = match head.arg {
        read::Arg::UInt(count) => Some(count),
        read::Arg::Indef => None,
    };
    let major = match head.major {
        read::Major::Array => 4,
        read::Major::Map => 5,
        _ => return None,
    };
    Some((head_pos, head_end, major, count))
}

/// The DIRECT children of a container, each item's (start, one past end), in wire order. A map's children alternate
/// key, value. An indefinite container's walk stops at its BREAK byte. `head_pos` must be the container's own head (tag
/// layers skipped).
fn container_children(source: &[u8], head_pos: usize) -> Option<Vec<(usize, usize)>> {
    let (head, after) = read::head(source, head_pos).ok()?;
    let count = match (head.major, head.arg) {
        (read::Major::Array, read::Arg::UInt(count)) => count,
        (read::Major::Map, read::Arg::UInt(count)) => count.checked_mul(2)?,
        (read::Major::Array | read::Major::Map, read::Arg::Indef) => {
            let mut children = Vec::new();
            let mut pos = after;
            loop {
                match source.get(pos) {
                    Some(0xff) => break,
                    Some(_) => {
                        let (start, end) = item_extent(source, pos)?;
                        children.push((start, end));
                        pos = end;
                    }
                    None => return None,
                }
            }
            return Some(children);
        }
        _ => return None,
    };
    let mut children = Vec::new();
    let mut pos = after;
    for _ in 0..count {
        let (start, end) = item_extent(source, pos)?;
        children.push((start, end));
        pos = end;
    }
    Some(children)
}

/// Renders the splice for a container the edit lane GREW: the new members' bytes are inserted at the container's
/// content end — past its last direct child for a definite container, before the BREAK byte for an indefinite one —
/// and the container's count head is re-derived from its new count (skipped entirely for an indefinite container, which
/// has no head to rewrite). Returns an empty insertion set (the floor) when the container's region cannot be named.
///
/// The seam does not itself check appended map keys against the container's existing keys: the edit lane never appends
/// a key that is already present, and any §5.6.1 violation that did slip through fails the re-decode verification,
/// declining the request loudly instead of publishing colliding members.
fn render_edit_append(
    document: &Document<'_>,
    container: NodeId,
    source: &[u8],
    members: EditAppendMembers<'_>,
    profile: Profile,
) -> Result<alloc::vec::Vec<EditInsertion>, CodecError> {
    let Some(span) = document.node_source_span(container).map_err(map_data)? else {
        return Ok(alloc::vec::Vec::new());
    };
    let start = span.start() as usize;
    let Some((head_pos, head_end, major, count)) = container_head_facts(source, start) else {
        return Ok(alloc::vec::Vec::new());
    };
    let kind_ok = match members {
        EditAppendMembers::Array(_) => major == 4,
        EditAppendMembers::Table(_) => major == 5,
    };
    if !kind_ok {
        return Ok(alloc::vec::Vec::new());
    }
    // Encode the added members in the container's item stream: array items are bare values; map members are key text
    // then value, in the profile's presentation (the profile owns a map's key ordering on a full re-encode; a splice
    // appends in the given order and the re-decode verification judges semantically, so the deterministic profiles'
    // sort is not imposed on the authored byte stream).
    let mut encoder = CborEncoder {
        bytes: Vec::new(),
        profile,
        root_done: false,
    };
    match members {
        EditAppendMembers::Array(items) => {
            for item in items {
                encoder.encode_value(item)?;
            }
        }
        EditAppendMembers::Table(members) => {
            for (key, value) in members {
                encoder.encode_text(key);
                encoder.encode_value(value)?;
            }
        }
    }
    let added = match members {
        EditAppendMembers::Array(items) => items.len() as u64,
        EditAppendMembers::Table(members) => members.len() as u64,
    };
    let mut insertions = Vec::new();
    if let Some(old_count) = count {
        // Definite container: re-derive the count head, then insert the members after the last direct child (the
        // container's authored span ends exactly past its last item, tag layers included).
        let Some(new_count) = old_count.checked_add(added) else {
            return Ok(alloc::vec::Vec::new());
        };
        let new_head = head_bytes(major, new_count);
        if new_head != source[head_pos..head_end] {
            // The count-head rewrite is a REPLACEMENT splice: the head's authored span is overwritten with the
            // re-derived bytes, so a count crossing an argument-width boundary grows or shrinks the head in place (the
            // seam's `replace` form).
            insertions.push(EditInsertion {
                at: head_pos,
                bytes: new_head,
                replace: Some((head_pos, head_end)),
            });
        }
        insertions.push(EditInsertion {
            at: span.end() as usize,
            bytes: encoder.bytes,
            replace: None,
        });
    } else {
        // Indefinite container: no count head to rewrite; the members land before the BREAK byte (the container's
        // authored span ends past it).
        let Some(at) = span.end().checked_sub(1) else {
            return Ok(alloc::vec::Vec::new());
        };
        insertions.push(EditInsertion {
            at: at as usize,
            bytes: encoder.bytes,
            replace: None,
        });
    }
    Ok(insertions)
}

/// Renders the cuts for a container the edit lane SHRANK: each removed member's item bytes are cut — the value span
/// alone for an array item, the key span plus the value span for a map member — and the container's count head is
/// re-derived from its new count. The map arm walks the container's direct children to locate the key immediately
/// before each removed value's span; any span the walk cannot match declines the whole removal to the floor.
fn render_edit_remove(
    document: &Document<'_>,
    container: NodeId,
    source: &[u8],
    members: EditRemoveMembers<'_>,
) -> Result<alloc::vec::Vec<EditRemoval>, CodecError> {
    let Some(span) = document.node_source_span(container).map_err(map_data)? else {
        return Ok(alloc::vec::Vec::new());
    };
    let start = span.start() as usize;
    let Some((head_pos, head_end, major, count)) = container_head_facts(source, start) else {
        return Ok(alloc::vec::Vec::new());
    };
    let kind_ok = match members {
        EditRemoveMembers::Array(_) => major == 4,
        EditRemoveMembers::Table(_) => major == 5,
    };
    if !kind_ok {
        return Ok(alloc::vec::Vec::new());
    }
    let children = match members {
        EditRemoveMembers::Array(_) => None,
        EditRemoveMembers::Table(_) => container_children(source, head_pos),
    };
    let mut removals = Vec::new();
    let mut removed = 0u64;
    match members {
        EditRemoveMembers::Array(items) => {
            for (_, node) in items {
                let Some(member_span) = document.node_source_span(*node).map_err(map_data)? else {
                    return Ok(alloc::vec::Vec::new());
                };
                removals.push(EditRemoval {
                    start: member_span.start() as usize,
                    end: member_span.end() as usize,
                    replacement: Vec::new(),
                });
                removed += 1;
            }
        }
        EditRemoveMembers::Table(members) => {
            let Some(children) = children else {
                return Ok(alloc::vec::Vec::new());
            };
            // One pass indexes the direct-child spans so each removed member's lookup is O(1); a linear `position` per
            // member made k removals from an n-member map O(k·n). `or_insert` keeps the first index, matching the scan
            // it replaces.
            let mut child_index = alloc::collections::BTreeMap::new();
            for (index, span) in children.iter().enumerate() {
                child_index.entry(*span).or_insert(index);
            }
            for (_, node) in members {
                let Some(member_span) = document.node_source_span(*node).map_err(map_data)? else {
                    return Ok(alloc::vec::Vec::new());
                };
                let vstart = member_span.start() as usize;
                let vend = member_span.end() as usize;
                // The value's direct-child index is odd in the key/value pair stream; the key is the child immediately
                // before it.
                let Some(&index) = child_index.get(&(vstart, vend)) else {
                    return Ok(alloc::vec::Vec::new());
                };
                if index == 0 || index % 2 == 0 {
                    return Ok(alloc::vec::Vec::new());
                }
                let (key_start, _) = children[index - 1];
                removals.push(EditRemoval {
                    start: key_start,
                    end: vend,
                    replacement: Vec::new(),
                });
                removed += 1;
            }
        }
    }
    if let Some(old_count) = count {
        let Some(new_count) = old_count.checked_sub(removed) else {
            return Ok(alloc::vec::Vec::new());
        };
        let new_head = head_bytes(major, new_count);
        if new_head != source[head_pos..head_end] {
            // The count-head rewrite is a replacement removal: the head's authored span is overwritten with the
            // re-derived bytes (the seam's `replacement` form).
            removals.push(EditRemoval {
                start: head_pos,
                end: head_end,
                replacement: new_head,
            });
        }
    }
    Ok(removals)
}

/// The CANONICAL unsigned decimal spelling law, shared by the encoder's tag emission and the target validator
/// ([`crate::tag`]): an all-digit, no-leading-zero spelling. `cbor:tag:5` and `cbor:tag:05` are distinct `TagId` texts
/// that would both emit wire tag 5 — a colliding tag set — so BOTH halves refuse the non-canonical form and cannot
/// disagree.
pub(crate) fn parse_tag_number(text: &str) -> Result<u64, CodecError> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) || (text.len() > 1 && text.starts_with('0')) {
        return Err(unrepresentable());
    }
    text.parse::<u64>().map_err(|_| unrepresentable())
}

pub(crate) fn parse_simple_number(text: &str) -> Result<u8, CodecError> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) || (text.len() > 1 && text.starts_with('0')) {
        return Err(unrepresentable());
    }
    text.parse::<u8>().map_err(|_| unrepresentable())
}

/// Pushes `YYYY-MM-DD`.
fn push_fixed_date(buffer: &mut String, datetime: &jqf_data::OffsetDateTime) {
    let date = datetime.local.date;
    push_four(buffer, date.year());
    buffer.push('-');
    push_two(buffer, date.month());
    buffer.push('-');
    push_two(buffer, date.day());
}

/// Pushes `HH:MM:SS`.
fn push_fixed_time(buffer: &mut String, datetime: &jqf_data::OffsetDateTime) {
    let time = &datetime.local.time;
    push_two(buffer, time.hour());
    buffer.push(':');
    push_two(buffer, time.minute());
    buffer.push(':');
    push_two(buffer, time.second());
}

/// Pushes the RFC 3339 offset suffix after minute alignment is verified.
fn push_offset_suffix(buffer: &mut String, offset: jqf_data::UtcOffset) {
    match offset {
        jqf_data::UtcOffset::UnknownLocalOffset => buffer.push_str("-00:00"),
        jqf_data::UtcOffset::KnownSeconds(known) => {
            let seconds = known.seconds();
            if seconds == 0 {
                buffer.push('Z');
            } else {
                buffer.push(if seconds < 0 { '-' } else { '+' });
                let magnitude = seconds.unsigned_abs();
                push_two(buffer, (magnitude / 3600) as u8);
                buffer.push(':');
                push_two(buffer, ((magnitude % 3600) / 60) as u8);
            }
        }
    }
}

/// `LocalDate::new` caps the year at 9999, so the fixed width never truncates.
fn push_four(buffer: &mut String, value: u16) {
    let _ = write!(buffer, "{value:04}");
}

/// Month, day, hour, minute and second are all validated below 100.
fn push_two(buffer: &mut String, value: u8) {
    let _ = write!(buffer, "{value:02}");
}

fn unrepresentable() -> CodecError {
    CodecError::new(CodecFailureKind::UnsupportedRepresentation)
}

/// Builds an owned offset date-time from a located view (the only copy a located CBOR item pays for: the fixed-size
/// datetime record).
fn located_offset_datetime(view: ValueView<'_, '_>) -> Result<jqf_data::OffsetDateTime, CodecError> {
    let ScalarView::OffsetDateTime(value) = view.scalar().map_err(map_data)?.ok_or_else(unrepresentable)? else {
        return Err(unrepresentable());
    };
    let time = jqf_data::LocalTime::new(
        value.local.time.hour,
        value.local.time.minute,
        value.local.time.second,
        jqf_data::FractionalSecond::parse(value.local.time.fraction).map_err(|_| unrepresentable())?,
    )
    .ok_or_else(unrepresentable)?;
    Ok(jqf_data::OffsetDateTime {
        local: jqf_data::LocalDateTime {
            date: value.local.date,
            time,
        },
        offset: value.offset,
    })
}

fn map_data(error: jqf_data::DataError) -> CodecError {
    jqf_codec_core::map_data(error, "CBOR encoder document access")
}

impl EncoderSession for CborEncoder {
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
                return Ok(self.report());
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
                        self.encode_item(item)?;
                        self.reparse(context.resources())?;
                        self.root_done = true;
                    }
                }
            }
        }
    }
}

impl CborEncoder {
    /// Reparses the staged bytes under `cbor.rfc8949-generic@1` before publication (the reparse-before-publish law). A
    /// result the generic decoder cannot read back as one valid item is unrepresentable.
    ///
    /// Uses the validate-only walk rather than a full document decode: the walk carries the same well-formedness,
    /// text-UTF-8, §5.6.1 uniqueness, text-key-only, recognized-tag shape, and bignum-size refusals the whole-document
    /// decoder enforces (see `validate_tag_payload`).
    fn reparse(&mut self, resources: &mut ResourceContext<'_>) -> Result<(), CodecError> {
        // The staged bytes have no source authority of their own; only their validity matters here, so the walk's
        // diagnostics are discarded.
        let staged = ResolvedSource::new(
            SourceRef::new(SourceId::new(0), SourceKind::Input),
            "cbor-staged",
            self.bytes.as_slice(),
            0,
        );
        crate::walk::locate(staged, &[], false, resources)
            .map(|_| ())
            .map_err(|_| unrepresentable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec;
    use jqf_codec_core::{
        AccessGuarantees, AccessOutcome, AccessRequirement, AccessResult, CodecDemand, DecodeRequest, DemandClause,
        DiagnosticPolicy, ValidationMode,
    };
    use jqf_data::DialectId;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("account");
        let work = WorkMeter::try_new_v1(4096).expect("work");
        ResourceContext::new(account, &CONTROL, work).expect("resources")
    }

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(SourceRef::new(SourceId::new(89), SourceKind::Input), "t.cbor", bytes, 0)
    }

    /// Encodes an owned value with a profile, driving the encoder session to its finished bytes.
    fn encode(value: &Value, dialect: &str) -> Vec<u8> {
        let mut resources = resources();
        let registration = crate::registration().expect("registration");
        let factory = registration
            .encoder()
            .expect("encoder")
            .create_factory(
                jqf_codec_core::EncodeRequest {
                    format: &jqf_data::FormatId::try_new("cbor").expect("fmt"),
                    dialect: &jqf_data::DialectId::try_new(dialect).expect("dialect"),
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    preservation: jqf_codec_core::PreservationRequest::None,
                    options: None,
                },
                &mut resources,
            )
            .expect("factory");
        let mut session = factory
            .start(
                EncodeItem::owned(value),
                jqf_codec_core::PreservationRequest::None,
                &mut resources,
            )
            .expect("session");
        let mut out = Vec::new();
        {
            let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
            let mut run = CodecRunContext::new(&mut resources);
            session.encode(&mut sink, &mut run).expect("encode");
        }
        out
    }

    /// Decodes CBOR bytes back to a materialized value via the whole route.
    fn decode(bytes: &[u8]) -> jqf_data::Value {
        let mut resources = resources();
        let registration = crate::registration().expect("registration");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::CBOR_PREFERRED_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        let mut demand = jqf_codec_core::CodecDemand::try_new(&resources);
        demand
            .try_insert(&jqf_codec_core::DemandClause::SemanticRoot)
            .expect("root");
        demand
            .try_insert(&jqf_codec_core::DemandClause::ValueShape)
            .expect("shape");
        let requirement = jqf_codec_core::AccessRequirement::try_whole(
            demand,
            jqf_codec_core::AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4096);
        let result = session.decode(&mut run).expect("decode");
        let jqf_codec_core::AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        product
            .document()
            .materialize_root(&mut resources)
            .expect("materialize")
    }

    fn object(pairs: Vec<(&str, Value)>) -> Value {
        let mut builder = jqf_data::ObjectBuilder::try_with_capacity(pairs.len()).expect("builder");
        for (key, value) in pairs {
            builder
                .try_insert_last(jqf_data::ObjectKey::try_from_str(key).expect("key"), value)
                .expect("insert");
        }
        Value::Object(builder.try_finish_unique().expect("object"))
    }

    fn integer(text: &str) -> Value {
        Value::Number(
            jqf_data::Number::try_integer_unaccounted(jqf_data::Integer::parse(text).expect("integer"))
                .expect("number"),
        )
    }

    fn tag(text: &str, payload: Value) -> Value {
        Value::try_tagged(jqf_data::TagId::try_new_unaccounted(text).expect("tag"), payload).expect("tagged")
    }

    /// Renders one value compactly for round-trip assertions.
    fn render(value: &Value) -> String {
        match value {
            Value::Null => "null".into(),
            Value::Bool(true) => "true".into(),
            Value::Bool(false) => "false".into(),
            Value::Number(number) => {
                if let Some(integer) = number.to_integer() {
                    integer.as_str().into()
                } else if let Some(float) = number.as_float() {
                    format!("{}", float.get())
                } else if let Some(decimal) = number.as_decimal() {
                    format!("{}e{}", decimal.coefficient().as_str(), -decimal.scale())
                } else {
                    format!("{number:?}")
                }
            }
            Value::String(text) => format!("{text:?}"),
            Value::Bytes(bytes) => format!("h{:?}", bytes.as_ref()),
            Value::Array(array) => {
                let mut out = String::from("[");
                for (index, item) in array.iter().enumerate() {
                    if index != 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&render(item));
                }
                out.push(']');
                out
            }
            Value::Object(object) => {
                let mut out = String::from("{");
                for (index, entry) in object.iter().enumerate() {
                    if index != 0 {
                        out.push_str(", ");
                    }
                    out.push('"');
                    out.push_str(entry.key());
                    out.push_str("\": ");
                    out.push_str(&render(entry.value()));
                }
                out.push('}');
                out
            }
            Value::Tagged { tag, payload } => format!("{}({})", tag.as_str(), render(payload)),
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn factory_accepts_the_output_profiles() {
        let mut resources = resources();
        let registration = crate::registration().expect("registration");
        for dialect in [
            crate::CBOR_SOURCE_DIALECT_ID,
            crate::CBOR_PREFERRED_DIALECT_ID,
            crate::CBOR_CORE_DETERMINISTIC_DIALECT_ID,
            crate::CBOR_LENGTH_FIRST_DIALECT_ID,
        ] {
            let factory = registration
                .encoder()
                .expect("encoder")
                .create_factory(
                    jqf_codec_core::EncodeRequest {
                        format: &jqf_data::FormatId::try_new("cbor").expect("fmt"),
                        dialect: &jqf_data::DialectId::try_new(dialect).expect("dialect"),
                        diagnostics: DiagnosticPolicy::ErrorsOnly,
                        preservation: jqf_codec_core::PreservationRequest::None,
                        options: None,
                    },
                    &mut resources,
                )
                .map_err(|e| format!("{dialect}: {:?}", e.kind()));
            assert!(factory.is_ok(), "factory must accept {dialect}: {factory:?}");
        }
    }

    #[test]
    fn round_trips_scalars_and_containers() {
        let value = object(vec![
            (
                "a",
                Value::Array(
                    jqf_data::Array::try_from_vec(vec![integer("1"), Value::Bool(true), Value::Null]).expect("array"),
                ),
            ),
            ("n", integer("21267647932558653966388855370447585280")),
        ]);
        let rendered = render(&value);
        for dialect in [
            crate::CBOR_PREFERRED_DIALECT_ID,
            crate::CBOR_CORE_DETERMINISTIC_DIALECT_ID,
        ] {
            let encoded = encode(&value, dialect);
            let decoded = decode(&encoded);
            assert_eq!(render(&decoded), rendered, "round-trip under {dialect}");
        }
    }

    #[test]
    fn round_trips_tags() {
        let value = tag(
            "cbor:tag:55799",
            tag(
                "cbor:tag:34",
                Value::Array(jqf_data::Array::try_from_vec(vec![integer("1"), integer("2")]).expect("array")),
            ),
        );
        let encoded = encode(&value, crate::CBOR_PREFERRED_DIALECT_ID);
        assert_eq!(encoded, vec![0xd9, 0xd9, 0xf7, 0xd8, 0x22, 0x82, 0x01, 0x02]);
        let decoded = decode(&encoded);
        assert_eq!(render(&decoded), "cbor:tag:55799(cbor:tag:34([1, 2]))");
    }

    #[test]
    fn deterministic_profile_sorts_keys() {
        let value = object(vec![("z", integer("1")), ("a", integer("2")), ("m", integer("3"))]);
        let encoded = encode(&value, crate::CBOR_CORE_DETERMINISTIC_DIALECT_ID);
        // a3 61 61 02 61 6d 03 61 7a 01
        assert_eq!(
            encoded,
            vec![0xa3, 0x61, 0x61, 0x02, 0x61, 0x6d, 0x03, 0x61, 0x7a, 0x01]
        );
    }

    /// An owned value has no source span, so `source@1` falls back to preferred bytes (occurrence order, shortest
    /// widths).
    #[test]
    fn source_profile_falls_back_to_preferred_bytes_for_owned_values() {
        let value = object(vec![("z", integer("1")), ("a", integer("2"))]);
        let source = encode(&value, crate::CBOR_SOURCE_DIALECT_ID);
        let preferred = encode(&value, crate::CBOR_PREFERRED_DIALECT_ID);
        assert_eq!(source, preferred);
        // Occurrence order: "z" then "a", not bytewise-sorted.
        assert_eq!(source, vec![0xa2, 0x61, 0x7a, 0x01, 0x61, 0x61, 0x02]);
    }

    /// Mixed-length text keys ("aa" vs "b"): length-then-bytes and bytes-only agree because the definite text head is
    /// monotonic in length. Both deterministic profiles emit the same bytes; preferred keeps occurrence order and
    /// differs.
    #[test]
    fn length_first_matches_core_deterministic_on_mixed_length_text_keys() {
        let value = object(vec![("aa", integer("1")), ("b", integer("2"))]);
        let core = encode(&value, crate::CBOR_CORE_DETERMINISTIC_DIALECT_ID);
        let length_first = encode(&value, crate::CBOR_LENGTH_FIRST_DIALECT_ID);
        let preferred = encode(&value, crate::CBOR_PREFERRED_DIALECT_ID);
        assert_eq!(core, length_first, "text-key model: length-first ≡ core-deterministic");
        // Bytewise / length-first: "b" (0x61 62) then "aa" (0x62 61 61).
        assert_eq!(core, vec![0xa2, 0x61, 0x62, 0x02, 0x62, 0x61, 0x61, 0x01]);
        // Preferred keeps authored order: "aa" then "b".
        assert_eq!(preferred, vec![0xa2, 0x62, 0x61, 0x61, 0x01, 0x61, 0x62, 0x02]);
        assert_ne!(preferred, core);
    }

    #[test]
    fn foreign_or_invalid_tags_are_unrepresentable() {
        for (text, payload) in [
            ("cbor:simple:20", Value::Null),
            ("cbor:simple:31", Value::Null),
            ("not-a-cbor-tag", Value::Null),
            ("cbor:tag:", Value::Null),
        ] {
            let value = tag(text, payload);
            let result = encode_owned_result(&value);
            assert!(
                matches!(result, Err(CodecFailureKind::UnsupportedRepresentation)),
                "expected unrepresentable for {text}"
            );
        }
    }

    /// A document carrying BOTH spellings of one wire tag must REFUSE at encode, never emit two colliding tag heads.
    /// The encoder's number parse is the same canonical law the target validator runs, so the non-canonical member is
    /// refused even on a path that skips validation.
    #[test]
    fn colliding_tag_spellings_refuse_at_encode() {
        let value = Value::Array(
            jqf_data::Array::try_from_vec(alloc::vec![
                tag("cbor:tag:5", Value::Null),
                tag("cbor:tag:05", Value::Null),
            ])
            .expect("array"),
        );
        assert!(
            matches!(
                encode_owned_result(&value),
                Err(CodecFailureKind::UnsupportedRepresentation)
            ),
            "cbor:tag:5 + cbor:tag:05 collide on wire tag 5 and must refuse"
        );
        // And each non-canonical spelling alone refuses too.
        for text in ["cbor:tag:05", "cbor:simple:05", "cbor:tag:+5"] {
            let value = tag(text, Value::Null);
            assert!(
                matches!(
                    encode_owned_result(&value),
                    Err(CodecFailureKind::UnsupportedRepresentation)
                ),
                "{text} is not canonical and must refuse at encode"
            );
        }
        // The shared parse's own boundary, pinned directly.
        assert!(parse_tag_number("0").is_ok());
        assert!(parse_tag_number("18446744073709551615").is_ok());
        for text in ["", "+5", "00", "05", " 5", "5 "] {
            assert!(parse_tag_number(text).is_err(), "{text:?} is not canonical");
        }
    }

    fn encode_owned_result(value: &Value) -> Result<Vec<u8>, CodecFailureKind> {
        let mut resources = resources();
        let registration = crate::registration().expect("registration");
        let factory = registration
            .encoder()
            .expect("encoder")
            .create_factory(
                jqf_codec_core::EncodeRequest {
                    format: &jqf_data::FormatId::try_new("cbor").expect("fmt"),
                    dialect: &jqf_data::DialectId::try_new(crate::CBOR_PREFERRED_DIALECT_ID).expect("dialect"),
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    preservation: jqf_codec_core::PreservationRequest::None,
                    options: None,
                },
                &mut resources,
            )
            .expect("factory");
        let mut session = factory
            .start(
                EncodeItem::owned(value),
                jqf_codec_core::PreservationRequest::None,
                &mut resources,
            )
            .expect("session");
        let mut out = Vec::new();
        {
            let mut sink = jqf_codec_core::VecByteSink::new(&mut out);
            let mut run = CodecRunContext::new(&mut resources);
            match session.encode(&mut sink, &mut run) {
                Ok(_) => {}
                Err(error) => return Err(error.kind()),
            }
        }
        Ok(out)
    }

    // --------------------------------------------------------------------- The splice receipts: the three edit hooks
    // over a document decoded through the eager whole route, whose per-item spans name every header-through-payload
    // extent. Each receipt pins the splice policy's byte bookkeeping: a leaf replaces exactly its own span, an
    // append/remove rewrites exactly the count-bearing head whose count changed (or nothing, for an indefinite
    // container) and nothing above it.

    /// Decodes one document through the eager whole route (per-item spans committed) and returns the product plus a
    /// factory in the preferred profile — the exact pair the SDK edit lane holds when it calls the hooks.
    fn edit_harness(bytes: &[u8]) -> (ErasedEncoderFactory, AccessResult<'_>) {
        let mut resources = resources();
        let registration = crate::registration().expect("registration");
        let factory = registration
            .encoder()
            .expect("encoder")
            .create_factory(
                jqf_codec_core::EncodeRequest {
                    format: &jqf_data::FormatId::try_new("cbor").expect("fmt"),
                    dialect: &jqf_data::DialectId::try_new(crate::CBOR_PREFERRED_DIALECT_ID).expect("dialect"),
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    preservation: jqf_codec_core::PreservationRequest::None,
                    options: None,
                },
                &mut resources,
            )
            .expect("factory");
        let mut provider = registration
            .decoder()
            .expect("decoder")
            .create_provider(
                source(bytes),
                DecodeRequest {
                    validation: ValidationMode::Strict,
                    diagnostics: DiagnosticPolicy::ErrorsOnly,
                    dialect: &DialectId::try_new(crate::CBOR_PREFERRED_DIALECT_ID).expect("dialect"),
                    options: None,
                    allow_adjacent_values: false,
                    value_separator: &[],
                },
                &mut resources,
            )
            .expect("provider");
        let mut demand = CodecDemand::try_new(&resources);
        demand.try_insert(&DemandClause::SemanticRoot).expect("root");
        demand.try_insert(&DemandClause::ValueShape).expect("shape");
        let requirement = AccessRequirement::try_whole(
            demand,
            AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
            &resources,
        )
        .expect("requirement");
        let handle = provider.bind(&requirement).expect("bind");
        let mut session = provider.open(&handle, &mut resources).expect("open");
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = session.decode(&mut run).expect("decode");
        (factory, result)
    }

    #[test]
    fn splice_leaf_replaces_exactly_the_item_span() {
        // `a2 61 61 01 61 62 02` — {"a": 1, "b": 2}. A changed scalar renders its new item bytes; the sibling's bytes
        // are untouched by the SPLICE (the caller applies one patch per span).
        let bytes = [0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x02];
        let (factory, result) = edit_harness(&bytes);
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        let document = product.document();
        let root = document
            .value_view(document.node_handle(document.root()).expect("handle"))
            .expect("view");
        let member = root
            .object()
            .expect("object")
            .expect("root object")
            .get("a")
            .expect("member a");
        let mut resources = resources();
        let rendered = factory
            .render_leaf(
                document,
                member.node(),
                &[String::from("a")],
                &bytes,
                &integer("5"),
                Some(&bytes[3..4]),
                &mut resources,
            )
            .expect("leaf");
        assert_eq!(rendered, vec![0x05], "a changed integer re-encodes as its item");
    }

    #[test]
    fn splice_leaf_rewrites_a_wider_marker_in_place() {
        // The head is part of the spliced item, so a value whose marker width differs needs no separate length
        // bookkeeping: 1 (`01`) to 2^40 (`1b 00 00 01 00 00 00 00 00`) replaces the whole span.
        let bytes = [0x82, 0x01, 0x05];
        let (factory, result) = edit_harness(&bytes);
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        let document = product.document();
        let root = document
            .value_view(document.node_handle(document.root()).expect("handle"))
            .expect("view");
        let item = root.array().expect("array").expect("root array").get(0).expect("item");
        let mut resources = resources();
        let rendered = factory
            .render_leaf(
                document,
                item.node(),
                &[],
                &bytes,
                &integer("1099511627776"),
                Some(&bytes[1..2]),
                &mut resources,
            )
            .expect("leaf");
        assert_eq!(
            rendered,
            vec![0x1b, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00],
            "2^40 re-encodes with its own 8-byte argument head"
        );
    }

    #[test]
    fn splice_append_grows_a_definite_array() {
        // `82 01 02` + [3]: the count head `82` is re-derived to `83` and the new item's bytes land after the last
        // direct child (the span's end). Two disjoint insertions.
        let bytes = [0x82, 0x01, 0x02];
        let (factory, result) = edit_harness(&bytes);
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        let document = product.document();
        let root = document.root();
        let mut resources = resources();
        let insertions = factory
            .render_edit_append(
                document,
                root,
                &[],
                &bytes,
                EditAppendMembers::Array(&[&integer("3")]),
                &mut resources,
            )
            .expect("append");
        assert_eq!(
            insertions,
            vec![
                EditInsertion {
                    at: 0,
                    bytes: vec![0x83],
                    replace: Some((0, 1)),
                },
                EditInsertion {
                    at: 3,
                    bytes: vec![0x03],
                    replace: None,
                },
            ],
            "the count head replacement and the item insertion are the whole splice"
        );
    }

    #[test]
    fn splice_append_rewrites_the_head_across_a_width_boundary() {
        // A 23-item array (`97`) growing to 24 grows its head from one to two bytes (`98 18`) — the width boundary is
        // the policy's whole length bookkeeping.
        let mut bytes = vec![0x97];
        bytes.extend(core::iter::repeat_n(0x01, 23));
        let (factory, result) = edit_harness(&bytes);
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        let document = product.document();
        let root = document.root();
        let mut resources = resources();
        let insertions = factory
            .render_edit_append(
                document,
                root,
                &[],
                &bytes,
                EditAppendMembers::Array(&[&integer("2")]),
                &mut resources,
            )
            .expect("append");
        let head = insertions
            .iter()
            .find(|insertion| insertion.at == 0)
            .expect("head insertion");
        assert_eq!(head.bytes, vec![0x98, 0x18], "23 -> 24 widens the head");
    }

    #[test]
    fn splice_append_indefinite_array_skips_the_head() {
        // `9f 01 02 ff` + [3]: no count head exists to rewrite, so the only insertion is the new item's bytes before
        // the BREAK byte.
        let bytes = [0x9f, 0x01, 0x02, 0xff];
        let (factory, result) = edit_harness(&bytes);
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        let document = product.document();
        let root = document.root();
        let mut resources = resources();
        let insertions = factory
            .render_edit_append(
                document,
                root,
                &[],
                &bytes,
                EditAppendMembers::Array(&[&integer("3")]),
                &mut resources,
            )
            .expect("append");
        assert_eq!(
            insertions,
            vec![EditInsertion {
                at: 3,
                bytes: vec![0x03],
                replace: None,
            }],
            "an indefinite container has no head to rewrite"
        );
    }

    #[test]
    fn splice_append_grows_a_map() {
        // `a1 61 61 01` + ("b", 2): the pair count `a1` becomes `a2` and the key + value bytes land after the last
        // pair.
        let bytes = [0xa1, 0x61, 0x61, 0x01];
        let (factory, result) = edit_harness(&bytes);
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        let document = product.document();
        let root = document.root();
        let mut resources = resources();
        let insertions = factory
            .render_edit_append(
                document,
                root,
                &[],
                &bytes,
                EditAppendMembers::Table(&[("b", &integer("2"))]),
                &mut resources,
            )
            .expect("append");
        assert_eq!(
            insertions,
            vec![
                EditInsertion {
                    at: 0,
                    bytes: vec![0xa2],
                    replace: Some((0, 1)),
                },
                EditInsertion {
                    at: 4,
                    bytes: vec![0x61, 0x62, 0x02],
                    replace: None,
                },
            ],
            "a map member splices key + value"
        );
    }

    #[test]
    fn splice_remove_cuts_an_array_item_and_rewrites_the_head() {
        // `83 01 02 03`, remove index 1: the item's span `[2, 3)` is cut and the count head `83` is re-derived to `82`.
        let bytes = [0x83, 0x01, 0x02, 0x03];
        let (factory, result) = edit_harness(&bytes);
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        let document = product.document();
        let root = document
            .value_view(document.node_handle(document.root()).expect("handle"))
            .expect("view");
        let item = root.array().expect("array").expect("root array").get(1).expect("item");
        let mut resources = resources();
        let removals = factory
            .render_edit_remove(
                document,
                document.root(),
                &[],
                &bytes,
                EditRemoveMembers::Array(&[(1, item.node())]),
                &mut resources,
            )
            .expect("remove");
        // The member cuts are returned in removed-list order and the head cut after them; the SDK sorts the patch set
        // by offset, so the order is not load-bearing.
        assert_eq!(
            removals,
            vec![
                EditRemoval {
                    start: 2,
                    end: 3,
                    replacement: Vec::new(),
                },
                EditRemoval {
                    start: 0,
                    end: 1,
                    replacement: vec![0x82],
                },
            ],
            "the removed item and the head replacement are the whole cut"
        );
    }

    #[test]
    fn splice_remove_cuts_a_map_member_key_and_value() {
        // `a2 61 61 01 61 62 02`, remove "a": the key + value span `[1, 4)` is cut and the pair count `a2` is
        // re-derived to `a1`.
        let bytes = [0xa2, 0x61, 0x61, 0x01, 0x61, 0x62, 0x02];
        let (factory, result) = edit_harness(&bytes);
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("full document")
        };
        let document = product.document();
        let root = document
            .value_view(document.node_handle(document.root()).expect("handle"))
            .expect("view");
        let member = root
            .object()
            .expect("object")
            .expect("root object")
            .get("a")
            .expect("member a");
        let mut resources = resources();
        let removals = factory
            .render_edit_remove(
                document,
                document.root(),
                &[],
                &bytes,
                EditRemoveMembers::Table(&[("a", member.node())]),
                &mut resources,
            )
            .expect("remove");
        assert_eq!(
            removals,
            vec![
                EditRemoval {
                    start: 1,
                    end: 4,
                    replacement: Vec::new(),
                },
                EditRemoval {
                    start: 0,
                    end: 1,
                    replacement: vec![0xa1],
                },
            ],
            "the key+value pair and the head replacement are the whole cut"
        );
    }
}
