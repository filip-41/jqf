//! Validate-only walk plus path locate for the native scoped route.
//!
//! Every skipped byte is validated to the dialect's exact strictness — heads, length bounds, `str` UTF-8 under
//! utf8@1, ext/timestamp shapes, nesting, and trailing content — while the target path is resolved over the wire
//! without building document nodes. A corrupt tail cannot pass a scoped run that the whole-document floor would refuse.

use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind, OwnedStep};
use jqf_data::ValueKind;
use jqf_resource::{DepthGuard, ResourceContext};

use crate::error;
use crate::marker::{MARKERS, Marker};
use crate::options::Dialect;
use jqf_source::ResolvedSource;

/// The located answer of the walk.
#[derive(Clone, Debug)]
pub(crate) enum Located {
    /// The value at the target path, as a byte span.
    Value { start: usize, end: usize },
    /// The step at which navigation stopped: no member or position exists.
    Missing { step: usize },
    /// The step at which a kind mismatch stopped the path.
    TypeMismatch { step: usize, actual: ValueKind },
}

#[derive(Clone, Copy)]
enum Container {
    Array,
    Object,
}

impl Container {
    fn kind(self) -> ValueKind {
        match self {
            Self::Array => ValueKind::Array,
            Self::Object => ValueKind::Object,
        }
    }
}

type StepCtx = Option<usize>;

struct Walker<'a> {
    source: ResolvedSource<'a>,
    bytes: &'a [u8],
    dialect: Dialect,
    steps: &'a [OwnedStep],
    pos: usize,
    outcome: Option<Located>,
    resources: &'a ResourceContext<'a>,
}

/// Validates the input and resolves the exact path, returning the located span (or a negative observation) together
/// with the end of the top-level item.
///
/// Cooperativity is deliberately asymmetric here: the walk itself runs one uncheckpointed pass over the retained input
/// (its only guard is the nesting ceiling), and only the key-equivalence skeleton pass below runs with a cooperative
/// credit budget. A large located request therefore monopolizes its poll quantum in this walk by design; splitting the
/// walk into resumable polls is the recorded owe, not an oversight.
pub(crate) fn locate(
    source: ResolvedSource<'_>,
    dialect: Dialect,
    steps: &[OwnedStep],
    resources: &mut ResourceContext<'_>,
) -> Result<(Located, usize), CodecError> {
    let mut walker = Walker {
        source,
        bytes: source.bytes(),
        dialect,
        steps,
        pos: 0,
        outcome: None,
        resources,
    };
    walker.walk_item(Some(0), 0)?;
    if walker.pos != walker.bytes.len() {
        return Err(error::invalid(
            walker.source,
            walker.pos,
            "trailing-bytes",
            "bytes remain after the top-level MessagePack object",
        ));
    }
    let item_end = walker.pos;
    let outcome = walker.outcome.take().ok_or_else(|| {
        CodecError::new(CodecFailureKind::InternalContractViolation {
            contract: "MessagePack walk produced no located answer",
        })
    })?;
    #[expect(
        clippy::drop_non_drop,
        reason = "ends the walk's mutable resource borrow before the key-equivalence skeleton \
                  pass re-enters the context; Walker has no destructor, so this is a \
                  borrow-scope cut, not a destruction site"
    )]
    drop(walker);
    if dialect == Dialect::KeyEquivalence {
        // The duplicate-key law compares native keys, including nested maps. The walk's skip does not retain those
        // identities, so the dialect still runs the skeleton comparator over the whole input.
        let mut run = jqf_codec_core::CodecRunContext::new(resources);
        run.set_cooperative_credits(4_096);
        let skeleton = crate::scan::scan(source, dialect, &mut run)?;
        crate::keys::validate_duplicate_keys(&skeleton, source, resources)?;
    }
    Ok((outcome, item_end))
}

impl Walker<'_> {
    fn walk_item(&mut self, step: StepCtx, value_start: usize) -> Result<(), CodecError> {
        if step.is_none() {
            return self.skip_item();
        }
        let start = self.pos;
        let marker = self.read_marker()?;
        match marker {
            Marker::Fixarray | Marker::Array16 | Marker::Array32 => {
                let count = self.read_count(marker)?;
                self.walk_container(step, start, Container::Array, count)?;
            }
            Marker::Fixmap | Marker::Map16 | Marker::Map32 => {
                let count = self.read_count(marker)?;
                self.walk_container(step, start, Container::Object, count)?;
            }
            _ => {
                let kind = self.skip_scalar(start, marker)?;
                self.note_scalar_mismatch(step, kind);
                self.note_value(step, value_start, self.pos);
            }
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one container walk: resolve the navigated member/index, validate every child, and step through; splitting it would thread the step context through helpers that each read one piece"
    )]
    fn walk_container(
        &mut self,
        step: StepCtx,
        value_start: usize,
        container: Container,
        count: u64,
    ) -> Result<(), CodecError> {
        let _guard = self.resources.enter_nesting().map_err(CodecError::from)?;
        let pending = match step {
            Some(i) if i < self.steps.len() => Some(i),
            _ => None,
        };
        let mut target_member: Option<&str> = None;
        let mut target_index: Option<i64> = None;
        let mut navigable = true;
        if let Some(step_index) = pending {
            match (&self.steps[step_index], container) {
                (OwnedStep::Member(member), Container::Object) => {
                    target_member = Some(member.as_str());
                }
                (OwnedStep::Index(index), Container::Array) => {
                    target_index = Some(*index);
                }
                (OwnedStep::Member(_), Container::Array) | (OwnedStep::Index(_), Container::Object) => {
                    self.note_mismatch(step_index, container.kind());
                    navigable = false;
                }
                (OwnedStep::Range { .. }, _) => {
                    return Err(jqf_codec_core::data_contract(
                        "MessagePack walk does not serve range steps",
                    ));
                }
            }
        }
        match container {
            Container::Array => {
                let len = usize::try_from(count).unwrap_or(usize::MAX);
                let resolved = pending
                    .filter(|_| navigable)
                    .and(target_index)
                    .and_then(|index| jqf_data::resolve_index(len, index));
                for position in 0..len {
                    let child_step = match (navigable, pending, resolved) {
                        (true, Some(index), Some(want)) if position == want => Some(
                            index
                                .checked_add(1)
                                .ok_or_else(|| jqf_codec_core::data_contract("MessagePack walk step overflow"))?,
                        ),
                        _ => None,
                    };
                    self.walk_item(child_step, self.pos)?;
                }
                if navigable
                    && target_index.is_some()
                    && resolved.is_none()
                    && let Some(index) = pending
                {
                    self.note_missing(index);
                }
            }
            Container::Object => {
                let mut matched = false;
                for _ in 0..count {
                    let key_start = self.pos;
                    let key_marker = self.read_marker()?;
                    let is_str = matches!(
                        key_marker,
                        Marker::Fixstr | Marker::Str8 | Marker::Str16 | Marker::Str32
                    );
                    let key_text = if is_str {
                        let len = usize::try_from(self.read_count(key_marker)?).map_err(|_| {
                            error::invalid(
                                self.source,
                                key_start,
                                "length-overflow",
                                "a str length does not fit this platform",
                            )
                        })?;
                        let check_utf8 = matches!(self.dialect, Dialect::Utf8 | Dialect::KeyEquivalence);
                        let payload = self.read_payload(key_start, len)?;
                        if check_utf8 && core::str::from_utf8(payload).is_err() {
                            return Err(error::invalid(
                                self.source,
                                key_start,
                                "invalid-utf8",
                                "a str payload is not valid UTF-8 under messagepack.utf8@1",
                            ));
                        }
                        core::str::from_utf8(payload).ok()
                    } else {
                        // Any MessagePack value may be a map key, so a non-str key can be a container; rewind to the
                        // key start (the marker byte is already consumed) and skip it with the same validated frame
                        // loop as any off-path value.
                        self.pos = key_start;
                        self.skip_item()?;
                        None
                    };
                    let child_step = match (navigable, pending, &target_member, key_text) {
                        (true, Some(index), Some(target), Some(text)) if text == *target => {
                            matched = true;
                            Some(
                                index
                                    .checked_add(1)
                                    .ok_or_else(|| jqf_codec_core::data_contract("MessagePack walk step overflow"))?,
                            )
                        }
                        _ => None,
                    };
                    // Duplicate str keys are legal (the ObjectBuilder's final-value law): the LAST matching member
                    // decides, so each new match re-arms the recorder over any earlier match's answer.
                    if child_step.is_some() {
                        self.outcome = None;
                    }
                    self.walk_item(child_step, self.pos)?;
                }
                if navigable
                    && target_member.is_some()
                    && !matched
                    && let Some(index) = pending
                {
                    self.note_missing(index);
                }
            }
        }
        self.note_value(step, value_start, self.pos);
        Ok(())
    }

    fn skip_item(&mut self) -> Result<(), CodecError> {
        let mut frames: Vec<(u64, DepthGuard<'_>)> = Vec::new();
        loop {
            if let Some((remaining, _)) = frames.last_mut() {
                if *remaining == 0 {
                    frames.pop();
                    if frames.is_empty() {
                        return Ok(());
                    }
                    continue;
                }
                *remaining -= 1;
            }
            let start = self.pos;
            let marker = self.read_marker()?;
            match marker {
                Marker::Fixarray
                | Marker::Array16
                | Marker::Array32
                | Marker::Fixmap
                | Marker::Map16
                | Marker::Map32 => {
                    let count = self.read_count(marker)?;
                    let owed = if matches!(marker, Marker::Fixmap | Marker::Map16 | Marker::Map32) {
                        count.saturating_mul(2)
                    } else {
                        count
                    };
                    let guard = self.resources.enter_nesting().map_err(CodecError::from)?;
                    frames.push((owed, guard));
                    if owed == 0 {
                        frames.pop();
                        if frames.is_empty() {
                            return Ok(());
                        }
                    }
                }
                _ => {
                    self.skip_scalar(start, marker)?;
                    if frames.is_empty() {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn skip_scalar(&mut self, start: usize, marker: Marker) -> Result<ValueKind, CodecError> {
        match marker {
            Marker::Nil => Ok(ValueKind::Null),
            Marker::False | Marker::True => Ok(ValueKind::Bool),
            Marker::PosFixint
            | Marker::NegFixint
            | Marker::Uint8
            | Marker::Uint16
            | Marker::Uint32
            | Marker::Uint64
            | Marker::Int8
            | Marker::Int16
            | Marker::Int32
            | Marker::Int64 => {
                self.skip_int_payload(marker)?;
                Ok(ValueKind::Number)
            }
            Marker::Float32 => {
                self.read_be::<4>()?;
                Ok(ValueKind::Number)
            }
            Marker::Float64 => {
                self.read_be::<8>()?;
                Ok(ValueKind::Number)
            }
            Marker::Fixstr | Marker::Str8 | Marker::Str16 | Marker::Str32 => {
                let len = usize::try_from(self.read_count(marker)?).map_err(|_| {
                    error::invalid(
                        self.source,
                        start,
                        "length-overflow",
                        "a str length does not fit this platform",
                    )
                })?;
                let check_utf8 = matches!(self.dialect, Dialect::Utf8 | Dialect::KeyEquivalence);
                let payload = self.read_payload(start, len)?;
                if check_utf8 && core::str::from_utf8(payload).is_err() {
                    return Err(error::invalid(
                        self.source,
                        start,
                        "invalid-utf8",
                        "a str payload is not valid UTF-8 under messagepack.utf8@1",
                    ));
                }
                Ok(ValueKind::String)
            }
            Marker::Bin8 | Marker::Bin16 | Marker::Bin32 => {
                let len = usize::try_from(self.read_count(marker)?).map_err(|_| {
                    error::invalid(
                        self.source,
                        start,
                        "length-overflow",
                        "a bin length does not fit this platform",
                    )
                })?;
                self.read_payload(start, len)?;
                Ok(ValueKind::Bytes)
            }
            Marker::Fixext1
            | Marker::Fixext2
            | Marker::Fixext4
            | Marker::Fixext8
            | Marker::Fixext16
            | Marker::Ext8
            | Marker::Ext16
            | Marker::Ext32 => {
                let len = match marker {
                    Marker::Fixext1 => 1,
                    Marker::Fixext2 => 2,
                    Marker::Fixext4 => 4,
                    Marker::Fixext8 => 8,
                    Marker::Fixext16 => 16,
                    _ => usize::try_from(self.read_count(marker)?).map_err(|_| {
                        error::invalid(
                            self.source,
                            start,
                            "length-overflow",
                            "an ext length does not fit this platform",
                        )
                    })?,
                };
                let ty = self.read_i8()?;
                let payload = self.read_payload(start, len)?;
                if ty == -1 {
                    if crate::scan::timestamp_from_payload(payload).is_some() {
                        return Ok(ValueKind::OffsetDateTime);
                    }
                    // Invalid reserved -1 is refused at the semantic build; the walk still consumes it so a scoped run
                    // sees the same well-formedness as the floor, and the span re-decode refuses it.
                    return Ok(ValueKind::Bytes);
                }
                Ok(ValueKind::Bytes)
            }
            Marker::NeverUsed => unreachable!("rejected at head time"),
            _ => unreachable!("container families dispatch before skip_scalar"),
        }
    }

    fn skip_int_payload(&mut self, marker: Marker) -> Result<(), CodecError> {
        // Fixints (positive and negative) are the marker byte itself and carry no payload, like every non-integer
        // marker that reaches the wildcard.
        match marker {
            Marker::Uint8 | Marker::Int8 => {
                self.read_u8()?;
                Ok(())
            }
            Marker::Uint16 | Marker::Int16 => {
                self.read_be::<2>()?;
                Ok(())
            }
            Marker::Uint32 | Marker::Int32 => {
                self.read_be::<4>()?;
                Ok(())
            }
            Marker::Uint64 | Marker::Int64 => {
                self.read_be::<8>()?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn read_marker(&mut self) -> Result<Marker, CodecError> {
        if self.pos >= self.bytes.len() {
            return Err(error::invalid(
                self.source,
                self.pos,
                "eof",
                "input ends in the middle of a MessagePack object",
            ));
        }
        let byte = self.bytes[self.pos];
        self.pos += 1;
        let marker = MARKERS[byte as usize];
        if marker == Marker::NeverUsed {
            return Err(error::invalid(
                self.source,
                self.pos - 1,
                "reserved-byte",
                "0xc1 is never used in MessagePack",
            ));
        }
        Ok(marker)
    }

    fn read_count(&mut self, marker: Marker) -> Result<u64, CodecError> {
        let byte = self.bytes[self.pos - 1];
        Ok(match marker {
            Marker::Fixstr | Marker::Fixarray | Marker::Fixmap => marker.embedded_count(byte),
            Marker::Bin8 | Marker::Ext8 | Marker::Str8 => u64::from(self.read_u8()?),
            Marker::Bin16 | Marker::Ext16 | Marker::Str16 | Marker::Array16 | Marker::Map16 => {
                u64::from(self.read_u16()?)
            }
            Marker::Bin32 | Marker::Ext32 | Marker::Str32 | Marker::Array32 | Marker::Map32 => {
                u64::from(self.read_u32()?)
            }
            _ => 0,
        })
    }

    fn read_payload(&mut self, start: usize, len: usize) -> Result<&[u8], CodecError> {
        let Some(end) = self.pos.checked_add(len) else {
            return Err(error::invalid(
                self.source,
                start,
                "length-overflow",
                "a payload length overflows this platform",
            ));
        };
        if end > self.bytes.len() {
            return Err(error::invalid(
                self.source,
                self.pos,
                "eof",
                "a payload length runs past the end of the input",
            ));
        }
        let payload = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(payload)
    }

    fn read_u8(&mut self) -> Result<u8, CodecError> {
        if self.pos >= self.bytes.len() {
            return Err(error::invalid(
                self.source,
                self.pos,
                "eof",
                "input ends in the middle of a MessagePack object",
            ));
        }
        let value = self.bytes[self.pos];
        self.pos += 1;
        Ok(value)
    }

    fn read_i8(&mut self) -> Result<i8, CodecError> {
        Ok(self.read_u8()? as i8)
    }

    fn read_be<const N: usize>(&mut self) -> Result<[u8; N], CodecError> {
        let end = self.pos + N;
        if end > self.bytes.len() {
            return Err(error::invalid(
                self.source,
                self.pos,
                "eof",
                "input ends in the middle of a MessagePack object",
            ));
        }
        let bytes = self.bytes[self.pos..end].try_into().map_err(|_| {
            error::invalid(
                self.source,
                self.pos,
                "eof",
                "input ends in the middle of a MessagePack object",
            )
        })?;
        self.pos = end;
        Ok(bytes)
    }

    fn read_u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(self.read_be()?))
    }

    fn read_u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(self.read_be()?))
    }

    fn note_value(&mut self, step: StepCtx, start: usize, end: usize) {
        if self.outcome.is_none() && step == Some(self.steps.len()) {
            self.outcome = Some(Located::Value { start, end });
        }
    }

    fn note_missing(&mut self, step: usize) {
        if self.outcome.is_none() {
            self.outcome = Some(Located::Missing { step });
        }
    }

    fn note_mismatch(&mut self, step: usize, actual: ValueKind) {
        if self.outcome.is_none() {
            self.outcome = Some(Located::TypeMismatch { step, actual });
        }
    }

    fn note_scalar_mismatch(&mut self, step: StepCtx, actual: ValueKind) {
        if self.outcome.is_none()
            && let Some(index) = step
            && index < self.steps.len()
        {
            self.note_mismatch(index, actual);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Dialect;
    use crate::test_support;
    use alloc::string::String;

    fn locate_bytes(bytes: &[u8], steps: &[OwnedStep]) -> Result<Located, String> {
        let mut resources = test_support::resources();
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(91), jqf_source::SourceKind::Input),
            "walk.test",
            bytes,
            0,
        );
        locate(source, Dialect::Utf8, steps, &mut resources)
            .map(|(located, _)| located)
            .map_err(|error| alloc::format!("{error:?}"))
    }

    #[test]
    fn a_corrupt_tail_fails_the_scoped_walk() {
        // fixmap {a:1} plus a reserved 0xc1 tail.
        let error = locate_bytes(&[0x81, 0xa1, b'a', 0x01, 0xc1], &[]).expect_err("tail");
        assert!(error.contains("reserved-byte") || error.contains("trailing"), "{error}");
    }

    #[test]
    fn locate_resolves_a_map_member() {
        // {a:1, b:2}
        let bytes = [0x82, 0xa1, b'a', 0x01, 0xa1, b'b', 0x02];
        let located = locate_bytes(&bytes, &[OwnedStep::Member(alloc::string::String::from("b"))]).expect("locate .b");
        match located {
            Located::Value { start, end } => {
                assert_eq!(&bytes[start..end], &[0x02]);
            }
            other => panic!("expected value, got {other:?}"),
        }
    }

    #[test]
    fn a_container_key_walks_without_panicking() {
        // fixmap(1) keyed by an empty fixarray, value posfixint 0.
        let located = locate_bytes(
            &[0x81, 0x90, 0x00],
            &[OwnedStep::Member(alloc::string::String::from("x"))],
        )
        .expect("a container-keyed map is well-formed");
        assert!(matches!(located, Located::Missing { step: 0 }));
    }

    #[test]
    fn a_missing_member_is_a_missing_observation() {
        let bytes = [0x81, 0xa1, b'a', 0x01];
        let located = locate_bytes(&bytes, &[OwnedStep::Member(alloc::string::String::from("nope"))]).expect("miss");
        assert!(matches!(located, Located::Missing { step: 0 }));
    }

    #[test]
    fn a_duplicate_key_resolves_the_final_value() {
        // {a:1, a:2}: duplicate str keys are legal, and the ObjectBuilder's final-value law makes the whole-document
        // floor answer 2 — the located walk must name the LAST `a`'s span, not the first's.
        let bytes = [0x82, 0xa1, b'a', 0x01, 0xa1, b'a', 0x02];
        let located = locate_bytes(&bytes, &[OwnedStep::Member(alloc::string::String::from("a"))]).expect("locate .a");
        match located {
            Located::Value { start, end } => {
                assert_eq!(&bytes[start..end], &[0x02], "final value wins");
            }
            other => panic!("expected value, got {other:?}"),
        }
    }
}
