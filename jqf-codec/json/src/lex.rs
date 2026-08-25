//! Stateless RFC 8259 byte-level lexing primitives; shared by the whole-document parser, the scoped validator, and the
//! lazy materializer.
//!
//! Everything here is a free function or constant over source bytes — no parser state of its own. The
//! [`JsonParseState`] machine, the scoped validator, and the lazy materializer all drive these same primitives, so most
//! accept/reject verdicts agree because the code is literally shared. The exception is the `\uXXXX`/surrogate-pair
//! classification, which the scoped validator re-implements by hand over already-lexed key text rather than driving
//! this module; that one family agrees by mirrored law plus lib.rs's mutation fence over both parsers, not by
//! construction.

use alloc::string::String;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::{DataError, DecimalText, DocumentCapacity, PreparedSemanticNode};
use jqf_resource::{ResourceContext, ResourceError, WorkAdmission};
use jqf_source::ResolvedSource;

use crate::error::data_contract;
use crate::parse::JsonParseState;
use crate::storage::{Builder, EscapeState, NumberLex, NumberState};

pub(crate) fn nonfinite_literal(
    source: ResolvedSource<'_>,
    offset: usize,
    sign: f64,
) -> Option<(PreparedSemanticNode, usize)> {
    let length = nonfinite_spelling_len(source, offset)?;
    let value = if length == 8 {
        f64::INFINITY
    } else {
        // `nan` (3) and `snan` (4) carry the fixed NaN bits; `inf` (3) the infinity payload.
        let rest = &source.bytes()[offset..offset + length];
        if rest.eq_ignore_ascii_case(b"nan") || rest.eq_ignore_ascii_case(b"snan") {
            f64::NAN
        } else {
            f64::INFINITY
        }
    };
    let payload = sign * value;
    let bits = if payload.is_nan() {
        JsonParseState::NONFINITE_NAN_BITS
    } else {
        payload.to_bits()
    };
    Some((
        PreparedSemanticNode::Float(jqf_data::Float::new(f64::from_bits(bits))),
        length,
    ))
}

/// The consumed byte length of a reference non-finite spelling at `offset` — `nan` (3), `inf` (3), or `infinity` (8),
/// case-insensitive — with the ordinary literal's value-boundary law, or `None` when the bytes are not a complete
/// non-finite token. Shared by the whole parser and the scoped validator, which must agree on what is a number.
pub(crate) fn nonfinite_spelling_len(source: ResolvedSource<'_>, offset: usize) -> Option<usize> {
    let bytes = source.bytes();
    let length = nonfinite_spelling_prefix_len(bytes, offset)?;
    // The same boundary law as the bare literals: `nanx`/`inf1` are one malformed token, rejected wholesale (the
    // grammar: `nanx` is a parse error).
    if let Some(&next) = bytes.get(offset + length)
        && !is_value_boundary(next)
    {
        return None;
    }
    Some(length)
}

/// The byte length of a complete non-finite spelling at `offset` — `snan` (4), `nan` (3), `inf` (3), or `infinity`
/// (8), case-insensitive — IGNORING the value-boundary law. `None` when the bytes are not a complete spelling. The
/// boundary law is applied separately by [`nonfinite_spelling_len`]; a caller that needs to label a boundary violation
/// at the offending byte (the bare literals' `nullx` law, which [`crate::parse::value_step`]'s non-finite arms mirror)
/// reads this instead.
pub(crate) fn nonfinite_spelling_prefix_len(bytes: &[u8], offset: usize) -> Option<usize> {
    let rest = bytes.get(offset.min(bytes.len())..)?;
    // `infinity` must be matched before its `inf` prefix; `snan` before nothing — it shares no prefix with the
    // others.
    if rest.len() >= 8 && rest[..8].eq_ignore_ascii_case(b"infinity") {
        Some(8)
    } else if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(b"snan") {
        Some(4)
    } else if rest.len() >= 3 && (rest[..3].eq_ignore_ascii_case(b"nan") || rest[..3].eq_ignore_ascii_case(b"inf")) {
        Some(3)
    } else {
        None
    }
}

/// Whether the bytes from `offset` run out in the MIDDLE of a reference non-finite spelling: what is present is a
/// nonempty proper prefix of one and the window ends there (`infi`, `sna`, `na`). Such a cut is an incomplete token,
/// not a wrong byte, so callers label the failure at the cut — the same law [`JsonParseState::literal_hoisted`]
/// applies to `nul` — and the streaming drive holds and refills instead of failing.
///
/// A COMPLETE spelling can also be a proper prefix of a longer one (`inf` of `infinity`), so this predicate alone would
/// hold those bytes forever; it is correct only because every caller checks completeness ([`nonfinite_spelling_len`] /
/// [`nonfinite_spelling_prefix_len`]) FIRST and consults truncation only on completeness failure. A new caller must
/// keep that order.
pub(crate) fn nonfinite_spelling_truncated(bytes: &[u8], offset: usize) -> bool {
    let Some(rest) = bytes.get(offset.min(bytes.len())..) else {
        return false;
    };
    !rest.is_empty()
        && NONFINITE_SPELLINGS
            .iter()
            .any(|spelling| rest.len() < spelling.len() && spelling[..rest.len()].eq_ignore_ascii_case(rest))
}

/// The reference non-finite spellings. Order is irrelevant: the only consumer tests each spelling's prefix
/// independently.
const NONFINITE_SPELLINGS: [&[u8]; 4] = [b"infinity", b"snan", b"nan", b"inf"];

/// The UTF-8 byte-order mark an input may carry before its first value (RFC 8259 §8.1). It is not part of any value.
pub(crate) const BYTE_ORDER_MARK: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Whether `bytes` is a nonempty PROPER prefix of the byte-order mark, and so ends mid-mark: the mark was split by a
/// read boundary rather than mistyped.
pub(crate) fn byte_order_mark_truncated(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.len() < BYTE_ORDER_MARK.len() && BYTE_ORDER_MARK.starts_with(bytes)
}

/// Conservative-low divisor turning source length into an initial node/occurrence arena reservation, for a source too
/// short to be reprojected (see [`SOURCE_BYTES_PER_SAMPLED_ARENA_SLOT`] for the reprojected case) and so sized to carry
/// the whole document. Derived from the fixture corpus's node densities (nodes per source byte): the sparsest shape
/// (escape-heavy) sits at ~0.025, the densest (nested ~0.087, wide ~0.077) higher. Choosing `1/40` (0.025) sizes the
/// slab at or below every shape's true density, so no input over-reserves (which would inflate peak on sparse
/// documents), while the densest lanes still start from a real slab that collapses their ~log2(n) from-empty doublings
/// to a couple — cutting arena-copy work without holding unused capacity. Publication compaction releases whatever
/// slack remains.
const SOURCE_BYTES_PER_ARENA_SLOT_ESTIMATE: usize = 40;

/// Conservative-low divisor for a source long enough that [`JsonParseState::reproject_capacity`] will resize its arenas
/// from observed density, which is the only case where the initial slab does not have to cover the whole document.
///
/// Such a slab only has to carry the parser as far as the density sample at `source_len /
/// CAPACITY_REPROJECT_SAMPLE_DENOMINATOR` bytes; from there the projected final count is reserved in one exact step and
/// the initial buffer is discarded. The densest JSON packing (`[1,1,...]`) approaches two source bytes per node from
/// below (`2 - 1/(n+1)`), so dividing by [`MIN_BYTES_PER_NODE`] is a hint-conservative ceiling, not an exact bound: it
/// can undercut the real maximum by about one node on tiny inputs, and the amortized growth this slab only exists to
/// reduce covers the difference. Even the densest shape reaches the density sample without one doubling, which is all
/// the initial slab was ever protecting against on these inputs. Sizing it from the sample rather than from the whole
/// document makes the buffer the reprojection then throws away 1.6x smaller (`1/64` against `1/40`), so a large
/// document's build both allocates and abandons that much less.
const SOURCE_BYTES_PER_SAMPLED_ARENA_SLOT: usize = CAPACITY_REPROJECT_SAMPLE_DENOMINATOR * MIN_BYTES_PER_NODE;

/// Initial arena slab reserved for one value decoded in adjacent mode, where `source_len` is the whole remaining stream
/// (every value shares one buffer via `open_at`) rather than this value's own extent. Sizing the estimate from the
/// remaining stream would reserve — then compact away — a slab proportional to the *rest of the file* for every
/// tiny NDJSON line, turning per-value overhead into O(remaining) work. A small fixed seed instead covers a typical
/// line's nodes without a from-empty doubling storm, and any value larger than the seed still grows by amortized
/// doubling (and, past the threshold, one reprojection).
const ADJACENT_VALUE_SEED_SLOTS: usize = 32;

/// Reserves a node/occurrence slab as an **optimization hint only**. This is never an admission requirement: if the
/// estimate cannot be afforded the parser degrades to amortized-from-empty growth, so a document that fits under exact
/// accounting is never rejected because the up-front estimate was too large. See [`reserve_hint`] for which failures
/// may be degraded and which propagate.
///
/// In whole-document mode the slab is source-length-proportional, from one of two divisors: a source that
/// [`JsonParseState::reproject_capacity`] will resize only needs to reach the density sample
/// ([`SOURCE_BYTES_PER_SAMPLED_ARENA_SLOT`]), while a shorter one is never reprojected and must be sized for the whole
/// document ([`SOURCE_BYTES_PER_ARENA_SLOT_ESTIMATE`]). In adjacent mode `source_len` is the whole remaining stream,
/// not this value's size, so a small value-scoped seed is used instead (see [`ADJACENT_VALUE_SEED_SLOTS`]).
pub(crate) fn reserve_estimate(
    builder: &mut Builder,
    source_len: usize,
    adjacent: bool,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    let slots = if adjacent {
        ADJACENT_VALUE_SEED_SLOTS
    } else if source_len >= CAPACITY_REPROJECT_MIN_SOURCE {
        source_len / SOURCE_BYTES_PER_SAMPLED_ARENA_SLOT
    } else {
        source_len / SOURCE_BYTES_PER_ARENA_SLOT_ESTIMATE
    };
    if slots == 0 {
        return Ok(());
    }
    reserve_hint(
        builder,
        DocumentCapacity {
            nodes: slots,
            occurrences: slots,
            ..DocumentCapacity::default()
        },
        resources,
    )
}

/// Applies one capacity reservation the parser WANTS but does not need.
///
/// Exactly two outcomes may be degraded to amortized-from-empty growth: the request's ledger refusing the projected
/// slab, and the allocator refusing it outright. Both say only "this much, up front, is too much", and the fallback
/// asks for less. Every other failure — an overflowed size, a violated accounting invariant, a host failure, a
/// rejected document — is a failure of the request itself, not of the estimate, and would be erased by a silent
/// degrade; those propagate.
///
/// The reservation is NOT atomic: the builder flushes its staged records first and then grows four tables in sequence,
/// so a refusal partway through leaves the earlier tables grown. That costs slack capacity, which publication
/// compaction releases, and never changes what the builder holds.
pub(crate) fn reserve_hint(
    builder: &mut Builder,
    additional: DocumentCapacity,
    resources: &ResourceContext<'_>,
) -> Result<(), CodecError> {
    match builder.try_reserve(additional, resources) {
        Ok(()) | Err(DataError::Resource(ResourceError::LimitExceeded { .. } | ResourceError::AllocationFailed)) => {
            Ok(())
        }
        Err(other) => Err(map_data(other)),
    }
}

/// Smallest source length that reprojects its table capacity from observed density. Below this, the conservative
/// one-shot estimate is kept unchanged, so every small document builds byte-for-byte as before — the reprojection
/// only governs the multi-megabyte inputs whose late doublings dominate peak memory.
pub(crate) const CAPACITY_REPROJECT_MIN_SOURCE: usize = 1 << 20;

/// Reprojection samples density once the consumed prefix reaches `source_len / DENOMINATOR`. A thirty-second is long
/// enough for the node/occurrence density of a uniform large document to stabilize — [`CAPACITY_REPROJECT_MIN_NODES`]
/// and the [`MIN_BYTES_PER_NODE`] ceiling guard the non-uniform case — and every doubling it happens before is one
/// the arenas never pay. Sampling at an eighth instead left roughly two further doublings, each holding the old and new
/// buffers live at once: on three 10 MB shapes, moving the sample from an eighth to a thirty-second cut peak RSS by
/// 5-8% (many-small-objects 291.4 -> 266.9 MB, escape 59.8 -> 56.6, catalog 171.5 -> 159.6) with the wall flat on every
/// lane.
pub(crate) const CAPACITY_REPROJECT_SAMPLE_DENOMINATOR: usize = 32;

/// Minimum sampled node count before a projection is trusted. Guards against projecting from a handful of nodes in a
/// tiny prefix of an otherwise large (whitespace- or single-scalar-dominated) source.
pub(crate) const CAPACITY_REPROJECT_MIN_NODES: usize = 1 << 10;

/// Headroom added to a projection so a slightly rising density late in the document does not force one more doubling:
/// reserve `projected + projected/8`.
pub(crate) const CAPACITY_REPROJECT_MARGIN_DENOMINATOR: usize = 8;

/// Hint-conservative divisor for strict JSON's node density. The densest node packing is an array of single-digit
/// numbers (`[1,1,1,...]`): `n` elements span `2n+1` bytes for `n+1` nodes, approaching two bytes per node from BELOW
/// (`2 - 1/(n+1)`), so `source_len / 2` is not an exact bound — it can undercut the true maximum by about one node on
/// tiny inputs. Every consumer applies it only to a reservation hint whose fallback is amortized growth, where that
/// slack is harmless; it is not a hard ceiling anywhere.
pub(crate) const MIN_BYTES_PER_NODE: usize = 2;

pub(crate) fn admit_bytes(resources: &mut ResourceContext<'_>, remaining: usize) -> Result<Option<usize>, CodecError> {
    match resources.admit_work_bytes(remaining)? {
        WorkAdmission::Granted(value) => Ok(Some(value)),
        WorkAdmission::Pending => Ok(None),
    }
}

/// The reserve pad [`reserve_window`] adds beyond the window's byte bound. It licenses two capacity draws the per-byte
/// bound alone does not cover: an escape RESUMED across the window boundary, whose completing window may consume as
/// little as one byte yet emit up to four (at most one can be in flight at entry), and [`push_chunk`]'s small-copy
/// tail, which needs `SMALL_COPY` spare bytes of arena at every push.
const RESERVE_PAD: usize = SMALL_COPY + 4;

/// Chunks at or below this length take [`push_chunk`]'s inline byte loop — the bound keeps the loop's trip count
/// small enough that the compiler emits straight-line stores instead of a `memcpy` libcall, which for the few-byte runs
/// between escapes costs more than the copy itself.
const SMALL_COPY: usize = 16;

/// Reserves the detached staged-text arena for one whole window's decoded output, fallibly — the refusal-not-abort
/// law is paid HERE, once per window, so every push inside the window is a raw capacity-backed tail write. The byte
/// bound is sound because decoded output never exceeds the source bytes that produce it (plain runs are 1:1; every
/// escape spelling is at least as long as its decoded scalar); [`RESERVE_PAD`] covers the two draws outside that bound.
/// The invariant every push relies on: `capacity - len >= remaining_window_input + SMALL_COPY`, maintained because each
/// push writes no more than the input it consumed (the one resumed escape's excess is the pad's other four bytes).
pub(crate) fn reserve_window(buf: &mut String, window_bytes: usize) -> Result<(), CodecError> {
    buf.try_reserve(window_bytes.saturating_add(RESERVE_PAD))
        .map_err(jqf_resource::ResourceError::from)?;
    Ok(())
}

/// Pushes one decoded chunk onto the detached staged-text arena as a raw tail write. Capacity is guaranteed by
/// [`reserve_window`] at the detach site, so no push in the window allocates (the debug assert pins the reserve bound's
/// soundness argument). Short chunks — the runs between escapes — copy through an inline byte loop; only a run past
/// [`SMALL_COPY`] pays the `memcpy` call, where it is the right tool.
#[expect(
    clippy::inline_always,
    reason = "the per-chunk write must fold into the string walk it serves"
)]
#[inline(always)]
pub(crate) fn push_chunk(buf: &mut String, chunk: &str) {
    debug_assert!(buf.capacity() - buf.len() >= chunk.len().max(SMALL_COPY));
    // SAFETY: `reserve_window` guaranteed capacity for every byte this window can decode, the bytes are UTF-8 (`chunk`
    // is a `str`), and the length settles to exactly the written extent. Both copy arms read only `chunk`'s own bytes.
    unsafe {
        let vec = buf.as_mut_vec();
        let len = vec.len();
        let dst = vec.as_mut_ptr().add(len);
        if chunk.len() <= SMALL_COPY {
            for (index, &byte) in chunk.as_bytes().iter().enumerate() {
                *dst.add(index) = byte;
            }
        } else {
            core::ptr::copy_nonoverlapping(chunk.as_ptr(), dst, chunk.len());
        }
        vec.set_len(len + chunk.len());
    }
}

/// Pushes one decoded scalar's UTF-8 bytes onto the detached staged-text arena; capacity-backed exactly as
/// [`push_chunk`]. The copy is a fixed four-byte store from the local encode buffer (reading all four is safe — it is
/// the whole local array), settled to the scalar's true length.
#[expect(
    clippy::inline_always,
    reason = "the per-escape write must fold into the string walk it serves"
)]
#[inline(always)]
pub(crate) fn push_char(buf: &mut String, value: char) {
    let mut encoded = [0u8; 4];
    let width = value.encode_utf8(&mut encoded).len();
    debug_assert!(buf.capacity() - buf.len() >= 4);
    // SAFETY: `reserve_window`'s pad keeps at least four spare bytes of arena at every push; the source is the whole
    // local array.
    unsafe {
        let vec = buf.as_mut_vec();
        let len = vec.len();
        core::ptr::copy_nonoverlapping(encoded.as_ptr(), vec.as_mut_ptr().add(len), 4);
        vec.set_len(len + width);
    }
}

/// The S4 duplicate-key probe fingerprint: FNV-1a 64 over the key's source bytes. While the session flag is still true
/// every key value has exactly one minimal spelling — plain characters and the eight escape forms (the seven short
/// escapes `\" \\ \b \f \n \r \t`, plus the six-byte `\u00XX` form every remaining C0 control and DEL takes; `/` has no
/// escape) — so byte equality IS value equality — the fingerprint probe is exact, and its collision direction (a
/// spurious match) only clears the flag, which merely declines the echo.
pub(crate) fn key_fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub(crate) fn is_delimiter(byte: u8) -> bool {
    crate::byte_scan::is_json_ws(byte) || matches!(byte, b',' | b']' | b'}')
}

/// Reports whether `byte` may immediately follow a completed bare-word value (a number or a `true`/`false`/`null`
/// literal). The reference terminates a bare word at whitespace, a structural delimiter, or the start of a
/// self-delimiting value (`"`, `[`, `{`) — so `1"a"`, `1[2]`, and `null"a"` are two adjacent values — but rejects a
/// bare word butted against another bare word (`1x`, `truex`, `1true`, `1-2`), because two words with no separator are
/// one malformed token. End of input is handled by the callers as its own accepting boundary.
pub(crate) fn is_value_boundary(byte: u8) -> bool {
    is_delimiter(byte) || matches!(byte, b'"' | b'[' | b'{')
}

#[expect(
    clippy::inline_always,
    reason = "the per-segment scan must fold into the string walk it serves"
)]
#[inline(always)]
pub(crate) fn plain_string_run_end(bytes: &[u8], start: usize, limit: usize) -> usize {
    start + crate::byte_scan::plain_string_prefix_len(&bytes[start..limit])
}

/// [`plain_string_run_end`] over the headed short-run scan: only for the fused escape burst, whose runs between escapes
/// are a few bytes long.
#[expect(
    clippy::inline_always,
    reason = "the per-segment scan must fold into the string walk it serves"
)]
#[inline(always)]
pub(crate) fn plain_string_run_end_short(bytes: &[u8], start: usize, limit: usize) -> usize {
    start + crate::byte_scan::plain_string_prefix_len_short(&bytes[start..limit])
}

/// One byte's position after the longest run of `bytes` (from `start`) that can sit verbatim inside a JSON5
/// single-quoted string: no `'`, no `\`, no C0 control. A `"` is ordinary content here (JSON5 single-quoted strings
/// carry unescaped double quotes), which is why this is a JSON5-specific scan and not the shared `PlainString` set's. A
/// scalar walk: single-quoted strings are the JSON5 rare path, and the shared SIMD kernel's budget is for the
/// double-quoted common case.
#[inline]
pub(crate) fn single_quoted_run_end(bytes: &[u8], start: usize, limit: usize) -> usize {
    let mut n = start;
    while n < limit && !matches!(bytes[n], b'\'' | b'\\' | 0x00..=0x1f) {
        n += 1;
    }
    n
}

#[inline]
pub(crate) fn string_content_run_end(bytes: &[u8], start: usize, limit: usize) -> usize {
    start + crate::byte_scan::string_content_prefix_len(&bytes[start..limit])
}

/// Hex-digit values, `0xff` for a non-digit: one load per digit instead of three range compares, on the `\uXXXX` path
/// that decodes four (or, for a surrogate pair, eight) digits per escape.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the loop index is bounded to 0..256, so the u8 casts are lossless"
)]
const HEX_LUT: [u8; 256] = {
    let mut table = [0xffu8; 256];
    let mut byte = 0usize;
    while byte < 256 {
        table[byte] = match byte as u8 {
            b'0'..=b'9' => byte as u8 - b'0',
            b'a'..=b'f' => byte as u8 - b'a' + 10,
            b'A'..=b'F' => byte as u8 - b'A' + 10,
            _ => 0xff,
        };
        byte += 1;
    }
    table
};

#[expect(
    clippy::inline_always,
    reason = "one table load; the call would cost more than the body"
)]
#[inline(always)]
pub(crate) fn hex(byte: u8) -> Option<u8> {
    let value = HEX_LUT[usize::from(byte)];
    if value == 0xff { None } else { Some(value) }
}

/// Accumulates every in-window hex digit of a `\uXXXX` run, up to four. Returns `(value, digits, end)`. `Err(at)` is
/// the first non-hex byte.
pub(crate) fn take_unicode_hex(
    bytes: &[u8],
    start: usize,
    limit: usize,
    seed: u16,
    have: u8,
) -> Result<(u16, u8, usize), usize> {
    let mut value = seed;
    let mut digits = have;
    let mut cursor = start;
    while digits < 4 && cursor < limit {
        let Some(digit) = hex(bytes[cursor]) else {
            return Err(cursor);
        };
        value = (value << 4) | u16::from(digit);
        digits += 1;
        cursor += 1;
    }
    Ok((value, digits, cursor))
}

pub(crate) fn apply_unicode_scalar(staged: &mut String, value: u16) -> Result<EscapeState, CodecError> {
    if (0xd800..=0xdbff).contains(&value) {
        Ok(EscapeState::LowBackslash { high: value })
    } else if (0xdc00..=0xdfff).contains(&value) {
        // A lone low surrogate decodes to U+FFFD, exactly as this encode law and the lenient fromjson reader do; only a
        // high surrogate not followed by a valid low one raises.
        push_char(staged, '\u{fffd}');
        Ok(EscapeState::Plain)
    } else {
        push_char(staged, char::from_u32(u32::from(value)).ok_or_else(data_contract)?);
        Ok(EscapeState::Plain)
    }
}

/// Converts a JSON5 hex digit run (the bytes after `0x`, no sign) to its canonical decimal text. Schoolbook
/// multiply-by-16 in little-endian decimal limbs, so arbitrarily long hex literals stay exact — up to
/// [`MAX_HEX_DIGITS`], which every caller enforces before converting: the multiply is quadratic in digit count, and
/// past the cap the literal is a decode refusal (`unsupported_hex_number`), not a rounding step.
pub(crate) fn hex_to_decimal(digits: &[u8]) -> alloc::string::String {
    let mut dec: alloc::vec::Vec<u8> = alloc::vec![0];
    for &digit in digits {
        let nibble = hex(digit).expect("validated hex digit");
        let mut carry = u32::from(nibble);
        for limb in &mut dec {
            let value = u32::from(*limb) * 16 + carry;
            *limb = (value % 10) as u8;
            carry = value / 10;
        }
        while carry > 0 {
            dec.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    let mut out = alloc::string::String::with_capacity(dec.len());
    for &digit in dec.iter().rev() {
        out.push(char::from(b'0' + digit));
    }
    out
}

/// The JSON5 hex digit ceiling. Exact hex-to-decimal conversion is quadratic in digit count (schoolbook multiply, one
/// decimal limb per digit), so an unbounded run turns one decode into seconds of CPU. The cap sits far past any real
/// exact integer use; a longer literal is refused at decode on every route (`unsupported_hex_number`), never rounded.
pub(crate) const MAX_HEX_DIGITS: usize = 4096;

/// Returns the total UTF-8 scalar width implied by a lead byte, or `None` when the byte cannot begin a scalar.
/// Continuation/overlong/surrogate validity is confirmed separately by `str::from_utf8` over the full sequence.
pub(crate) fn scalar_width_from_lead(byte: u8) -> Option<usize> {
    Some(match byte {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    })
}

pub(crate) fn number_lex_complete(lex: NumberLex, lenient: bool, json5: bool, digit_count: usize) -> bool {
    match lex {
        NumberLex::Zero | NumberLex::Integer | NumberLex::Fraction | NumberLex::Exponent => true,
        // Lenient / JSON5: `1.` is a complete number (the bare point with digits before it); `.`/`-.` alone carry no
        // digits and stay incomplete.
        NumberLex::FractionStart => (lenient || json5) && digit_count > 0,
        // A JSON5 hex integer is complete once it holds at least one hex digit after the `0x` (the leading `0` is digit
        // one; `0x` alone is incomplete).
        NumberLex::Hex => digit_count > 1,
        NumberLex::Start | NumberLex::IntegerStart | NumberLex::ExponentStart | NumberLex::ExponentSign => false,
    }
}

/// The reference implementation's exponent caps, asserted byte for byte by the compat corpus's lenient rows: a huge
/// NEGATIVE exponent underflows to `0E-1147483646` (scale +1,147,483,646) and a huge POSITIVE exponent on a zero
/// mantissa renders `0E+999999999` (scale −999,999,999). Both come from the decimal render path and are reproduced
/// exactly — it clamps to these fixed scales, and the exact decimal zero at that scale renders the same text.
pub(crate) const LENIENT_UNDERFLOW_SCALE: i64 = 1_147_483_646;
pub(crate) const LENIENT_OVERFLOW_ZERO_SCALE: i64 = -999_999_999;

/// Computes the canonical copy range and decimal scale for a completed number, or `Ok(None)` when the exponent places
/// the value outside the supported exact decimal range (the caller reports a decode-class `InvalidInput` refusal via
/// `unsupported_number` — never an encode-class error). This is the single authority shared by the full-decode
/// normalizer and the scoped validator so their accept/reject verdict on every number is identical by construction.
/// Under `lenient`, a leading-zero integer's plan skips the leading zero (the canonical text is the strict integer's);
/// the scale refusal is untouched here and the lenient caller clamps instead.
pub(crate) fn number_finish_plan(
    state: &NumberState,
    lenient: bool,
) -> Result<Option<(usize, usize, Option<i64>)>, CodecError> {
    if state.first_nonzero.is_some() {
        let last = state.last_nonzero.ok_or_else(data_contract)?;
        if state.has_fraction_or_exponent {
            if state.exponent_overflow {
                return Ok(None);
            }
            let Ok(magnitude) = i128::try_from(state.exponent) else {
                return Ok(None);
            };
            let exponent = if state.exponent_negative { -magnitude } else { magnitude };
            let trailing = i128::from(state.digit_count - 1 - last);
            let Some(scale) = i128::from(state.fraction_digits)
                .checked_sub(exponent)
                .and_then(|value| value.checked_sub(trailing))
            else {
                return Ok(None);
            };
            let Ok(scale) = i64::try_from(scale) else {
                return Ok(None);
            };
            Ok(Some((
                state.first_nonzero_offset.ok_or_else(data_contract)? as usize,
                state
                    .last_nonzero_offset
                    .and_then(|offset| offset.checked_add(1))
                    .ok_or_else(data_contract)? as usize,
                Some(scale),
            )))
        } else if lenient && state.lenient_spelling {
            // Lenient: a leading-zero integer (`01`, `007`) is not its own render — the coefficient starts at the
            // first nonzero digit and the leading zero is dropped, so the canonical text is the strict integer's. The
            // caller's `number_is_verbatim_source` sees the flag and stores the text instead of naming the span.
            Ok(Some((
                state.first_nonzero_offset.ok_or_else(data_contract)? as usize,
                state.cursor_usize(),
                None,
            )))
        } else {
            let start = state
                .start_usize()
                .checked_add(usize::from(state.negative))
                .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
            Ok(Some((start, state.cursor_usize(), None)))
        }
    } else if !state.has_fraction_or_exponent {
        // A plain zero integer (`0`, `-0`): no scale, the prefix is the whole span.
        Ok(Some((0, 0, None)))
    } else {
        // A zero mantissa clears the SAME out-of-range consultation as the nonzero arm above. An exact zero is
        // representable at any scale, so answering an in-range scale here silently renders plain `0` for spellings
        // whose documented render is one of the documented cap scientific forms (`0E+999999999` / `0E-1147483646`);
        // `None` routes them to the same callers as every other out-of-scale number — the decode refusal off lenient,
        // the lenient clamp's cap scales. The arithmetic below mirrors `number_render_plan` exactly, so both plans
        // agree on which inputs are in range.
        if state.exponent_overflow {
            return Ok(None);
        }
        let Ok(magnitude) = i128::try_from(state.exponent) else {
            return Ok(None);
        };
        let exponent = if state.exponent_negative { -magnitude } else { magnitude };
        let Some(scale) = i128::from(state.fraction_digits).checked_sub(exponent) else {
            return Ok(None);
        };
        let Ok(scale) = i64::try_from(scale) else {
            return Ok(None);
        };
        Ok(Some((0, 0, Some(scale))))
    }
}

/// Reports whether a completed number's canonical text is byte-identical to its own source span, so the document can
/// NAME those bytes instead of building a copy of them in the text arena.
///
/// The canonical text is otherwise assembled by `number_normalize_step` as `prefix ++ source[copy_start..copy_end]`
/// with every `.` dropped. Equality with `source[start..cursor]` therefore demands two things at once: the plan's copy
/// range must cover the whole span except for a sign the prefix puts back, and the span must contain no byte the copy
/// drops or reorders. Exactly one family of spellings satisfies both — an integer, meaning a number with neither a
/// fraction nor an exponent — and it is recognised here by the same fact that already means "verbatim" everywhere
/// else in this module: `has_fraction_or_exponent` is false, i.e. `number_finish_plan` returns no scale.
///
/// The argument, over the two shapes `number_finish_plan` can return for such a number:
///
/// - Nonzero magnitude. The plan is `(start + negative, cursor, None)`: the copy range is the entire digit run,
///   verbatim, and the prefix is precisely the `-` that range skipped. JSON's integer grammar is `-?(0|[1-9][0-9]*)`,
///   so the run holds nothing but digits — no `.` for the copy to drop, no leading `+`, and no leading zero, since
///   the lexer rejects `01` before it gets here (under leniency the lexer SETS `lenient_spelling` for a digit after a
///   leading zero, forfeiting verbatim status — which is exactly what the predicate reads — see
///   [`number_is_verbatim_source`]). Concatenating prefix and range reproduces `source[start..cursor]` exactly.
/// - Zero magnitude. The plan is the empty range and the prefix is the literal `"0"` or `"-0"`. That grammar leaves
///   only two spellings that can reach it without a fraction or exponent — `0` and `-0` — and the synthesized
///   prefix is, in both cases, the whole span again.
///
/// Every other form is excluded because its canonical text genuinely differs from the input, not out of caution:
///
/// - A fraction. The copy range never contains the `.`, so `1.5` canonicalizes to the coefficient `15` carrying scale
///   1.
/// - An exponent. `number_render_plan` ends the coefficient at the last digit BEFORE the exponent marker and folds the
///   exponent into the scale, so `1e3` canonicalizes to `1` with scale -3. This also disposes of the only place a `+`
///   may legally appear in a JSON number.
/// - The zero and trailing-zero trims (`0.100`, `0e5`, `-0e5`) exist only on the decimal side, where the coefficient
///   and scale are re-derived rather than quoted.
///
/// The predicate is deliberately a property of the LEXED FORM rather than of the bytes: it can only ever be true for a
/// spelling whose canonical rendering this module already documents as verbatim, so a future change to decimal
/// rendering cannot silently widen it.
pub(crate) const fn number_is_verbatim_source(state: &NumberState) -> bool {
    !state.has_fraction_or_exponent && !state.lenient_spelling
}

/// End offset of an in-window verbatim integer at `start`, or `None` when the token is not a source-verbatim integer
/// (fraction, exponent, leading zero, incomplete window, or a suffix the number machine must reject).
pub(crate) fn verbatim_integer_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start;
    if bytes.get(pos) == Some(&b'-') {
        pos += 1;
    }
    match bytes.get(pos).copied()? {
        b'0' => {
            pos += 1;
            if matches!(bytes.get(pos), Some(b'0'..=b'9')) {
                return None;
            }
        }
        b'1'..=b'9' => {
            pos += 1;
            while matches!(bytes.get(pos), Some(b'0'..=b'9')) {
                pos += 1;
            }
        }
        _ => return None,
    }
    match bytes.get(pos) {
        None => Some(pos),
        Some(byte) if is_value_boundary(*byte) => Some(pos),
        Some(_) => None,
    }
}

/// Whether one completed DECIMAL number's authored spelling is the codec's own render (the number canonicality, the
/// decimal arm of the disqualifier set).
///
/// The encoder renders a decimal from its coefficient and scale through [`DecimalText`], which switches to the
/// scientific form when the value leaves the fixed range (`0.0000001` renders `1E-7`, `0.0000000` renders `0E-7`);
/// within the fixed range the render keeps the authored coefficient digits and scale, so `0.1`, `1.50` and `1.000` ARE
/// their own render. The check compares the render's pieces against the authored bytes exactly — a spelling that is
/// not its own render clears the canonicality flag, which only ever costs the source echo, never the bytes.
///
/// The coefficient is ASSEMBLED, never sliced: the render plan's digit range runs from the first non-zero digit to the
/// last digit, which straddles the fraction point whenever the integer part is non-zero (`1.5` names the range `1.5`),
/// and an all-zero magnitude names the EMPTY range whose coefficient is the sign-carrying zero. The first shape
/// therefore copies the range with every `.` dropped, exactly as `number_normalize_step` copies it; the second
/// synthesizes the literal `0`/`-0`.
pub(crate) fn decimal_source_is_canonical(source: &[u8], state: &NumberState) -> bool {
    let Ok(Some((render_start, render_end, scale))) = number_render_plan(state) else {
        return false;
    };
    // The sign is not part of the render plan's digit range either: re-attach it first on a small stack buffer (a
    // coefficient long enough to overflow it declines — the safe direction, and no decoder-produced number gets near
    // it).
    let mut signed = [0u8; 64];
    let mut coefficient_len = usize::from(state.negative);
    if state.negative {
        signed[0] = b'-';
    }
    let mut digits_written = 0;
    for &byte in &source[render_start..render_end] {
        if byte == b'.' {
            continue;
        }
        if coefficient_len >= signed.len() {
            return false;
        }
        signed[coefficient_len] = byte;
        coefficient_len += 1;
        digits_written += 1;
    }
    if digits_written == 0 {
        // The all-zero magnitude's render plan names the empty range; its coefficient is the sign-carrying zero itself.
        if coefficient_len >= signed.len() {
            return false;
        }
        signed[coefficient_len] = b'0';
        coefficient_len += 1;
    }
    let Ok(coefficient) = core::str::from_utf8(&signed[..coefficient_len]) else {
        return false;
    };
    let Some(text) = DecimalText::new(coefficient, scale) else {
        return false;
    };
    let authored = &source[state.start_usize()..state.cursor_usize()];
    let mut offset = 0;
    for piece in text.pieces() {
        let Some(rest) = authored.get(offset..offset + piece.len()) else {
            return false;
        };
        if rest != piece {
            return false;
        }
        offset += piece.len();
    }
    offset == authored.len()
}

/// Whether one completed number's authored spelling survives the round trip a DEFERRED subtree's materialization makes,
/// and so whether the container holding it may be deferred at all.
///
/// The eager route publishes a number out of the DOCUMENT, which keeps the literal's trailing zeroes and the sign of a
/// zero — that is what makes `1.000`, `10.250`, `100.0` and `-0` byte-match the scientific-string form. A deferred
/// subtree is re-read and handed to its toucher as an owned [`Value`](jqf_data::Value), so the predicate is exactly the
/// complement of "survives the owned round trip".
///
/// The owned value model's canonicality risk: its decimal was NORMALIZED, so `1.000`, `10.250`, `100.0` and `-0` came
/// back as `1`, `10.25`, `1E+2` and `0`, which is why the predicate declined every trailing-zero and zero-magnitude
/// spelling whole-document. `Decimal::from_literal_parts` gave the owned `Number` the authored literal parts, so the
/// normalization no longer happens; the ONE class that still collapses is the sign of a zero magnitude —
/// `Integer::parse` normalizes `-0` to `0`, and the `[-0.0]` tostring intdiff row pins it.
///
/// The predicate is exact rather than cautious, and it was re-derived spelling-by-spelling against the documented
/// render:
///
/// - Negative zero in every spelling (`-0`, `-0.0`, `-0.00`, `-0e5`, `-0E+5`) declines: the owned round trip renders
///   `0`, `0.0`, `0E+5`.
/// - A verbatim integer is republished digit for digit, so every other spelling survives.
/// - A decimal with no render plan falls back to a canonical range that drops trailing zeroes, which is a third
///   spelling again; decline rather than reason about it.
/// - Every other decimal survives: trailing-zero coefficients (`100.0`, `1.000`, `10.50`, `1.2300e2`), zero-magnitude
///   decimals (`0.0`, `0.00`, `0e5`, `0e-5`), and negative non-zero magnitudes (`-0.5`, `-100.0`, `-10.50`).
pub(crate) fn number_survives_owned_round_trip(state: &NumberState) -> bool {
    // The sign of a zero magnitude is the one fact the owned value layer still drops. Everything else survives, so the
    // predicate is negative-zero-only.
    if state.negative && state.first_nonzero.is_none() {
        return false;
    }
    if number_is_verbatim_source(state) {
        return true;
    }
    // A decimal with no render plan falls back to a canonical range that drops trailing zeroes, which is a third
    // spelling again; decline rather than reason about it.
    number_render_plan(state).is_ok_and(|plan| plan.is_some())
}

/// Computes the *render* copy range and scale for a completed decimal number: the coefficient keeps its source
/// literal's trailing zeroes, and the scale is the decimal exponent (`fraction_digits - explicit_exponent`, no
/// trailing-zero adjustment). Rendering these reproduces the scientific-string scientific-string form byte for byte
/// (`1.000`, `10.250`, `0.00`, `0e5`).
///
/// Returns `Ok(None)` when the render scale overflows `i64`; the caller then falls back to [`number_finish_plan`]'s
/// canonical range, which drops the trailing zeroes but still round-trips the value. This never widens the set of
/// *accepted* numbers — acceptance stays owned solely by [`number_finish_plan`] so the full and scoped routes agree
/// by construction.
///
/// Only decimal numbers (`has_fraction_or_exponent`) reach this; integers keep their verbatim spelling through
/// [`number_finish_plan`].
pub(crate) fn number_render_plan(state: &NumberState) -> Result<Option<(usize, usize, i64)>, CodecError> {
    if state.exponent_overflow {
        return Ok(None);
    }
    let Ok(magnitude) = i128::try_from(state.exponent) else {
        return Ok(None);
    };
    let exponent = if state.exponent_negative { -magnitude } else { magnitude };
    let Some(scale) = i128::from(state.fraction_digits).checked_sub(exponent) else {
        return Ok(None);
    };
    let Ok(scale) = i64::try_from(scale) else {
        return Ok(None);
    };
    if state.first_nonzero.is_some() {
        Ok(Some((
            state.first_nonzero_offset.ok_or_else(data_contract)? as usize,
            state
                .last_digit_offset
                .and_then(|offset| offset.checked_add(1))
                .ok_or_else(data_contract)? as usize,
            scale,
        )))
    } else {
        // An all-zero magnitude: the coefficient is the sign-carrying `0`/`-0` prefix (empty copy range); only the
        // scale distinguishes `0.0` from `0e5`.
        Ok(Some((0, 0, scale)))
    }
}

#[expect(clippy::too_many_lines, reason = "kept as a single lexical state transition table")]
pub(crate) fn advance_number(
    state: &mut NumberState,
    byte: u8,
    lenient: bool,
    json5: bool,
) -> Result<bool, CodecError> {
    use NumberLex::{
        Exponent, ExponentSign, ExponentStart, Fraction, FractionStart, Hex, Integer, IntegerStart, Start, Zero,
    };
    match state.lex {
        Start => {
            if byte == b'-' {
                state.negative = true;
                state.lex = IntegerStart;
                return Ok(true);
            }
            // Lenient / JSON5: a leading `+` or a leading `.` starts a number. The `+` is skipped by the caller's span
            // start; the bare point needs at least one digit somewhere (`number_lex_complete` guards the empty shapes),
            // and both mark the spelling non-canonical.
            if (lenient || json5) && matches!(byte, b'+' | b'.') {
                state.lenient_spelling = true;
                if byte == b'+' {
                    state.lex = IntegerStart;
                } else {
                    state.has_fraction_or_exponent = true;
                    state.lex = FractionStart;
                }
                return Ok(true);
            }
            state.lex = IntegerStart;
            advance_number(state, byte, lenient, json5)
        }
        IntegerStart => match byte {
            b'0' => {
                push_digit(state, byte, false)?;
                state.lex = Zero;
                Ok(true)
            }
            b'1'..=b'9' => {
                push_digit(state, byte, false)?;
                state.lex = Integer;
                Ok(true)
            }
            // Lenient / JSON5: a point right after the sign (`-.5`).
            b'.' if lenient || json5 => {
                state.lenient_spelling = true;
                state.has_fraction_or_exponent = true;
                state.lex = FractionStart;
                Ok(true)
            }
            _ => Ok(false),
        },
        Zero => match byte {
            b'.' => {
                state.has_fraction_or_exponent = true;
                state.lex = FractionStart;
                Ok(true)
            }
            b'e' | b'E' => {
                state.has_fraction_or_exponent = true;
                state.lex = ExponentStart;
                Ok(true)
            }
            // The JSON5 hex arm: `0x`/`0X` opens a hex integer; the digit run that follows is the only new number
            // grammar JSON5 adds over the lenient spelling set.
            b'x' | b'X' if json5 => {
                state.lex = Hex;
                Ok(true)
            }
            // Lenient: a digit after a leading zero (`01`, `007`) is the reference's accepted spelling of the same
            // integer; strict rejects it by design (RFC 8259). The state continues as an ordinary integer run, and
            // `number_finish_plan` drops the leading zero.
            b'0'..=b'9' if lenient => {
                state.lenient_spelling = true;
                push_digit(state, byte, false)?;
                state.lex = Integer;
                Ok(true)
            }
            b'0'..=b'9' => Err(CodecError::new(CodecFailureKind::InvalidInput)),
            _ => Ok(false),
        },
        Integer => match byte {
            b'0'..=b'9' => {
                push_digit(state, byte, false)?;
                Ok(true)
            }
            b'.' => {
                state.has_fraction_or_exponent = true;
                state.lex = FractionStart;
                Ok(true)
            }
            b'e' | b'E' => {
                state.has_fraction_or_exponent = true;
                state.lex = ExponentStart;
                Ok(true)
            }
            _ => Ok(false),
        },
        FractionStart => match byte {
            b'0'..=b'9' => {
                push_digit(state, byte, true)?;
                state.lex = Fraction;
                Ok(true)
            }
            // Lenient / JSON5: an exponent right after a bare point (`1.e5`) is the accepted spelling; the strict
            // grammar requires a fraction digit first. The spelling still needs at least ONE digit somewhere before the
            // exponent — the same `digit_count > 0` guard `number_lex_complete` uses for completion — so
            // `.e5`/`-.e5` (no digit at all) stop here and reject as incomplete numbers instead of completing as zero.
            b'e' | b'E' if (lenient || json5) && state.digit_count > 0 => {
                state.lenient_spelling = true;
                state.lex = ExponentStart;
                Ok(true)
            }
            _ => Ok(false),
        },
        Fraction => match byte {
            b'0'..=b'9' => {
                push_digit(state, byte, true)?;
                Ok(true)
            }
            b'e' | b'E' => {
                state.lex = ExponentStart;
                Ok(true)
            }
            _ => Ok(false),
        },
        ExponentStart => match byte {
            b'+' => {
                state.lex = ExponentSign;
                Ok(true)
            }
            b'-' => {
                state.exponent_negative = true;
                state.lex = ExponentSign;
                Ok(true)
            }
            b'0'..=b'9' => {
                push_exponent(state, byte);
                state.lex = Exponent;
                Ok(true)
            }
            _ => Ok(false),
        },
        ExponentSign => match byte {
            b'0'..=b'9' => {
                push_exponent(state, byte);
                state.lex = Exponent;
                Ok(true)
            }
            _ => Ok(false),
        },
        Exponent => match byte {
            b'0'..=b'9' => {
                push_exponent(state, byte);
                Ok(true)
            }
            _ => Ok(false),
        },
        Hex => match byte {
            // Hex digits accumulate through the ordinary digit accounting (`push_digit` tracks offsets and counts,
            // which the hex materialization bypasses — the digits are re-read from the source span).
            b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => {
                push_digit(state, byte, false)?;
                Ok(true)
            }
            _ => Ok(false),
        },
    }
}

fn push_digit(state: &mut NumberState, byte: u8, fractional: bool) -> Result<(), CodecError> {
    let index = state.digit_count;
    state.digit_count = state
        .digit_count
        .checked_add(1)
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    state.last_digit_offset = Some(state.cursor);
    if byte != b'0' {
        if state.first_nonzero.is_none() {
            state.first_nonzero = Some(index);
            state.first_nonzero_offset = Some(state.cursor);
        }
        state.last_nonzero = Some(index);
        state.last_nonzero_offset = Some(state.cursor);
    }
    if fractional {
        state.fraction_digits = state
            .fraction_digits
            .checked_add(1)
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    }
    Ok(())
}

/// Folds one run of `0-9` bytes into `state`'s digit accounting in bulk and advances `state.cursor` past it, mirroring
/// [`push_digit`]'s per-digit effects exactly (digit count, last-digit offset, first/last non-zero indices and offsets,
/// fraction digit count). A run-skipped number is therefore indistinguishable from a byte-at-a-time one to every later
/// consumer. The caller re-enters [`advance_number`] at the run's first non-digit byte (or the slice end). Only the
/// `Integer`/`Fraction` lexemes are eligible: a digit in `Zero` is the leading-zero error, whose position must not
/// move, and no other lexeme is mid-digit-run.
pub(crate) fn consume_digit_run(state: &mut NumberState, digits: &[u8], fractional: bool) -> Result<usize, CodecError> {
    // One walk: digit-run length plus first/last non-zero, matching `push_digit`'s per-digit effects. Three separate
    // iterator scans walked the same bytes; the fused loop is indistinguishable to every later consumer.
    let mut run = 0;
    let mut first_rel = None;
    let mut last_rel = None;
    for (rel, &byte) in digits.iter().enumerate() {
        if !byte.is_ascii_digit() {
            break;
        }
        run = rel + 1;
        if byte != b'0' {
            if first_rel.is_none() {
                first_rel = Some(rel);
            }
            last_rel = Some(rel);
        }
    }
    if run == 0 {
        return Ok(0);
    }
    let run_u32 = crate::storage::offset_u32(run)?;
    let first_index = state.digit_count;
    state.digit_count = state
        .digit_count
        .checked_add(run_u32)
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    state.last_digit_offset = Some(
        state
            .cursor
            .checked_add(run_u32.saturating_sub(1))
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?,
    );
    if let Some(rel) = first_rel {
        let rel = crate::storage::offset_u32(rel)?;
        if state.first_nonzero.is_none() {
            state.first_nonzero = Some(
                first_index
                    .checked_add(rel)
                    .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?,
            );
            state.first_nonzero_offset = Some(
                state
                    .cursor
                    .checked_add(rel)
                    .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?,
            );
        }
        if let Some(last) = last_rel {
            let last = crate::storage::offset_u32(last)?;
            state.last_nonzero = Some(
                first_index
                    .checked_add(last)
                    .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?,
            );
            state.last_nonzero_offset = Some(
                state
                    .cursor
                    .checked_add(last)
                    .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?,
            );
        }
    }
    if fractional {
        state.fraction_digits = state
            .fraction_digits
            .checked_add(run_u32)
            .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    }
    state.cursor = state
        .cursor
        .checked_add(run_u32)
        .ok_or_else(|| CodecError::new(CodecFailureKind::Overflow))?;
    Ok(run)
}

fn push_exponent(state: &mut NumberState, byte: u8) {
    if !state.exponent_overflow {
        match state
            .exponent
            .checked_mul(10)
            .and_then(|value| value.checked_add(u128::from(byte - b'0')))
        {
            Some(value) => state.exponent = value,
            None => state.exponent_overflow = true,
        }
    }
}

pub(crate) fn map_data(error: DataError) -> CodecError {
    jqf_codec_core::map_data(error, "strict JSON authoritative document construction")
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec::Vec;

    use jqf_codec_core::{AccessInput, AccessOutcome, AccessSession as _, CodecFailureKind, CodecRunContext};
    use jqf_data::{DecimalText, Value};
    use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

    use super::{LENIENT_OVERFLOW_ZERO_SCALE, LENIENT_UNDERFLOW_SCALE, advance_number, number_finish_plan};
    use crate::storage::{JsonGrammar, NumberState, ParseMode};

    const LENIENT: JsonGrammar = JsonGrammar {
        comments: false,
        trailing_commas: false,
        lenient: true,
        json5: false,
    };
    const JSON5: JsonGrammar = JsonGrammar {
        comments: false,
        trailing_commas: false,
        lenient: false,
        json5: true,
    };

    fn source(bytes: &[u8]) -> ResolvedSource<'_> {
        ResolvedSource::new(
            SourceRef::new(SourceId::new(91), SourceKind::Input),
            "test.json",
            bytes,
            0,
        )
    }

    /// Decodes one whole document under `grammar` and materializes its root. This drives the real parser (the same
    /// machine the provider constructs), so every assertion below pins produced bytes, not plan guesses.
    fn decoded_root(grammar: JsonGrammar, bytes: &[u8]) -> Value {
        let mut resources = crate::test_support::resources();
        let mut state = crate::parse::JsonParseState::new(
            jqf_data::DiagnosticCoverage::NotRequested,
            jqf_data::BuilderCoverage::minimal_semantic(),
            ParseMode::Document,
        );
        state.set_grammar(grammar);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let result = state
            .decode(AccessInput::Source(source(bytes)), &mut run)
            .expect("decode succeeds");
        let AccessOutcome::FullDocument(product) = result.outcome() else {
            panic!("expected a full document");
        };
        let mut witness = crate::test_support::resources();
        product.document().materialize_root(&mut witness).expect("materialize")
    }

    /// Decodes under `grammar`, expects a decode-class refusal, and returns the structured diagnostic's namespaced code
    /// (`json.invalid-number`, `json.number-scale-out-of-range`, ...).
    fn refusal_code(grammar: JsonGrammar, bytes: &[u8]) -> String {
        let mut resources = crate::test_support::resources();
        let mut state = crate::parse::JsonParseState::new(
            jqf_data::DiagnosticCoverage::NotRequested,
            jqf_data::BuilderCoverage::minimal_semantic(),
            ParseMode::Document,
        );
        state.set_grammar(grammar);
        let mut run = CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        let error = state
            .decode(AccessInput::Source(source(bytes)), &mut run)
            .expect_err("decode must refuse");
        assert_eq!(
            error.kind(),
            CodecFailureKind::InvalidInput,
            "refusals stay decode-class"
        );
        let diagnostic = error.diagnostic().expect("structured diagnostic");
        alloc::format!("{}", diagnostic.code())
    }

    /// A materialized root decimal's `(coefficient, scale)`.
    fn decimal_parts(value: Value) -> (String, i64) {
        let Value::Number(number) = value else {
            panic!("expected a number");
        };
        let decimal = number.as_decimal().expect("decimal");
        (String::from(decimal.coefficient().as_str()), decimal.scale())
    }

    fn float_value(value: Value) -> f64 {
        let Value::Number(number) = value else {
            panic!("expected a number");
        };
        number.as_float().expect("float").get()
    }

    /// The exact render bytes the encoder produces from coefficient + scale.
    fn rendered(coefficient: &str, scale: i64) -> String {
        let pieces: Vec<u8> = DecimalText::new(coefficient, scale)
            .expect("renderable")
            .pieces()
            .concat();
        String::from_utf8(pieces).expect("rendered bytes are UTF-8")
    }

    // An exponent may follow a bare point only when a digit precedes it.

    #[test]
    fn bare_point_before_an_exponent_without_any_digit_rejects() {
        // No digit anywhere before `e`: the FractionStart transition is now refused, so no spelling completes as a
        // spurious zero any more. The refusal CLASS splits on whether a number token ever began: `-`/`+` start one
        // under the lenient grammars, so their refusal is the documented number class; a bare point cannot start a
        // value, so its refusal is the value-position class.
        assert_eq!(
            refusal_code(LENIENT, b".e5"),
            "json.expected-value",
            ".e5 under lenient"
        );
        assert_eq!(refusal_code(JSON5, b".e5"), "json.expected-value", ".e5 under json5");
        for spelling in ["-.e5", "+.e5"] {
            assert_eq!(
                refusal_code(LENIENT, spelling.as_bytes()),
                "json.invalid-number",
                "{spelling} under lenient"
            );
            assert_eq!(
                refusal_code(JSON5, spelling.as_bytes()),
                "json.invalid-number",
                "{spelling} under json5"
            );
        }
    }

    #[test]
    fn a_digit_before_the_exponent_keeps_the_point_spelling_accepted() {
        // `0.e5` carries its leading zero (digit_count == 1), exactly what the transition guard and the completion
        // guard both require.
        for grammar in [LENIENT, JSON5] {
            let (coefficient, scale) = decimal_parts(decoded_root(grammar, b"0.e5"));
            assert_eq!((coefficient.as_str(), scale), ("0", -5));
            assert_eq!(rendered(&coefficient, scale), "0E+5");
        }
        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, b"1.e5"));
        assert_eq!((coefficient.as_str(), scale), ("1", -5));
        // The gate adds no new refusals: `-.5` (no exponent) is untouched.
        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, b"-.5"));
        assert_eq!((coefficient.as_str(), scale), ("-5", 1));
    }

    #[test]
    fn strict_still_rejects_an_exponent_after_a_bare_point() {
        // Strictness is unchanged: the strict grammar never reached this arm (the `lenient || json5` half of the
        // guard), and `1.e5` refused there before this gate existed too.
        assert_eq!(refusal_code(JsonGrammar::STRICT, b"1.e5"), "json.invalid-number");
    }

    // A zero mantissa obeys the same out-of-range law as a nonzero one.

    #[test]
    fn zero_mantissa_out_of_scale_clamps_to_the_documented_caps_under_lenient() {
        // Scale past i64 in either direction: these silently rendered plain `0` before; now they land on the documented
        // cap scales like every other out-of-range shape.
        let huge_positive = alloc::format!("0e{}", "9".repeat(25));
        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, huge_positive.as_bytes()));
        assert_eq!(coefficient, "0");
        assert_eq!(scale, LENIENT_OVERFLOW_ZERO_SCALE);
        assert_eq!(rendered(&coefficient, scale), "0E+999999999");

        let huge_negative = alloc::format!("0e-{}", "9".repeat(25));
        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, huge_negative.as_bytes()));
        assert_eq!(coefficient, "0");
        assert_eq!(scale, LENIENT_UNDERFLOW_SCALE);
        assert_eq!(rendered(&coefficient, scale), "0E-1147483646");

        // Sign preserved through the clamp.
        let signed = alloc::format!("-0e-{}", "9".repeat(25));
        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, signed.as_bytes()));
        assert_eq!(coefficient, "-0");
        assert_eq!(scale, LENIENT_UNDERFLOW_SCALE);
        assert_eq!(rendered(&coefficient, scale), "-0E-1147483646");

        // The u128 exponent-accumulator overflow lands on the same positive cap.
        let accumulator_overflow = alloc::format!("0e{}", "9".repeat(64));
        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, accumulator_overflow.as_bytes()));
        assert_eq!(coefficient, "0");
        assert_eq!(scale, LENIENT_OVERFLOW_ZERO_SCALE);
    }

    #[test]
    fn documented_cap_spellings_render_their_own_bytes() {
        // The caps themselves sit inside the representable range, so they take the ordinary render path and echo
        // verbatim — produceable before and after the zero-arm fix alike.
        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, b"0E+999999999"));
        assert_eq!((coefficient.as_str(), scale), ("0", -999_999_999));
        assert_eq!(rendered(&coefficient, scale), "0E+999999999");

        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, b"0E-1147483646"));
        assert_eq!((coefficient.as_str(), scale), ("0", 1_147_483_646));
        assert_eq!(rendered(&coefficient, scale), "0E-1147483646");
    }

    #[test]
    fn mid_range_zero_exponents_keep_their_render_on_every_grammar() {
        for grammar in [JsonGrammar::STRICT, LENIENT] {
            let (coefficient, scale) = decimal_parts(decoded_root(grammar, b"0e5"));
            assert_eq!((coefficient.as_str(), scale), ("0", -5));
            assert_eq!(rendered(&coefficient, scale), "0E+5");
        }
    }

    #[test]
    #[allow(clippy::float_cmp, reason = "the clamp stores exactly INFINITY")]
    fn nonzero_mantissa_controls_clamp_per_direction_under_lenient() {
        // Positive direction: the DBL_MAX clamp stores the widest binary64 through a Float node (the renderer writes
        // the clamped finite text). Exact comparison is the point: the stored Float IS +INFINITY, not merely near it.
        let huge_positive_value = float_value(decoded_root(LENIENT, alloc::format!("1e{}", "9".repeat(25)).as_bytes()));
        assert_eq!(huge_positive_value, f64::INFINITY);

        // Negative direction: the underflow cap, mantissa notwithstanding.
        let huge_negative = alloc::format!("1e-{}", "9".repeat(25));
        let (coefficient, scale) = decimal_parts(decoded_root(LENIENT, huge_negative.as_bytes()));
        assert_eq!((coefficient.as_str(), scale), ("0", LENIENT_UNDERFLOW_SCALE));
        assert_eq!(rendered(&coefficient, scale), "0E-1147483646");
    }

    #[test]
    fn out_of_scale_zero_mantissa_refuses_off_lenient() {
        // New: strict refuses instead of silently answering plain `0`.
        let huge = alloc::format!("0e{}", "9".repeat(25));
        assert_eq!(
            refusal_code(JsonGrammar::STRICT, huge.as_bytes()),
            "json.number-scale-out-of-range"
        );
        // Pre-existing control: the same refusal for a nonzero mantissa.
        let nonzero = alloc::format!("1e{}", "9".repeat(21));
        assert_eq!(
            refusal_code(JsonGrammar::STRICT, nonzero.as_bytes()),
            "json.number-scale-out-of-range"
        );
    }

    #[test]
    fn the_zero_mantissa_plan_shares_the_nonzero_out_of_range_law() {
        let driven = |bytes: &[u8]| {
            let mut state = NumberState::start_at(0, false).expect("state");
            for &byte in bytes {
                assert!(
                    advance_number(&mut state, byte, true, false).expect("advance"),
                    "{bytes:?} fully consumed"
                );
            }
            state
        };

        // A plain zero integer keeps its unscaled plan.
        let state = driven(b"-0");
        assert_eq!(number_finish_plan(&state, true).expect("plan"), Some((0, 0, None)));

        // In range: the plan carries the REAL scale, matching the render plan.
        let state = driven(b"0e5");
        assert_eq!(number_finish_plan(&state, true).expect("plan"), Some((0, 0, Some(-5))));

        // Out of range: `None`, the same signal the nonzero arm returns.
        let mut state = driven(b"0e5");
        state.exponent_overflow = true;
        assert_eq!(number_finish_plan(&state, true).expect("plan"), None);
    }
}
