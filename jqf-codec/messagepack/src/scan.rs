//! The validating scan: source bytes to a span skeleton.
//!
//! One straight-line, ITERATIVE walk (an explicit container frame stack, never recursion — a `MessagePack` map key
//! may be any value, so `0x81` repeated is one open level per byte of untrusted input) turns the whole source into a
//! skeleton of pre-order items carrying their exact authored spans and their decoded kinds. The scan is a VALIDATING
//! scan — every grammar clause is checked here, so a terminal failure names the clause it violated, and
//! materialization cannot fail on grammar. It is CORRUPT-LATE (a corrupt byte anywhere in the file fails the whole
//! scan, never a silent prefix) and WORK-CHECKED at each item head.
//!
//! ## The dialect split
//!
//! Under `messagepack.utf8@1` every `str` payload is validated as UTF-8 here (`InvalidInput`). Under
//! `messagepack.wire@1` (registered, unadvertised) the payload stays raw bytes and the SEMANTIC build refuses the
//! offending span with `UnsupportedRepresentation` — the two dialects differ only in where an invalid-UTF-8 `str`
//! fails.
//!
//! ## The scan's laws
//!
//! - `0xc1` is rejected at head time.
//! - One top-level object; every byte after it is trailing content (the adjacent-value opt-in is refused in `decode`).
//! - Container, `str`, `bin`, and extension lengths must fit the remaining input; a count or payload that runs past the
//!   end is `Eof` at the byte   where it is detected.
//! - Nesting is bounded by the governed ceiling: every open container holds the [`DepthGuard`] that admitted its level,
//!   so a document deeper than the request's `max_nesting_depth` is refused before its frame is pushed.
//! - Extension `-1` with a structurally valid timestamp payload (4/8/12 bytes, `nanoseconds` ≤ `999_999_999`)
//!   projects to [`ItemKind::Timestamp`]; any other reserved `-1` payload stays [`ItemKind::Ext`] and is refused at the
//!   semantic build, never silently accepted.

use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecRunContext};
use jqf_resource::{OwnedDepthGuard, WorkAdmission};
use jqf_source::{ResolvedSource, Span};

use crate::error;
use crate::marker::{MARKERS, Marker};
use crate::options::Dialect;

/// The span skeleton of one whole document.
#[derive(Debug)]
pub(crate) struct Skeleton {
    /// All items in pre-order; a container's children are the indices its kind carries.
    pub(crate) items: Vec<Item>,
    /// The index of the top-level item.
    pub(crate) root: usize,
}

/// One decoded item: its semantic kind and its exact authored byte extent.
#[derive(Debug)]
pub(crate) struct Item {
    pub(crate) kind: ItemKind,
    /// The item's exact authored bytes: header through payload (a container spans its whole header through its last
    /// member's last byte).
    pub(crate) span: Span,
}

/// The decoded semantic kind of one item.
#[derive(Debug)]
pub(crate) enum ItemKind {
    /// `nil`.
    Null,
    /// `false` / `true`.
    Bool(bool),
    /// An integer from the uint/int marker families.
    Integer(IntVal),
    /// A float32 (widened to binary64) or float64 payload.
    Float(f64),
    /// A `str` payload span (the payload bytes, not the item header). UTF-8 validated under `utf8@1` by the scan, and
    /// by the semantic build under `wire@1`.
    Str(Span),
    /// A `bin` payload span.
    Bin(Span),
    /// An array; the child item indices, in order.
    Array(Vec<usize>),
    /// A map; the child item indices flattened as `[k0, v0, k1, v1, ..]`.
    Map(Vec<usize>),
    /// A non-timestamp extension: type code plus opaque payload span.
    Ext { ty: i8, payload: Span },
    /// Extension `-1` with a valid timestamp payload.
    Timestamp { seconds: i64, nanoseconds: u32 },
}

/// One integer value from the uint/int families, kept in the widest native form that holds it (everything above
/// `i64::MAX` stays a `u64`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct IntVal {
    pub(crate) negative: bool,
    pub(crate) magnitude: u64,
}

impl IntVal {
    /// The signed value when it fits `i64` (every negative value does, being bounded by `-2^63`; a positive magnitude
    /// above `i64::MAX` stays a `u64`).
    #[must_use]
    pub(crate) fn to_i64(self) -> Option<i64> {
        if self.negative {
            // `-1 - magnitude`; every magnitude ≤ 2^63-1 fits.
            Some(-(self.magnitude as i64) - 1)
        } else if i64::try_from(self.magnitude).is_ok() {
            Some(self.magnitude as i64)
        } else {
            None
        }
    }

    /// The canonical integer for this value. The machine arm does not allocate; magnitudes above `i64::MAX` take the
    /// boxed spelling.
    #[must_use]
    pub(crate) fn to_integer(self) -> jqf_data::Integer {
        if let Some(value) = self.to_i64() {
            jqf_data::Integer::from_i64(value)
        } else {
            jqf_data::Integer::parse(&alloc::format!("{}", self.magnitude))
                .expect("a u64 decimal spelling is a canonical integer")
        }
    }
}

/// One OPEN container on the explicit stack.
struct Frame {
    /// The container's item index in the arena.
    item: usize,
    /// The container's first byte (its span's start, sealed at close).
    start: usize,
    /// Children still owed: array items, or map key/value entries (pairs flattened, so a map of N entries owes `2N`
    /// children).
    remaining: u64,
    /// The [`DepthGuard`] that admitted this level: the governed nesting ceiling is enforced here, before the frame is
    /// pushed.
    _depth: OwnedDepthGuard,
}

struct Scanner<'a> {
    source: ResolvedSource<'a>,
    bytes: &'a [u8],
    dialect: Dialect,
    pos: usize,
    items: Vec<Item>,
    frames: Vec<Frame>,
}

/// Scans one whole source into its span skeleton.
///
/// # Errors
///
/// Returns a terminal decode failure naming the violated clause.
pub(crate) fn scan(
    source: ResolvedSource<'_>,
    dialect: Dialect,
    resources: &mut CodecRunContext<'_, '_>,
) -> Result<Skeleton, CodecError> {
    let bytes = source.bytes();
    // Span offsets are u32 (jqf-source's law); a source whose byte count exceeds that space has no representable spans,
    // so it refuses here instead of failing mid-scan at span construction.
    if bytes.len() > u32::MAX as usize {
        return Err(error::invalid(
            source,
            0,
            "length-overflow",
            "the source exceeds the u32 span-offset space",
        ));
    }
    let mut scanner = Scanner {
        source,
        bytes,
        dialect,
        pos: 0,
        items: Vec::new(),
        frames: Vec::new(),
    };
    let root = scanner.run(resources)?;
    if scanner.pos != scanner.bytes.len() {
        return Err(error::invalid(
            scanner.source,
            scanner.pos,
            "trailing-bytes",
            "bytes remain after the top-level MessagePack object",
        ));
    }
    Ok(Skeleton {
        items: scanner.items,
        root,
    })
}

impl Scanner<'_> {
    fn run(&mut self, resources: &mut CodecRunContext<'_, '_>) -> Result<usize, CodecError> {
        loop {
            Self::check_work(resources)?;
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
                    let (item, kind) = match marker {
                        Marker::Fixmap | Marker::Map16 | Marker::Map32 => {
                            (self.open_item(start, ItemKind::Map(Vec::new())), count * 2)
                        }
                        _ => (self.open_item(start, ItemKind::Array(Vec::new())), count),
                    };
                    let guard = resources.resources().enter_nesting_owned().map_err(CodecError::from)?;
                    self.frames.push(Frame {
                        item,
                        start,
                        remaining: kind,
                        _depth: guard,
                    });
                    if kind == 0 {
                        let frame = self.frames.pop().ok_or_else(data_contract)?;
                        let item = frame.item;
                        self.seal_container(item, frame.start);
                        if let Some(root) = self.deliver(item) {
                            return Ok(root);
                        }
                    }
                }
                _ => {
                    let item = self.read_scalar(start, marker)?;
                    if let Some(root) = self.deliver(item) {
                        return Ok(root);
                    }
                }
            }
        }
    }

    /// One cooperative work check at each item head. One credit per item, so a 4096-credit meter yields every 4096
    /// items (the CBOR/jqfb quantum), not a drain of the whole remaining grant discarded on `Granted`.
    fn check_work(resources: &mut CodecRunContext<'_, '_>) -> Result<(), CodecError> {
        if resources.resources().admit_work_transition()? == WorkAdmission::Pending {
            resources.replenish_work()?;
        }
        Ok(())
    }

    /// Reads and validates the next marker byte.
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

    /// Reads the unsigned length of a length-carrying family (the embedded count for the fix families, the big-endian
    /// field otherwise).
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

    /// Appends `item` to the open frame's container (if any), popping every frame the append completes. Returns the
    /// index of the value that completed the TOP-LEVEL item.
    fn deliver(&mut self, mut item: usize) -> Option<usize> {
        loop {
            match self.frames.last_mut() {
                None => return Some(item),
                Some(frame) => {
                    match &mut self.items[frame.item].kind {
                        ItemKind::Array(children) | ItemKind::Map(children) => children.push(item),
                        _ => return None,
                    }
                    frame.remaining -= 1;
                    if frame.remaining > 0 {
                        return None;
                    }
                    let frame = self.frames.pop().unwrap();
                    self.seal_container(frame.item, frame.start);
                    item = frame.item;
                }
            }
        }
    }

    fn seal_container(&mut self, item: usize, start: usize) {
        self.items[item].span = span(start, self.pos);
    }

    /// Creates an item with a span that is filled when its value completes (scalars fill immediately; containers seal
    /// at close).
    fn open_item(&mut self, start: usize, kind: ItemKind) -> usize {
        self.items.push(Item {
            kind,
            span: span(start, start),
        });
        self.items.len() - 1
    }

    /// Reads one scalar (non-container) item into the arena, returning its index. The span runs from `start` through
    /// the payload's last byte.
    fn read_scalar(&mut self, start: usize, marker: Marker) -> Result<usize, CodecError> {
        let kind = match marker {
            Marker::Nil => ItemKind::Null,
            Marker::False => ItemKind::Bool(false),
            Marker::True => ItemKind::Bool(true),
            Marker::PosFixint => ItemKind::Integer(IntVal {
                negative: false,
                magnitude: u64::from(self.bytes[start]),
            }),
            Marker::NegFixint => ItemKind::Integer(signed_int_val(marker.negative_fixint(self.bytes[start]))),
            Marker::Uint8 => ItemKind::Integer(unsigned_int_val(u64::from(self.read_u8()?))),
            Marker::Uint16 => ItemKind::Integer(unsigned_int_val(u64::from(self.read_u16()?))),
            Marker::Uint32 => ItemKind::Integer(unsigned_int_val(u64::from(self.read_u32()?))),
            Marker::Uint64 => ItemKind::Integer(unsigned_int_val(self.read_u64()?)),
            Marker::Int8 => ItemKind::Integer(signed_int_val(i64::from(self.read_i8()?))),
            Marker::Int16 => ItemKind::Integer(signed_int_val(i64::from(self.read_i16()?))),
            Marker::Int32 => ItemKind::Integer(signed_int_val(i64::from(self.read_i32()?))),
            Marker::Int64 => ItemKind::Integer(signed_int_val(self.read_i64()?)),
            Marker::Float32 => {
                let bits = self.read_u32()?;
                ItemKind::Float(f64::from_bits(jqf_codec_core::widen_f32(bits)))
            }
            Marker::Float64 => ItemKind::Float(f64::from_bits(self.read_u64()?)),
            Marker::Fixstr | Marker::Str8 | Marker::Str16 | Marker::Str32 => {
                let len = usize::try_from(self.read_count(marker)?).map_err(|_| {
                    error::invalid(
                        self.source,
                        start,
                        "length-overflow",
                        "a str length does not fit this platform",
                    )
                })?;
                self.read_str(start, len)?
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
                ItemKind::Bin(self.read_payload_span(start, len)?)
            }
            Marker::Fixext1 | Marker::Fixext2 | Marker::Fixext4 | Marker::Fixext8 | Marker::Fixext16 => {
                let len = match marker {
                    Marker::Fixext1 => 1,
                    Marker::Fixext2 => 2,
                    Marker::Fixext4 => 4,
                    Marker::Fixext8 => 8,
                    _ => 16,
                };
                self.read_ext(start, len)?
            }
            Marker::Ext8 | Marker::Ext16 | Marker::Ext32 => {
                let len = usize::try_from(self.read_count(marker)?).map_err(|_| {
                    error::invalid(
                        self.source,
                        start,
                        "length-overflow",
                        "an ext length does not fit this platform",
                    )
                })?;
                self.read_ext(start, len)?
            }
            Marker::NeverUsed => unreachable!("rejected at head time"),
            _ => unreachable!("container families dispatch before read_scalar"),
        };
        let index = self.open_item(start, kind);
        self.items[index].span = span(start, self.pos);
        Ok(index)
    }

    /// Reads one `str` payload, validating UTF-8 under `utf8@1` and `key-equivalence@1`.
    fn read_str(&mut self, start: usize, len: usize) -> Result<ItemKind, CodecError> {
        let payload = self.read_payload_span(start, len)?;
        let bytes = self.payload_bytes(payload);
        if matches!(self.dialect, Dialect::Utf8 | Dialect::KeyEquivalence) && core::str::from_utf8(bytes).is_err() {
            return Err(error::invalid(
                self.source,
                start,
                "invalid-utf8",
                "a str payload is not valid UTF-8 under messagepack.utf8@1",
            ));
        }
        Ok(ItemKind::Str(payload))
    }

    /// Reads one extension: the signed type byte then the payload. Extension `-1` with a valid timestamp payload
    /// projects to [`ItemKind::Timestamp`]; any other `-1` payload stays an [`ItemKind::Ext`] and is refused at the
    /// semantic build.
    fn read_ext(&mut self, start: usize, len: usize) -> Result<ItemKind, CodecError> {
        let ty = self.read_i8()?;
        let payload = self.read_payload_span(start, len)?;
        if ty == -1
            && let Some(timestamp) = timestamp_from_payload(self.payload_bytes(payload))
        {
            return Ok(ItemKind::Timestamp {
                seconds: timestamp.0,
                nanoseconds: timestamp.1,
            });
        }
        Ok(ItemKind::Ext { ty, payload })
    }

    /// Reads `len` payload bytes as a source span, rejecting a length that runs past the end.
    fn read_payload_span(&mut self, start: usize, len: usize) -> Result<Span, CodecError> {
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
        let payload = span(self.pos, end);
        self.pos = end;
        Ok(payload)
    }

    fn payload_bytes(&self, payload: Span) -> &[u8] {
        &self.bytes[payload.start() as usize..payload.end() as usize]
    }

    fn read_u8(&mut self) -> Result<u8, CodecError> {
        if self.pos >= self.bytes.len() {
            return Err(self.eof());
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
            return Err(self.eof());
        }
        let bytes = self.bytes[self.pos..end].try_into().map_err(|_| self.eof())?;
        self.pos = end;
        Ok(bytes)
    }

    fn read_u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(self.read_be()?))
    }

    fn read_i16(&mut self) -> Result<i16, CodecError> {
        Ok(self.read_u16()? as i16)
    }

    fn read_u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(self.read_be()?))
    }

    fn read_i32(&mut self) -> Result<i32, CodecError> {
        Ok(self.read_u32()? as i32)
    }

    fn read_u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(self.read_be()?))
    }

    fn read_i64(&mut self) -> Result<i64, CodecError> {
        Ok(self.read_u64()? as i64)
    }

    fn eof(&self) -> CodecError {
        error::invalid(
            self.source,
            self.pos,
            "eof",
            "input ends in the middle of a MessagePack object",
        )
    }
}

/// An unsigned integer in `(negative: false, magnitude)` form.
fn unsigned_int_val(magnitude: u64) -> IntVal {
    IntVal {
        negative: false,
        magnitude,
    }
}

/// A signed integer from the intN families: positive payloads are ordinary magnitudes, negative payloads keep the `-1 -
/// magnitude` form.
fn signed_int_val(value: i64) -> IntVal {
    if value >= 0 {
        IntVal {
            negative: false,
            magnitude: value as u64,
        }
    } else {
        IntVal {
            negative: true,
            magnitude: (-1 - value) as u64,
        }
    }
}

/// A validated timestamp payload: extension `-1` with 4/8/12 payload bytes and `nanoseconds` ≤ `999_999_999`. Returns
/// `(seconds, nanoseconds)`.
///
/// - 32-bit (`0xd6 0xff`): `u32` seconds, zero nanoseconds.
/// - 64-bit (`0xd7 0xff`): `(nanoseconds << 34) | seconds`, nanoseconds in the top 30 bits.
/// - 96-bit (`0xc7 0x0c 0xff`): `u32` nanoseconds then signed `i64` seconds.
pub(crate) fn timestamp_from_payload(payload: &[u8]) -> Option<(i64, u32)> {
    match payload.len() {
        4 => {
            let seconds = u32::from_be_bytes(payload.try_into().ok()?);
            Some((i64::from(seconds), 0))
        }
        8 => {
            let value = u64::from_be_bytes(payload.try_into().ok()?);
            let nanoseconds = (value >> 34) as u32;
            let seconds = (value & 0x3_ffff_ffff) as i64;
            (nanoseconds <= 999_999_999).then_some((seconds, nanoseconds))
        }
        12 => {
            let nanoseconds = u32::from_be_bytes(payload[..4].try_into().ok()?);
            let seconds = i64::from_be_bytes(payload[4..].try_into().ok()?);
            (nanoseconds <= 999_999_999).then_some((seconds, nanoseconds))
        }
        _ => None,
    }
}

fn span(start: usize, end: usize) -> Span {
    // Every offset this produces is at most the source byte length, and `scan`'s entry guard refuses sources beyond the
    // u32 span-offset space before the first item opens — so construction cannot overflow here.
    Span::from_usize(start, end)
}

fn data_contract() -> CodecError {
    jqf_codec_core::data_contract("MessagePack authoritative document construction")
}

/// The adjacent-value law pinned (transcribed from cbor's `the_adjacent_value_ruling_is_pinned`): the plain
/// `messagepack` format stays a single-document format — its registration declares NO
/// [`jqf_codec_core::RouteCapability::AdjacentValues`] route, so the SDK's sequence drive can never select it — and
/// the decode side refuses the opt-in with `RequirementMismatch` (pinned in `decode::tests`).
#[cfg(test)]
mod tests {
    use std::format;
    use std::string::String;

    use super::{ItemKind, Skeleton, scan};
    use crate::options::Dialect;
    use jqf_codec_core::CodecRunContext;

    fn scan_bytes(bytes: &[u8], dialect: Dialect) -> Result<Skeleton, String> {
        let mut resources = crate::test_support::resources();
        let source = jqf_source::ResolvedSource::new(
            jqf_source::SourceRef::new(jqf_source::SourceId::new(96), jqf_source::SourceKind::Input),
            "scan.test",
            bytes,
            0,
        );
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        scan(source, dialect, &mut run).map_err(|error| format!("{error:?}"))
    }

    /// One item per marker family: the scan names every family and rejects `0xc1` at head time (the grammar gate). Each
    /// family is its OWN top-level object — the scan reads exactly one and rejects trailing bytes.
    #[test]
    fn every_marker_family_scans() {
        let cases: &[(&str, &[u8])] = &[
            ("nil", &[0xc0]),
            ("false", &[0xc2]),
            ("true", &[0xc3]),
            ("positive fixint", &[0x01]),
            ("uint8", &[0xcc, 0x05]),
            ("uint16", &[0xcd, 0x00, 0x05]),
            ("uint32", &[0xce, 0x00, 0x00, 0x00, 0x05]),
            ("uint64", &[0xcf, 0, 0, 0, 0, 0, 0, 0, 5]),
            ("negative fixint", &[0xe0]),
            ("int8", &[0xd0, 0xfb]),
            ("int16", &[0xd1, 0xff, 0xfb]),
            ("int32", &[0xd2, 0xff, 0xff, 0xff, 0xfb]),
            ("int64", &[0xd3, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfb]),
            ("float32", &[0xca, 0x3f, 0x80, 0x00, 0x00]),
            ("float64", &[0xcb, 0x3f, 0xf0, 0, 0, 0, 0, 0, 0]),
            ("fixstr", &[0xa3, b'a', b'b', b'c']),
            ("str8", &[0xd9, 0x03, b'a', b'b', b'c']),
            ("str16", &[0xda, 0x00, 0x03, b'a', b'b', b'c']),
            ("str32", &[0xdb, 0, 0, 0, 3, b'a', b'b', b'c']),
            ("bin8", &[0xc4, 0x03, 1, 2, 3]),
            ("bin16", &[0xc5, 0x00, 0x03, 1, 2, 3]),
            ("bin32", &[0xc6, 0, 0, 0, 3, 1, 2, 3]),
            ("fixext1", &[0xd4, 0x01, 0xaa]),
            ("fixext2", &[0xd5, 0x02, 0xaa, 0xbb]),
            ("fixext4", &[0xd6, 0x03, 1, 2, 3, 4]),
            ("fixext8", &[0xd7, 0x04, 1, 2, 3, 4, 5, 6, 7, 8]),
            (
                "fixext16",
                &[0xd8, 0x05, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            ),
            ("ext8", &[0xc7, 0x02, 0x06, 0xaa, 0xbb]),
            ("ext16", &[0xc8, 0x00, 0x02, 0x07, 0xaa, 0xbb]),
            ("ext32", &[0xc9, 0, 0, 0, 2, 0x08, 0xaa, 0xbb]),
            ("timestamp 32", &[0xd6, 0xff, 0, 0, 0, 1]),
            ("timestamp 64", &[0xd7, 0xff, 0, 0, 0, 0, 0, 0, 0, 1]),
            ("timestamp 96", &[0xc7, 0x0c, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            ("empty fixarray", &[0x90]),
            ("empty fixmap", &[0x80]),
            ("fixarray [1]", &[0x91, 0x01]),
            ("fixmap {a:1}", &[0x81, 0xa1, b'a', 0x01]),
            ("array16", &[0xdc, 0x00, 0x01, 0x02]),
            ("array32", &[0xdd, 0, 0, 0, 1, 0x02]),
            ("map16", &[0xde, 0x00, 0x01, 0xa1, b'b', 0x02]),
            ("map32", &[0xdf, 0, 0, 0, 1, 0xa1, b'c', 0x03]),
        ];
        for (name, bytes) in cases.iter().copied() {
            let skeleton = scan_bytes(bytes, Dialect::Utf8).unwrap_or_else(|error| panic!("{name} must scan: {error}"));
            // Pre-order: the root is the FIRST item, children follow.
            assert_eq!(skeleton.root, 0, "{name}");
            // Every item carries an authored span.
            for item in &skeleton.items {
                assert!(
                    item.span.start() < item.span.end() || matches!(item.kind, ItemKind::Null),
                    "{name}: item spans name authored bytes"
                );
            }
        }
        // The scanned integer families carry the right values.
        let skeleton = scan_bytes(&[0xcf, 0, 0, 0, 0, 0, 0, 0, 5], Dialect::Utf8).unwrap();
        let ItemKind::Integer(value) = &skeleton.items[0].kind else {
            panic!("uint64 is an integer");
        };
        assert_eq!(value.to_i64(), Some(5));
        let skeleton = scan_bytes(&[0xe0], Dialect::Utf8).unwrap();
        let ItemKind::Integer(value) = &skeleton.items[0].kind else {
            panic!("negative fixint is an integer");
        };
        assert_eq!(value.to_i64(), Some(-32));
    }

    #[test]
    fn the_never_used_byte_is_rejected_at_head_time() {
        let error = scan_bytes(&[0xc1], Dialect::Utf8).expect_err("0xc1 rejects");
        assert!(error.contains("reserved-byte"), "{error}");
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let error = scan_bytes(&[0x01, 0x02], Dialect::Utf8).expect_err("trailing byte rejects");
        assert!(error.contains("trailing-bytes"), "{error}");
    }

    #[test]
    fn an_invalid_utf8_str_rejects_under_utf8_and_survives_under_wire() {
        let bytes = [0xa2, 0xff, 0xfe]; // str8(len=2) with invalid UTF-8
        let error = scan_bytes(&bytes, Dialect::Utf8).expect_err("invalid utf8 rejects");
        assert!(error.contains("invalid-utf8"), "{error}");
        // Under wire@1 the scan succeeds; the semantic build refuses.
        let skeleton = scan_bytes(&bytes, Dialect::Wire).expect("wire keeps the payload");
        let ItemKind::Str(payload) = &skeleton.items[skeleton.root].kind else {
            panic!("expected a str item");
        };
        assert_eq!(&bytes[payload.start() as usize..payload.end() as usize], &[0xff, 0xfe]);
    }

    #[test]
    fn a_truncated_container_is_eof() {
        // fixarray declares TWO items but carries one: EOF at the missing item.
        let error = scan_bytes(&[0x92, 0x01], Dialect::Utf8).expect_err("fixarray len 2 with one item");
        assert!(error.contains("eof"), "{error}");
        // array16 declares two items but carries one: EOF inside the second.
        let error = scan_bytes(&[0xdc, 0x00, 0x02, 0x01], Dialect::Utf8).expect_err("array16");
        assert!(error.contains("eof"), "{error}");
        // A COMPLETE array followed by another byte is trailing, not eof.
        let error = scan_bytes(&[0x91, 0x01, 0x02], Dialect::Utf8).expect_err("trailing");
        assert!(error.contains("trailing-bytes"), "{error}");
    }

    #[test]
    fn an_invalid_reserved_timestamp_payload_survives_the_scan() {
        // ext -1 with a 3-byte payload: structurally fine, semantically bad.
        let bytes = [0xc7, 0x03, 0xff, 0, 0, 0];
        let skeleton = scan_bytes(&bytes, Dialect::Utf8).expect("scan accepts the ext");
        let ItemKind::Ext { ty, .. } = &skeleton.items[skeleton.root].kind else {
            panic!("expected an ext item");
        };
        assert_eq!(ty, &-1);
    }

    #[test]
    fn the_timestamp_encodings_project() {
        use super::timestamp_from_payload;
        assert_eq!(timestamp_from_payload(&[0, 0, 0, 1]), Some((1, 0)));
        assert_eq!(timestamp_from_payload(&[0, 0, 0, 0, 0, 0, 0, 1]), Some((1, 0)));
        // 64-bit: nanoseconds << 34 | seconds.
        let mut payload = [0u8; 8];
        let value = (999_999_999u64 << 34) | 1;
        payload.copy_from_slice(&value.to_be_bytes());
        assert_eq!(timestamp_from_payload(&payload), Some((1, 999_999_999)));
        // 96-bit: u32 nanoseconds, i64 seconds.
        let mut payload = [0u8; 12];
        payload[..4].copy_from_slice(&500u32.to_be_bytes());
        payload[4..].copy_from_slice(&(-5i64).to_be_bytes());
        assert_eq!(timestamp_from_payload(&payload), Some((-5, 500)));
        // Invalid: nanoseconds out of range and wrong lengths.
        let mut payload = [0u8; 8];
        let value = (1_000_000_000u64 << 34) | 1;
        payload.copy_from_slice(&value.to_be_bytes());
        assert_eq!(timestamp_from_payload(&payload), None);
        assert_eq!(timestamp_from_payload(&[0, 0, 0]), None);
        assert_eq!(timestamp_from_payload(&[0; 16]), None);
    }
}
