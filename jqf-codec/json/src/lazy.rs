//! Reading a deferred container span back into a value.
//!
//! The parse may leave a container subtree UNBUILT and commit a span of the validated source it occupies instead (see
//! [`crate::parse`]'s lazy frontier). This module is the JSON half of that mechanism, and ONLY the JSON half: the
//! reader that turns those bytes back into an owned value when, and only when, something touches the subtree.
//!
//! The other two thirds are deliberately elsewhere, so that a second format lowering to the same `Document`/`Value`
//! structures inherits them instead of reimplementing them: `jqf-data` owns the seam
//! ([`jqf_data::LazySpanMaterializer`]) and `jqf-codec-core` owns the frontier policy, the committed-span accounting,
//! and the failure mapping. Nothing in this file decides how deep to defer or how a refusal is reported — it only
//! knows how to read JSON text.
//!
//! The consumption shape is validate-everything-first with materialize-on- touch: the outer decode proves the whole
//! subtree valid up front, and this reader re-parses only the span a toucher actually reads — the same
//! re-parse-one-extent-out-of-the-sealed-source shape the exact-path route applies to its located selection,
//! generalized from "the one path the requirement named" to "any deferred child".
//!
//! # Why the nested parse is an owned run
//!
//! The span text is re-read in [`ParseMode::OwnedRun`]: every string is COPIED into the nested builder's arenas, so the
//! document it publishes borrows nothing from the outer source and the [`Value`] survives it. That also makes the
//! nested parse structurally unable to defer anything of its own — the frontier refuses any mode that does not retain
//! source spans — so materialization terminates.
//!
//! # Coverage
//!
//! A [`Value`] is built from MANDATORY semantics only: the semantic relationship arenas and the core/non-core tag
//! wrappers are always present, and topology, attached facts, and provenance are optional side data no materialization
//! reads. The nested parse therefore demands [`BuilderCoverage::minimal_semantic`] — the cheapest shape that produces
//! the same value, and the reason a touched subtree does not pay for arenas the toucher would throw away.

use jqf_codec_core::{CodecError, CodecRunContext, map_span_materialization_error};
use jqf_data::{BuilderCoverage, DataError, DiagnosticCoverage, LazySpanMaterializer, Value};
use jqf_resource::ResourceContext;
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

use crate::parse::{JsonParseState, OwnedRunPoll};
use crate::scoped::skip_ws;
use crate::storage::{JsonGrammar, ParseMode};

/// The credits one cooperative entry of the nested parse is granted.
///
/// This is the crate's standing quantum (`jqf-codec/json/src/lib.rs` drives its own sessions with the same number), so
/// a span materialization consumes the work meter in exactly the sized steps a top-level decode of the same bytes
/// would.
const SPAN_PARSE_CREDITS: u32 = 4_096;

/// Synthetic source identity for the re-read of a deferred subtree.
///
/// The span is valid by construction — the outer decode's validating scan already accepted these exact bytes — so
/// no diagnostic ever renders this identity or the label below. They exist because [`ResolvedSource`] is how bytes are
/// handed to the parser, not because a position is reportable.
const SPAN_SOURCE: SourceRef = SourceRef::new(SourceId::new(0), SourceKind::Input);

/// The JSON reader for deferred container spans.
pub(crate) struct JsonSpanMaterializer;

/// The one installed reader.
///
/// It is a unit type with no configuration: every span is read the same way, so a single `&'static` reference serves
/// every document the JSON decoder publishes.
pub(crate) static JSON_SPAN_MATERIALIZER: JsonSpanMaterializer = JsonSpanMaterializer;

impl LazySpanMaterializer for JsonSpanMaterializer {
    fn materialize_span(&self, text: &str, resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        materialize(text, session_grammar(resources), resources).map_err(|error| map_span_materialization_error(&error))
    }

    fn count_span(
        &self,
        text: &str,
        container: jqf_data::ContainerSpanKind,
        range: Option<jqf_data::SliceRange>,
        probe: &[jqf_data::CountStep],
        _resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_data::CountVerdict, DataError> {
        // The span is validated JSON (the decode's scan proved it one complete container text), so the walk needs no
        // error path for malformed input; a shape the count cannot prove DECLINES and the caller falls back to the
        // floor, never guessing.
        match container {
            jqf_data::ContainerSpanKind::Array => Ok(count_array_span(text, range, probe)),
            // An object span's member count needs the duplicate-key law (the reference's last-value-wins) over the raw
            // key text; the floor owns it. v1 declines object spans.
            jqf_data::ContainerSpanKind::Object => Ok(jqf_data::CountVerdict::Decline),
        }
    }

    fn count_span_filtered(
        &self,
        text: &str,
        container: jqf_data::ContainerSpanKind,
        range: Option<jqf_data::SliceRange>,
        filter: &jqf_data::CountFilter,
        _resources: &mut ResourceContext<'_>,
    ) -> Result<jqf_data::CountVerdict, DataError> {
        match container {
            jqf_data::ContainerSpanKind::Array => Ok(count_array_span_filtered(text, range, filter)),
            // An object span's member iteration needs the duplicate-key law over raw key text; v1 declines it to the
            // floor, exactly as the unfiltered leaf does.
            jqf_data::ContainerSpanKind::Object => Ok(jqf_data::CountVerdict::Decline),
        }
    }

    fn visit_span_elements(
        &self,
        text: &str,
        container: jqf_data::ContainerSpanKind,
        demand: &jqf_data::ElementDemand,
        resources: &mut ResourceContext<'_>,
        visit: &mut dyn FnMut(&jqf_data::Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
    ) -> Result<jqf_data::ElementVerdict, DataError> {
        // The element-iteration leaf: iterate the array span's elements in bounded BATCHES, re-parsing each batch's
        // bytes as one bracketed array (the old element-stream route's run amortization: one parse session per batch,
        // never one per element), navigating the demand's probe per built element, and NEVER building the whole
        // container's tree. An object span's member iteration needs the duplicate-key law over the raw key text; v1
        // declines it to the floor.
        match container {
            jqf_data::ContainerSpanKind::Array => visit_array_span(text, demand, resources, visit),
            jqf_data::ContainerSpanKind::Object => Ok(jqf_data::ElementVerdict::Decline),
        }
    }
}

/// Counts an ARRAY span's in-range top-level elements under a count probe: a byte scan that never builds a leaf.
/// `range` bounds the walk to `[start, end)` — the slice cuts the out-of-range elements before the count, so they are
/// never touched, and a container shorter than the range's start counts 0. The probe is v1-scoped: empty (every
/// in-range element counts) or one `ObjectKey` step (an object element with the key counts, a null element counts via
/// the null precedence, any other category declines). Deeper probes and index probes decline to the floor. The
/// materializer's grammar: the validating scan's grammar with the resource dial's leniency bit — the two MUST agree
/// or the re-parse could accept what the scan rejected.
fn session_grammar(resources: &ResourceContext<'_>) -> JsonGrammar {
    JsonGrammar {
        lenient: resources.decode_lenient(),
        ..JsonGrammar::STRICT
    }
}

/// Counts a span's top-level array elements without decoding payloads.
///
/// # Precondition: strict-grammar bytes
///
/// The bytes must be STRICT-grammar valid — this walk has no error path for a trailing comma, which a `]` directly
/// after a comma would count as a phantom extra element. Nothing here re-checks that, and none is needed: only strict
/// sessions bind [`JsonSpanMaterializer`] (their validating scan rejected every trailing comma before the span was
/// deferred), while a commented dialect binds a [`CommentedSpanMaterializer`], which does NOT override `count_span` —
/// it takes the trait default, materializing the span under its own grammar and counting the built value, so a trailing
/// comma or comment never reaches this byte scan at all.
fn count_array_span(
    text: &str,
    range: Option<jqf_data::SliceRange>,
    probe: &[jqf_data::CountStep],
) -> jqf_data::CountVerdict {
    let bytes = text.as_bytes();
    // The span starts with `[`; walk past it and the leading whitespace.
    let mut pos = skip_ws(bytes, 1);
    if pos >= bytes.len() || bytes[pos] == b']' {
        return jqf_data::CountVerdict::Count(0);
    }
    let (skip, limit) = span_limits(range);
    let mut index = 0u64;
    let mut total = 0u64;
    loop {
        let in_range = index >= skip && limit.is_none_or(|end| index < end);
        if in_range {
            let contribution = match probe {
                [] => {
                    pos = skip_value(bytes, pos);
                    1
                }
                [jqf_data::CountStep::ObjectKey(_)] => match bytes.get(pos).copied() {
                    // An object element contributes 1 (a final key step's whole domain: an absent member is null, still
                    // exactly one output); a null element contributes 1 via the null precedence. Any other category
                    // raises in the reference — decline and let the floor render it.
                    Some(b'{' | b'n') => {
                        pos = skip_value(bytes, pos);
                        1
                    }
                    _ => return jqf_data::CountVerdict::Decline,
                },
                // Deeper key paths and index probes: v1 declines (the leaf has no span-level walk for them; the floor
                // materializes and serves).
                _ => return jqf_data::CountVerdict::Decline,
            };
            total = total.saturating_add(contribution);
        } else {
            // An out-of-range element: the slice never touches it, so its category is irrelevant — skip it without a
            // probe check.
            pos = skip_value(bytes, pos);
        }
        index = index.saturating_add(1);
        if limit.is_some_and(|end| index >= end) {
            return jqf_data::CountVerdict::Count(total);
        }
        pos = skip_ws(bytes, pos);
        if pos >= bytes.len() {
            return jqf_data::CountVerdict::Decline;
        }
        match bytes[pos] {
            b',' => {
                pos = skip_ws(bytes, pos + 1);
                if pos >= bytes.len() {
                    return jqf_data::CountVerdict::Decline;
                }
            }
            b']' => return jqf_data::CountVerdict::Count(total),
            _ => return jqf_data::CountVerdict::Decline,
        }
    }
}

/// Counts an ARRAY span's in-range top-level elements under a collect-filter predicate: a byte scan that never builds a
/// leaf, the filtered twin of [`count_array_span`]. Each in-range element contributes 0 or 1 by
/// [`jqf_data::CountTest::answer`] over the tested member's span-scan classification; every other shape (a non-object
/// element, a member the closed law cannot rank) declines to the floor.
///
/// The element walk is the same validated-grammar scan as the unfiltered leaf. The per-element read reproduces the
/// reference laws exactly:
/// - LAST-VALUE-WINS: every top-level member is scanned and the last match supplies the value (the object build law);
/// - ESCAPED KEYS decode before comparison (`"st\u006fck"` IS `"stock"`);
/// - an ABSENT member reads null, and a null item reads null through every step (the null precedence).
///
/// A scratch buffer rides the walk so numeric members never allocate.
fn count_array_span_filtered(
    text: &str,
    range: Option<jqf_data::SliceRange>,
    filter: &jqf_data::CountFilter,
) -> jqf_data::CountVerdict {
    let bytes = text.as_bytes();
    // The span starts with `[`; walk past it and the leading whitespace.
    let mut pos = skip_ws(bytes, 1);
    if pos >= bytes.len() || bytes[pos] == b']' {
        return jqf_data::CountVerdict::Count(0);
    }
    let (skip, limit) = span_limits(range);
    // The recognizer admits single-Key filter paths only; anything wider is not expressible in this scan and declines.
    let [jqf_data::CountStep::ObjectKey(key)] = filter.path.as_slice() else {
        return jqf_data::CountVerdict::Decline;
    };
    let mut index = 0u64;
    let mut total = 0u64;
    let mut scratch = alloc::string::String::new();
    loop {
        let in_range = index >= skip && limit.is_none_or(|end| index < end);
        if in_range {
            let contribution = match bytes.get(pos).copied() {
                // An object element: scan its top-level members for the key.
                Some(b'{') => match object_member_contribution(bytes, pos, key.as_str(), &filter.test, &mut scratch) {
                    Some((contribution, end)) => {
                        pos = end;
                        contribution
                    }
                    None => return jqf_data::CountVerdict::Decline,
                },
                // A null item reads null through every step of the path. The EXACT spelling matters: the accepted
                // non-finite spellings (`nan`/`snan`, case-insensitive) share the leading `n` and decode to TRUTHY
                // numbers, so anything that is not literally `null` declines to the floor.
                Some(b'n') if bytes[pos..].starts_with(b"null") => {
                    match filter.test.answer(jqf_data::CountMember::Null) {
                        Some(truthy) => {
                            pos = skip_value(bytes, pos);
                            u64::from(truthy)
                        }
                        None => return jqf_data::CountVerdict::Decline,
                    }
                }
                // Every other category raises on a type mismatch — decline and let the floor render it.
                _ => return jqf_data::CountVerdict::Decline,
            };
            total = total.saturating_add(contribution);
        } else {
            // An out-of-range element: the slice never touches it, so its category is irrelevant — skip without a
            // test.
            pos = skip_value(bytes, pos);
        }
        index = index.saturating_add(1);
        if limit.is_some_and(|end| index >= end) {
            return jqf_data::CountVerdict::Count(total);
        }
        pos = skip_ws(bytes, pos);
        if pos >= bytes.len() {
            return jqf_data::CountVerdict::Decline;
        }
        match bytes[pos] {
            b',' => {
                pos = skip_ws(bytes, pos + 1);
                if pos >= bytes.len() {
                    return jqf_data::CountVerdict::Decline;
                }
            }
            b']' => return jqf_data::CountVerdict::Count(total),
            _ => return jqf_data::CountVerdict::Decline,
        }
    }
}

/// Scans one object element's TOP-LEVEL members for `target_key`, applies `test` to the last match's value (absent
/// reads null), and returns `(contribution, position one past the closing brace)`. `None` declines: a shape the closed
/// law cannot rank, or a member the scan cannot read.
///
/// Duplicate keys follow the build law — the LAST occurrence supplies the value — so the scan never stops at the
/// first match.
fn object_member_contribution(
    bytes: &[u8],
    object_start: usize,
    target_key: &str,
    test: &jqf_data::CountTest,
    scratch: &mut alloc::string::String,
) -> Option<(u64, usize)> {
    let mut cursor = skip_ws(bytes, object_start + 1);
    let mut matched_at: Option<usize> = None;
    loop {
        if cursor >= bytes.len() {
            return None;
        }
        match bytes[cursor] {
            b'}' => break,
            b'"' => {
                let (after_key, raw_key) = read_string_content(bytes, cursor)?;
                let is_match = decoded_is(raw_key, target_key);
                cursor = skip_ws(bytes, after_key);
                if cursor >= bytes.len() || bytes[cursor] != b':' {
                    return None;
                }
                cursor = skip_ws(bytes, cursor + 1);
                if cursor >= bytes.len() {
                    return None;
                }
                if is_match {
                    matched_at = Some(cursor);
                }
                cursor = skip_value(bytes, cursor);
            }
            _ => return None,
        }
        cursor = skip_ws(bytes, cursor);
        if cursor >= bytes.len() {
            return None;
        }
        match bytes[cursor] {
            b',' => cursor = skip_ws(bytes, cursor + 1),
            b'}' => break,
            _ => return None,
        }
    }
    let Some(at) = matched_at else {
        // An ABSENT member reads null in the reference (`"other"` here); that is a real answer (usually a 0
        // contribution), never a raise.
        let truthy = test.answer(jqf_data::CountMember::Null)?;
        return Some((u64::from(truthy), cursor + 1));
    };
    let member = classify_member(bytes, at, scratch)?;
    let truthy = test.answer(member)?;
    Some((u64::from(truthy), cursor + 1))
}

/// Whether one raw JSON string content decodes to exactly `expected`. The no-escape fast path is a byte compare;
/// escapes decode.
fn decoded_is(raw_content: &[u8], expected: &str) -> bool {
    if !raw_content.contains(&b'\\') {
        return raw_content == expected.as_bytes();
    }
    match unescape_string(raw_content) {
        Some(decoded) => decoded == expected,
        None => false,
    }
}

/// Decodes raw JSON string content (between the quotes) into owned text. `None` on any escape the grammar forbids or a
/// broken surrogate pair — unreachable on validated input, kept total.
fn unescape_string(raw_content: &[u8]) -> Option<alloc::string::String> {
    let mut out = alloc::string::String::with_capacity(raw_content.len());
    let mut cursor = 0;
    while cursor < raw_content.len() {
        if raw_content[cursor] != b'\\' {
            let run = jqf_codec_core::byte_scan::prefix_len::<jqf_codec_core::byte_scan::StringContent>(
                &raw_content[cursor..],
            );
            out.push_str(core::str::from_utf8(&raw_content[cursor..cursor + run]).ok()?);
            cursor += run;
            continue;
        }
        let escaped = *raw_content.get(cursor + 1)?;
        cursor += 2;
        match escaped {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let high = hex4(raw_content, cursor)?;
                cursor += 4;
                if (0xD800..0xDC00).contains(&high) {
                    // A high surrogate must pair with \uDC00..\uDFFF next.
                    if raw_content.get(cursor) != Some(&b'\\') || raw_content.get(cursor + 1) != Some(&b'u') {
                        return None;
                    }
                    let low = hex4(raw_content, cursor + 2)?;
                    if !(0xDC00..0xE000).contains(&low) {
                        return None;
                    }
                    cursor += 6;
                    let combined = 0x1_0000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                    out.push(char::from_u32(combined)?);
                } else if (0xDC00..0xE000).contains(&high) {
                    out.push('\u{fffd}');
                } else {
                    out.push(char::from_u32(high)?);
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Reads four hex digits at `start`, or `None`.
fn hex4(raw_content: &[u8], start: usize) -> Option<u32> {
    if start + 4 > raw_content.len() {
        return None;
    }
    let mut value = 0u32;
    for byte in &raw_content[start..start + 4] {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' => u32::from(byte - b'a') + 10,
            b'A'..=b'F' => u32::from(byte - b'A') + 10,
            _ => return None,
        };
        value = (value << 4) | digit;
    }
    Some(value)
}

/// Classifies the tested member at its first significant byte into the shared [`jqf_data::CountMember`] vocabulary.
/// Numeric members assemble their exact decimal parts into `scratch` (cleared per call), so the hot walk allocates
/// nothing.
fn classify_member<'a>(
    bytes: &'a [u8],
    at: usize,
    scratch: &'a mut alloc::string::String,
) -> Option<jqf_data::CountMember<'a>> {
    match bytes.get(at).copied()? {
        // Exact spelling only: `nan`/`snan` (case-insensitive) share the leading `n` and are truthy NUMBERS, never
        // null.
        b'n' if bytes[at..].starts_with(b"null") => Some(jqf_data::CountMember::Null),
        b't' => Some(jqf_data::CountMember::Bool(true)),
        b'f' => Some(jqf_data::CountMember::Bool(false)),
        b'"' => {
            let (_, raw) = read_string_content(bytes, at)?;
            if !raw.contains(&b'\\') {
                return Some(jqf_data::CountMember::Text(core::str::from_utf8(raw).ok()?));
            }
            let decoded = unescape_string(raw)?;
            *scratch = decoded;
            Some(jqf_data::CountMember::Text(scratch.as_str()))
        }
        b'-' | b'0'..=b'9' => {
            let run = jqf_codec_core::byte_scan::prefix_len::<jqf_codec_core::byte_scan::Delimiter>(&bytes[at..]);
            let literal = core::str::from_utf8(&bytes[at..at + run]).ok()?;
            let (negative, scale) = decimal_parts(literal)?;
            scratch.clear();
            for byte in literal.bytes() {
                if byte.is_ascii_digit() {
                    scratch.push(byte as char);
                }
            }
            Some(jqf_data::CountMember::Decimal {
                negative,
                digits: scratch.as_str(),
                scale,
            })
        }
        b'[' => Some(jqf_data::CountMember::Array),
        b'{' => Some(jqf_data::CountMember::Object),
        _ => None,
    }
}

/// The sign and scale of a validated JSON number literal: scale is the fraction's digit count minus the exponent. An
/// exponent whose magnitude leaves i64 (or any spelling the count cannot carry) declines.
fn decimal_parts(literal: &str) -> Option<(bool, i64)> {
    let unsigned = literal.strip_prefix('-').unwrap_or(literal);
    let negative = unsigned.len() != literal.len();
    let bytes = unsigned.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
        cursor += 1;
    }
    let mut fraction_digits = 0i64;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        fraction_digits = i64::try_from(cursor - start).ok()?;
    }
    let mut exponent: i128 = 0;
    if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
        cursor += 1;
        let negative_exponent = match bytes.get(cursor) {
            Some(b'+') => {
                cursor += 1;
                false
            }
            Some(b'-') => {
                cursor += 1;
                true
            }
            _ => false,
        };
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == start {
            return None;
        }
        for &byte in &bytes[start..cursor] {
            exponent = exponent * 10 + i128::from(byte - b'0');
            if exponent > 1_000_000_000_000 {
                return None;
            }
        }
        if negative_exponent {
            exponent = -exponent;
        }
    }
    if cursor != bytes.len() {
        return None;
    }
    Some((negative, fraction_digits - i64::try_from(exponent).ok()?))
}

/// One batch of array-span elements, bounded by element count and bytes so the batch re-parse cost stays amortized and
/// the retained batch stays a fraction of the container (the old element-stream route's run law).
const ELEMENT_BATCH_LEN: usize = 256;
/// The batch's byte budget; a batch stops growing past it regardless of element count.
const ELEMENT_BATCH_BYTES: usize = 64 * 1024;

/// The skip/limit reading of a demand range: the element ordinals the walk visits are `[skip, limit)` — `limit`
/// `None` is the container edge. The bounds are the normalized non-negative reading, so against the actual container
/// length this is a pure clamp.
fn span_limits(range: Option<jqf_data::SliceRange>) -> (u64, Option<u64>) {
    match range {
        None => (0, None),
        // The bounds are already non-negative here; the cast cannot lose anything (i64::MAX fits u64).
        Some((start, end)) => (
            start.unwrap_or(0).max(0).cast_unsigned(),
            end.map(|end| end.max(0).cast_unsigned()),
        ),
    }
}

/// The element-iteration leaf over an ARRAY span: iterate the span's top-level elements in bounded batches — each
/// batch re-parsed as one bracketed array (one parse session per batch, never one per element) — navigating the
/// demand's probe per built element, and never building the whole container's tree.
///
/// The probe is v1-scoped to what the first-significant-byte pre-pass can prove for a [`jqf_data::ElementRow::FanOut`]
/// demand: empty, `Length`, or a ONE-step `Key`/`Index` path (the element's own category is its first byte). Deeper
/// probes decline to the floor (the built-container path's full navigation serves them where a span exists; the floor
/// serves them everywhere else). A [`jqf_data::ElementRow::ReduceFold`] demand visits as it goes — nothing is
/// published until the fold completes — so its deeper probes are served (each batch element navigates the full
/// probe).
fn visit_array_span(
    text: &str,
    demand: &jqf_data::ElementDemand,
    resources: &mut ResourceContext<'_>,
    visit: &mut dyn FnMut(&Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
) -> Result<jqf_data::ElementVerdict, DataError> {
    let bytes = text.as_bytes();
    // The span starts with `[`; walk past it and the leading whitespace.
    let pos = skip_ws(bytes, 1);
    if pos >= bytes.len() || bytes[pos] == b']' {
        return Ok(jqf_data::ElementVerdict::Completed(0));
    }
    let (skip, limit) = span_limits(demand.range);
    // The FanOut pre-pass: every in-range element's probe must be provable BEFORE the first visit (the
    // visit-all-or-none contract). A one-step probe's provability is its element's first byte — a cheap scan over the
    // in-range elements. A DEEPER probe (the first byte cannot prove the deeper steps) falls back to a full
    // materialize-and-check pre-pass over the in-range batches. ReduceFold visits as it goes (the caller's fold state
    // is unpublished until completion), so it needs no pre-pass.
    if matches!(demand.row, jqf_data::ElementRow::FanOut) {
        match fan_out_first_byte_provable(&demand.probe) {
            Some(provable) => {
                let mut scan = pos;
                let mut index = 0u64;
                loop {
                    if index >= skip && limit.is_none_or(|end| index < end) {
                        let Some(&first) = bytes.get(scan) else {
                            return Ok(jqf_data::ElementVerdict::Decline);
                        };
                        if !provable(first) {
                            return Ok(jqf_data::ElementVerdict::Decline);
                        }
                    }
                    scan = skip_value(bytes, scan);
                    scan = skip_ws(bytes, scan);
                    index = index.saturating_add(1);
                    if limit.is_some_and(|end| index >= end) {
                        break;
                    }
                    if scan >= bytes.len() {
                        return Ok(jqf_data::ElementVerdict::Decline);
                    }
                    match bytes[scan] {
                        b',' => {
                            scan = skip_ws(bytes, scan + 1);
                            if scan >= bytes.len() {
                                return Ok(jqf_data::ElementVerdict::Decline);
                            }
                        }
                        b']' => break,
                        _ => return Ok(jqf_data::ElementVerdict::Decline),
                    }
                }
            }
            None => {
                // A deeper probe: the batch pass with a check-only visitor verifies every in-range element's full probe
                // before a byte is published. The materialized elements are deterministic, so the publish pass
                // re-navigates identically.
                match run_batches(bytes, pos, demand, resources, true, &mut |_, _| Ok(()))? {
                    jqf_data::ElementVerdict::Completed(_) => {}
                    jqf_data::ElementVerdict::Decline => {
                        return Ok(jqf_data::ElementVerdict::Decline);
                    }
                }
            }
        }
    }
    run_batches(bytes, pos, demand, resources, false, visit)
}

/// The batch pass over an array span: skip the out-of-range elements, scan the in-range ones into bounded batches,
/// re-parse each batch as one bracketed array (one parse session per batch, never one per element), navigate the
/// demand's probe per built element, and hand the value to `visit` — or, with `check_only`, verify every probe and
/// visit nothing (the deep-probe `FanOut` pre-pass). Returns the visited count.
fn run_batches(
    bytes: &[u8],
    mut pos: usize,
    demand: &jqf_data::ElementDemand,
    resources: &mut ResourceContext<'_>,
    check_only: bool,
    visit: &mut dyn FnMut(&Value, &mut ResourceContext<'_>) -> Result<(), DataError>,
) -> Result<jqf_data::ElementVerdict, DataError> {
    let (skip, limit) = span_limits(demand.range);
    let mut visited = 0u64;
    let mut index = 0u64;
    // The skip phase: the slice never touches the out-of-range elements, so they are pure byte skips. A container
    // shorter than the range's start yields an empty range.
    while index < skip {
        if pos >= bytes.len() {
            return Ok(jqf_data::ElementVerdict::Decline);
        }
        if bytes[pos] == b']' {
            return Ok(jqf_data::ElementVerdict::Completed(visited));
        }
        pos = skip_value(bytes, pos);
        pos = skip_ws(bytes, pos);
        index = index.saturating_add(1);
        if pos >= bytes.len() {
            return Ok(jqf_data::ElementVerdict::Decline);
        }
        if bytes[pos] == b',' {
            pos = skip_ws(bytes, pos + 1);
            if pos >= bytes.len() {
                return Ok(jqf_data::ElementVerdict::Decline);
            }
        } else if bytes[pos] != b']' {
            return Ok(jqf_data::ElementVerdict::Decline);
        }
    }
    let mut buffer: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    loop {
        if limit.is_some_and(|end| index >= end) {
            return Ok(jqf_data::ElementVerdict::Completed(visited));
        }
        buffer.clear();
        buffer.push(b'[');
        // The batch is capped by the range's remaining width, so a batch never overruns the limit.
        let remaining = limit.map_or(ELEMENT_BATCH_LEN, |end| {
            usize::try_from(end.saturating_sub(index))
                .unwrap_or(usize::MAX)
                .min(ELEMENT_BATCH_LEN)
        });
        let mut count = 0usize;
        while count < remaining && buffer.len() < ELEMENT_BATCH_BYTES {
            if pos >= bytes.len() {
                return Ok(jqf_data::ElementVerdict::Decline);
            }
            if bytes[pos] == b']' {
                break;
            }
            let element_end = skip_value(bytes, pos);
            buffer.extend_from_slice(&bytes[pos..element_end]);
            buffer.push(b',');
            count += 1;
            pos = skip_ws(bytes, element_end);
            if pos >= bytes.len() {
                return Ok(jqf_data::ElementVerdict::Decline);
            }
            if bytes[pos] == b',' {
                pos = skip_ws(bytes, pos + 1);
                if pos >= bytes.len() {
                    return Ok(jqf_data::ElementVerdict::Decline);
                }
            } else if bytes[pos] != b']' {
                return Ok(jqf_data::ElementVerdict::Decline);
            }
        }
        if count == 0 {
            return Ok(jqf_data::ElementVerdict::Completed(visited));
        }
        // Drop the trailing comma and close the batch.
        buffer.pop();
        buffer.push(b']');
        let batch_text = core::str::from_utf8(&buffer).map_err(|_| DataError::InvalidDocument)?;
        let batch_value = materialize(batch_text, session_grammar(resources), resources)
            .map_err(|error| map_span_materialization_error(&error))?;
        let Value::Array(batch) = batch_value.untagged() else {
            return Ok(jqf_data::ElementVerdict::Decline);
        };
        for element in batch {
            let Some(probe_value) = jqf_data::owned_probe_value(element, &demand.probe) else {
                return Ok(jqf_data::ElementVerdict::Decline);
            };
            if !check_only {
                visit(&probe_value, resources)?;
            }
            visited = visited.saturating_add(1);
        }
        index = index.saturating_add(count as u64);
    }
}

/// The `FanOut` probe's per-element first-byte provability predicate, or `None` for a probe the byte pre-pass cannot
/// prove (a `Key`/`Index` path of MORE than one step: the first byte decides only the element's own category, not the
/// categories a deeper step would address, so a mid-publish decline could leave a published prefix a floor rerun would
/// duplicate). A `null` element is total over every probe; a container element is total over a first-step domain
/// matching its own kind.
fn fan_out_first_byte_provable(probe: &jqf_data::ElementProbe) -> Option<fn(u8) -> bool> {
    match probe {
        jqf_data::ElementProbe::Path(path) => match path.as_slice() {
            // Empty: every element is the probe value.
            [] => Some(|_| true),
            [jqf_data::CountStep::ObjectKey(_)] => Some(|byte| matches!(byte, b'{' | b'n')),
            [jqf_data::CountStep::ArrayIndex(_)] => Some(|byte| matches!(byte, b'[' | b'n')),
            // A deeper path: the first byte cannot prove the deeper steps.
            _ => None,
        },
        jqf_data::ElementProbe::Length => Some(|byte| matches!(byte, b'[' | b'{' | b'n')),
    }
}

/// Reads one string's raw content (between the opening and closing quotes) and the position one past the closing quote.
/// `None` when the token is not a well-formed string (unreachable on validated bytes; kept total).
fn read_string_content(bytes: &[u8], pos: usize) -> Option<(usize, &[u8])> {
    if bytes.get(pos) != Some(&b'"') {
        return None;
    }
    let mut cursor = pos + 1;
    loop {
        let run = jqf_codec_core::byte_scan::prefix_len::<jqf_codec_core::byte_scan::StringContent>(&bytes[cursor..]);
        cursor += run;
        match bytes.get(cursor).copied() {
            Some(b'"') => {
                return Some((cursor + 1, &bytes[pos + 1..cursor]));
            }
            Some(0x7f) => {
                // DEL is content; the stop set names it so the decoder's canonicality probe does not memchr every run.
                cursor += 1;
            }
            Some(b'\\') => {
                // An escape: skip the backslash and its one escaped byte (or a `\u` sequence's six bytes).
                if bytes.get(cursor + 1) == Some(&b'u') {
                    cursor += 6;
                } else {
                    cursor += 2;
                }
            }
            _ => return None,
        }
    }
}

/// Skips one complete JSON value starting at `pos` (its first significant byte), returning the position one past its
/// last byte.
fn skip_value(bytes: &[u8], pos: usize) -> usize {
    match bytes.get(pos).copied() {
        Some(b'"') => match read_string_content(bytes, pos) {
            Some((end, _)) => end,
            None => pos,
        },
        Some(b'{' | b'[') => {
            // The span is validated JSON, so brackets balance: every opener increments and every closer decrements —
            // never only the matching close, which would miscount a nested sibling-bracket container (`[1,{"a":[]}]`
            // needs the inner `]` to decrement).
            let mut depth = 0usize;
            let mut cursor = pos;
            loop {
                if cursor >= bytes.len() {
                    return cursor;
                }
                match bytes[cursor] {
                    b'"' => match read_string_content(bytes, cursor) {
                        Some((end, _)) => {
                            cursor = end;
                        }
                        None => return cursor,
                    },
                    b'[' | b'{' => {
                        depth += 1;
                        cursor += 1;
                    }
                    b']' | b'}' => {
                        depth = depth.saturating_sub(1);
                        cursor += 1;
                        if depth == 0 {
                            return cursor;
                        }
                    }
                    _ => cursor += 1,
                }
            }
        }
        // A bare word: true / false / null / a number. Skip to its delimiter.
        _ => {
            let run = jqf_codec_core::byte_scan::prefix_len::<jqf_codec_core::byte_scan::Delimiter>(&bytes[pos..]);
            pos + run
        }
    }
}

/// JSONC/JSON5 deferred-span reader: the STRICT materializer rejects `//` inside a deferred container. Each dialect
/// holds its trailing-comma / JSON5 grammar; comments are always trivia here. Count and element walks use the trait
/// default (materialize, then walk the owned value) so a comment inside the span cannot take the STRICT byte skip.
pub(crate) struct CommentedSpanMaterializer {
    trailing_commas: bool,
    json5: bool,
}

/// JSONC `jsonc.trailing@1` (comments and trailing commas).
pub(crate) static JSONC_TRAILING_SPAN_MATERIALIZER: CommentedSpanMaterializer = CommentedSpanMaterializer {
    trailing_commas: true,
    json5: false,
};
/// JSONC `jsonc.default@1` (comments, strict comma law).
pub(crate) static JSONC_DEFAULT_SPAN_MATERIALIZER: CommentedSpanMaterializer = CommentedSpanMaterializer {
    trailing_commas: false,
    json5: false,
};
/// JSON5 document grammar (comments, trailing commas, JSON5 syntax).
pub(crate) static JSON5_SPAN_MATERIALIZER: CommentedSpanMaterializer = CommentedSpanMaterializer {
    trailing_commas: true,
    json5: true,
};

impl LazySpanMaterializer for CommentedSpanMaterializer {
    fn materialize_span(&self, text: &str, resources: &mut ResourceContext<'_>) -> Result<Value, DataError> {
        materialize(
            text,
            JsonGrammar {
                comments: true,
                trailing_commas: self.trailing_commas,
                lenient: resources.decode_lenient(),
                json5: self.json5,
            },
            resources,
        )
        .map_err(|error| map_span_materialization_error(&error))
    }
}

fn materialize(text: &str, grammar: JsonGrammar, resources: &mut ResourceContext<'_>) -> Result<Value, CodecError> {
    let source = ResolvedSource::new(SPAN_SOURCE, "container-span", text.as_bytes(), 0);
    let mut state = JsonParseState::new(
        DiagnosticCoverage::NotRequested,
        BuilderCoverage::minimal_semantic(),
        ParseMode::OwnedRun,
    );
    // The span materializer must accept exactly what the validating scan accepted under the resource dial: the lazy
    // route validates leniently and then re-materializes the deferred span, so the materializer carries the same
    // grammar.
    state.set_grammar(grammar);
    // The span is a `&str`; UTF-8 is already proved.
    state.set_utf8_proved();
    let product = loop {
        let mut run = CodecRunContext::new(resources);
        match state.poll_owned(source, &mut run)? {
            OwnedRunPoll::Ready(product) => break product,
            OwnedRunPoll::Pending => {
                // The control check inside the replenish is what keeps a large subtree cancellable and
                // deadline-bounded: the loop cannot run past a cancellation, it can only run without yielding to the
                // caller's scheduler between entries.
                resources.try_begin_next_cooperative_entry(SPAN_PARSE_CREDITS)?;
            }
        }
    };
    let value = product
        .document()
        .materialize_root(resources)
        .map_err(crate::lex::map_data)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_data::CountStep;

    fn count(text: &str, probe: &[CountStep]) -> jqf_data::CountVerdict {
        count_array_span(text, None, probe)
    }

    fn key(name: &str) -> CountStep {
        CountStep::ObjectKey(alloc::string::String::from(name))
    }

    #[test]
    fn empty_and_simple_arrays_count_every_element() {
        assert_eq!(count("[]", &[]), jqf_data::CountVerdict::Count(0));
        assert_eq!(count("[ ]", &[]), jqf_data::CountVerdict::Count(0));
        assert_eq!(count("[1,2,3]", &[]), jqf_data::CountVerdict::Count(3));
        assert_eq!(
            count(r#"["a", true, null, [1, 2], {"x": 1}]"#, &[]),
            jqf_data::CountVerdict::Count(5)
        );
        // Nested containers and strings with delimiters do not split elements.
        assert_eq!(
            count(r#"[{"a":[1,2,3]},"x,y",[{"z":"]"}]]"#, &[]),
            jqf_data::CountVerdict::Count(3)
        );
    }

    #[test]
    fn one_key_probe_counts_the_whole_object_domain_and_null() {
        let probe = [key("name")];
        // Every object element contributes 1 (an absent member is the reference's null — still exactly one output).
        assert_eq!(
            count(r#"[{"name":1},{"x":2},{"name":3}]"#, &probe),
            jqf_data::CountVerdict::Count(3)
        );
        // Null elements count via the null precedence.
        assert_eq!(count(r#"[{"name":1}, null]"#, &probe), jqf_data::CountVerdict::Count(2));
        // A non-object element raises on a type mismatch; decline.
        assert_eq!(count(r#"[{"name":1}, 5]"#, &probe), jqf_data::CountVerdict::Decline);
        assert_eq!(count(r#"[{"name":1}, "s"]"#, &probe), jqf_data::CountVerdict::Decline);
        assert_eq!(count(r#"[{"name":1}, [1]]"#, &probe), jqf_data::CountVerdict::Decline);
    }

    #[test]
    fn index_and_deep_probes_decline() {
        assert_eq!(
            count("[1,2]", &[CountStep::ArrayIndex(0)]),
            jqf_data::CountVerdict::Decline
        );
        assert_eq!(count("[1,2]", &[key("a"), key("b")]), jqf_data::CountVerdict::Decline);
    }

    #[test]
    fn ranges_count_only_the_in_range_elements() {
        // `[a:b]` bounds the count to the in-range elements.
        assert_eq!(
            count_span_limited("[1,2,3,4,5]", Some((Some(1), Some(4))), &[]),
            jqf_data::CountVerdict::Count(3)
        );
        // An open end runs to the container edge.
        assert_eq!(
            count_span_limited("[1,2,3]", Some((Some(1), None)), &[]),
            jqf_data::CountVerdict::Count(2)
        );
        // A start past the end is zero.
        assert_eq!(
            count_span_limited("[1,2,3]", Some((Some(5), None)), &[]),
            jqf_data::CountVerdict::Count(0)
        );
        // The one-key probe applies only to the in-range elements: an out-of-range scalar does not decline.
        assert_eq!(
            count_span_limited(
                "[5, {\"name\": 1}, {\"name\": 2}]",
                Some((Some(1), None)),
                &[key("name")]
            ),
            jqf_data::CountVerdict::Count(2)
        );
        // An in-range foreign category still declines (the reference raises).
        assert_eq!(
            count_span_limited("[{\"name\": 1}, 5]", Some((Some(1), None)), &[key("name")]),
            jqf_data::CountVerdict::Decline
        );
    }

    fn count_span_limited(
        text: &str,
        range: Option<(Option<i64>, Option<i64>)>,
        probe: &[CountStep],
    ) -> jqf_data::CountVerdict {
        count_array_span(text, range, probe)
    }

    #[test]
    fn a_comment_inside_a_deferred_span_does_not_take_the_strict_materializer() {
        let text = "[1, // c\n2, 3]";
        let mut resources = crate::test_support::resources();
        assert!(
            JSON_SPAN_MATERIALIZER.materialize_span(text, &mut resources).is_err(),
            "STRICT must refuse a // inside the span"
        );
        let value = JSONC_TRAILING_SPAN_MATERIALIZER
            .materialize_span(text, &mut resources)
            .expect("JSONC grammar must re-read a comment-bearing span");
        let Value::Array(items) = value else {
            panic!("expected array, got {value:?}")
        };
        assert_eq!(items.len(), 3);
    }
}

#[cfg(test)]
mod count_filter_tests {
    //! The collect-filter byte scan: per-element 0-or-1 over the closed predicate vocabulary, decline-never-wrong on
    //! every shape it cannot rank. These pin the scan law directly; the CLI corpus pins the end-to-end bytes against
    //! the encode law.

    use super::*;
    use crate::lazy::JsonSpanMaterializer;

    fn scan_element(text: &str, key: &str, predicate: jqf_data::CountTest) -> jqf_data::CountVerdict {
        let materializer = JsonSpanMaterializer;
        let filter = jqf_data::CountFilter {
            path: alloc::vec![jqf_data::CountStep::ObjectKey(alloc::string::String::from(key,))],
            test: predicate,
        };
        let mut resources = crate::test_support::resources();
        materializer
            .count_span_filtered(text, jqf_data::ContainerSpanKind::Array, None, &filter, &mut resources)
            .expect("declines, never errors")
    }

    fn greater_zero() -> jqf_data::CountTest {
        jqf_data::CountTest::Compare {
            op: jqf_data::CountCompare::Greater,
            rhs: jqf_data::CountLiteral::Decimal {
                negative: false,
                digits: alloc::string::String::from("0"),
                scale: 0,
            },
        }
    }

    #[test]
    fn counts_truthy_comparisons_per_element() {
        let text = r#"[{"stock":5},{"stock":-1},{"stock":0},{"other":9},{"stock":null},null]"#;
        assert_eq!(
            scan_element(text, "stock", greater_zero()),
            jqf_data::CountVerdict::Count(1)
        );
    }

    #[test]
    fn duplicate_keys_follow_last_value_wins() {
        let text = r#"[{"stock":1,"stock":-5},{"a":2,"stock":3,"stock":0}]"#;
        // First element: last value -5 -> excluded. Second: last value 0 -> excluded.
        assert_eq!(
            scan_element(text, "stock", greater_zero()),
            jqf_data::CountVerdict::Count(0)
        );
    }

    #[test]
    fn escaped_key_spellings_match_the_decoded_key() {
        let text = r#"[{"st\u006fck":7}]"#;
        assert_eq!(
            scan_element(text, "stock", greater_zero()),
            jqf_data::CountVerdict::Count(1)
        );
    }

    #[test]
    fn cross_band_and_exact_decimal_laws_match_the_engine() {
        // Cross-band rank: string/array/object outrank numbers; -0 == 0 so `-0 > 0` is false; 1e999 is the exact
        // decimal (true), never an overflow.
        let text = r#"[{"stock":"many"},{"stock":1e999},{"stock":-0},{"stock":[1]},{"stock":{}}]"#;
        assert_eq!(
            scan_element(text, "stock", greater_zero()),
            jqf_data::CountVerdict::Count(4)
        );
    }

    #[test]
    fn nan_spellings_decline_to_the_floor() {
        // The non-finite spellings share the leading `n` with `null` but are TRUTHY numbers; the scan refuses to guess
        // and declines.
        let text = r#"[{"stock":nan},{"stock":NaN},{"stock":snan},{"stock":null}]"#;
        assert_eq!(
            scan_element(text, "stock", greater_zero()),
            jqf_data::CountVerdict::Decline
        );
    }

    #[test]
    fn raising_elements_decline_whole_answer() {
        // `.stock` over a non-object raises on a type mismatch; the scan must not answer a count the floor would have
        // failed.
        for text in [
            r#"[{"stock":1},[2]]"#,
            r#"[{"stock":1},"s"]"#,
            r#"[{"stock":1},7]"#,
            r#"[{"stock":1},true]"#,
        ] {
            assert_eq!(
                scan_element(text, "stock", greater_zero()),
                jqf_data::CountVerdict::Decline,
                "{text}"
            );
        }
    }

    #[test]
    fn ranges_count_only_in_range_elements() {
        let text = r#"[{"stock":-1},{"stock":2},{"stock":3},{"stock":4}]"#;
        let range: jqf_data::SliceRange = (Some(1), Some(3));
        let materializer = JsonSpanMaterializer;
        let filter = jqf_data::CountFilter {
            path: alloc::vec![jqf_data::CountStep::ObjectKey(alloc::string::String::from("stock"))],
            test: greater_zero(),
        };
        let mut resources = crate::test_support::resources();
        assert_eq!(
            materializer
                .count_span_filtered(
                    text,
                    jqf_data::ContainerSpanKind::Array,
                    Some(range),
                    &filter,
                    &mut resources,
                )
                .expect("declines, never errors"),
            jqf_data::CountVerdict::Count(2)
        );
    }

    #[test]
    fn truthiness_answers_the_bare_key_form() {
        // Truthy: `""`, `[]`, `0` (only false/null are falsy; an empty string and empty containers are truthy). The
        // absent member reads null -> falsy.
        let text = r#"[{"k":false},{"k":null},{"k":""},{"k":[]},{},{"k":0}]"#;
        assert_eq!(
            scan_element(text, "k", jqf_data::CountTest::Truthy),
            jqf_data::CountVerdict::Count(3)
        );
    }

    #[test]
    fn object_containers_decline() {
        let materializer = JsonSpanMaterializer;
        let filter = jqf_data::CountFilter {
            path: alloc::vec![jqf_data::CountStep::ObjectKey(alloc::string::String::from("stock"))],
            test: greater_zero(),
        };
        let mut resources = crate::test_support::resources();
        assert_eq!(
            materializer
                .count_span_filtered(
                    r#"{"a":{"stock":1}}"#,
                    jqf_data::ContainerSpanKind::Object,
                    None,
                    &filter,
                    &mut resources,
                )
                .expect("declines, never errors"),
            jqf_data::CountVerdict::Decline
        );
    }
}

/// In-crate coverage for the batch element-iteration machinery: batch boundaries, the skip phase, and the deep-probe
/// pre-pass decision. The routes' cross-crate behavior has differentials upstream; these tests pin the machinery's own
/// laws, deterministically.
#[cfg(test)]
mod element_batch_tests {
    use super::*;
    use jqf_data::{CountStep, ElementDemand, ElementProbe, ElementRow};

    /// What one drive collected: integral numeric probe values exactly, string probe values as text.
    #[derive(Default)]
    struct Seen {
        numbers: alloc::vec::Vec<i64>,
        texts: alloc::vec::Vec<alloc::string::String>,
    }

    impl Seen {
        fn drive(&mut self, text: &str, demand: &ElementDemand) -> jqf_data::ElementVerdict {
            let mut resources = crate::test_support::resources();
            visit_array_span(text, demand, &mut resources, &mut |value, _| {
                match value {
                    Value::Number(number) => self.numbers.push(number.to_i64().expect("test values are integral")),
                    Value::String(text) => {
                        let text: &str = text;
                        self.texts.push(alloc::string::String::from(text));
                    }
                    other => panic!("unexpected probe value {other:?}"),
                }
                Ok(())
            })
            .expect("validated span bytes never raise")
        }
    }

    fn demand(row: ElementRow, range: Option<(Option<i64>, Option<i64>)>, probe: ElementProbe) -> ElementDemand {
        // `run_batches` navigates only `probe`; `increment` belongs to the caller's fold state and is irrelevant to
        // this leaf.
        ElementDemand {
            row,
            path: alloc::vec::Vec::new(),
            range,
            probe,
            increment: None,
        }
    }

    fn key(name: &str) -> CountStep {
        CountStep::ObjectKey(alloc::string::String::from(name))
    }

    fn path_probe(steps: &[&str]) -> ElementProbe {
        ElementProbe::Path(steps.iter().map(|name| key(name)).collect())
    }

    #[test]
    fn batches_split_at_the_element_cap_and_visit_every_element_in_order() {
        // 600 elements = three batches at ELEMENT_BATCH_LEN (256/256/88); the visitor must see all of them, in span
        // order, across every seam.
        let mut text = alloc::string::String::from("[");
        for n in 0..600 {
            if n > 0 {
                text.push(',');
            }
            core::fmt::Write::write_fmt(&mut text, core::format_args!("{n}"))
                .expect("writing a digit into a String cannot fail");
        }
        text.push(']');
        let mut seen = Seen::default();
        let verdict = seen.drive(&text, &demand(ElementRow::FanOut, None, path_probe(&[])));
        assert_eq!(verdict, jqf_data::ElementVerdict::Completed(600));
        assert_eq!(seen.numbers.len(), 600);
        assert_eq!(seen.numbers[0], 0);
        assert_eq!(seen.numbers[255], 255, "last element of the first batch");
        assert_eq!(seen.numbers[256], 256, "first element of the second batch");
        assert_eq!(seen.numbers[599], 599);
    }

    #[test]
    fn a_batch_flushes_at_its_byte_budget_and_still_visits_every_element() {
        // One string larger than ELEMENT_BATCH_BYTES fills the first batch by itself; the flush boundary must not lose
        // or duplicate either side.
        let big = alloc::vec![b'a'; ELEMENT_BATCH_BYTES];
        let big = core::str::from_utf8(&big).expect("ascii");
        let text = alloc::format!("[\"{big}\",\"b\"]");
        let mut seen = Seen::default();
        let verdict = seen.drive(&text, &demand(ElementRow::FanOut, None, path_probe(&[])));
        assert_eq!(verdict, jqf_data::ElementVerdict::Completed(2));
        assert_eq!(seen.texts.len(), 2);
        assert_eq!(seen.texts[0].len(), ELEMENT_BATCH_BYTES);
        assert_eq!(seen.texts[1], "b");
    }

    #[test]
    fn the_skip_phase_never_visits_out_of_range_elements() {
        let mut seen = Seen::default();
        // `[2:]` over five elements visits 2,3,4 — in order.
        let verdict = seen.drive(
            "[0,1,2,3,4]",
            &demand(ElementRow::FanOut, Some((Some(2), None)), path_probe(&[])),
        );
        assert_eq!(verdict, jqf_data::ElementVerdict::Completed(3));
        assert_eq!(seen.numbers, alloc::vec![2, 3, 4]);

        // A start past the container's end is an EMPTY completion.
        let mut seen = Seen::default();
        let verdict = seen.drive(
            "[0,1]",
            &demand(ElementRow::FanOut, Some((Some(9), None)), path_probe(&[])),
        );
        assert_eq!(verdict, jqf_data::ElementVerdict::Completed(0));

        // A closed end bounds the visits: [1:4) over five elements is 1,2.
        let mut seen = Seen::default();
        let verdict = seen.drive(
            "[0,1,2,3,4]",
            &demand(ElementRow::FanOut, Some((Some(1), Some(3))), path_probe(&[])),
        );
        assert_eq!(verdict, jqf_data::ElementVerdict::Completed(2));
        assert_eq!(seen.numbers, alloc::vec![1, 2]);
    }

    #[test]
    fn the_skip_phase_fails_closed_on_truncated_bytes() {
        // Two elements then a cut span: the skip phase runs off the end and the whole walk DECLINES — never a partial
        // element, never an error.
        let mut resources = crate::test_support::resources();
        let mut visited = 0u64;
        let verdict = visit_array_span(
            "[0,1,",
            &demand(ElementRow::FanOut, Some((Some(2), None)), path_probe(&[])),
            &mut resources,
            &mut |_, _| {
                visited += 1;
                Ok(())
            },
        )
        .expect("a decline is a verdict, not an error");
        assert_eq!(verdict, jqf_data::ElementVerdict::Decline);
        assert_eq!(visited, 0);
    }

    #[test]
    fn a_two_step_key_probe_prepasses_and_publishes_nothing_on_a_late_miss() {
        // A two-step key path defeats the first-byte pre-pass (the first byte proves only the element's own category,
        // not the deeper step's), so FanOut falls to the materialize-and-check pass — which succeeds on fully
        // provable elements...
        let mut seen = Seen::default();
        let verdict = seen.drive(
            r#"[{"a":{"b":10}},{"a":{"b":20}}]"#,
            &demand(ElementRow::FanOut, None, path_probe(&["a", "b"])),
        );
        assert_eq!(verdict, jqf_data::ElementVerdict::Completed(2));
        assert_eq!(seen.numbers, alloc::vec![10, 20]);

        // ...and declines on a late miss (the SECOND element's deep step lands on a number) with ZERO published
        // elements.
        let mut resources = crate::test_support::resources();
        let mut visited = 0u64;
        let verdict = visit_array_span(
            r#"[{"a":{"b":10}},{"a":5}]"#,
            &demand(ElementRow::FanOut, None, path_probe(&["a", "b"])),
            &mut resources,
            &mut |_, _| {
                visited += 1;
                Ok(())
            },
        )
        .expect("a decline is a verdict, not an error");
        assert_eq!(verdict, jqf_data::ElementVerdict::Decline);
        assert_eq!(visited, 0, "the visit-all-or-none contract");
    }

    #[test]
    fn a_fold_demand_runs_no_prepass_and_may_decline_mid_fold() {
        // ReduceFold publishes nothing until it completes, so it needs no pre-pass: earlier elements ARE visited before
        // a later miss declines — exactly the shape FanOut's pre-pass exists to prevent.
        let mut seen = Seen::default();
        let verdict = seen.drive(
            r#"[{"a":{"b":10}},{"a":5}]"#,
            &demand(ElementRow::ReduceFold, None, path_probe(&["a", "b"])),
        );
        assert_eq!(verdict, jqf_data::ElementVerdict::Decline);
        assert_eq!(seen.numbers, alloc::vec![10]);
    }

    #[test]
    fn first_byte_provability_table() {
        let proves = |probe| fan_out_first_byte_provable(probe).expect("provable");
        // Empty path: the element itself — every first byte.
        let empty = path_probe(&[]);
        assert!(proves(&empty)(b'"'));
        // One object-key step: object or null.
        let key_path = path_probe(&["k"]);
        let key_step = proves(&key_path);
        assert!(key_step(b'{') && key_step(b'n'));
        assert!(!key_step(b'['));
        assert!(!key_step(b'5'));
        // One index step: array or null.
        let index_path = ElementProbe::Path(alloc::vec![CountStep::ArrayIndex(0)]);
        let index_step = proves(&index_path);
        assert!(index_step(b'[') && index_step(b'n'));
        assert!(!index_step(b'{'));
        // Length: any container or null; scalars decline.
        let length = proves(&ElementProbe::Length);
        assert!(length(b'[') && length(b'{') && length(b'n'));
        assert!(!length(b'"') && !length(b'5'));
        // A deeper path is NOT provable from the first byte alone.
        assert!(fan_out_first_byte_provable(&path_probe(&["a", "b"])).is_none());
        let two_indices = ElementProbe::Path(alloc::vec![CountStep::ArrayIndex(0), CountStep::ArrayIndex(0),]);
        assert!(fan_out_first_byte_provable(&two_indices).is_none());
    }

    #[test]
    fn span_limits_normalize_bounds_to_a_clamped_window() {
        assert_eq!(span_limits(None), (0, None));
        assert_eq!(span_limits(Some((None, None))), (0, None));
        assert_eq!(span_limits(Some((Some(2), Some(5)))), (2, Some(5)));
        // Bounds arrive non-negative-or-open; a defensive negative normalizes to the window start rather than wrapping.
        assert_eq!(span_limits(Some((Some(-3), Some(4)))), (0, Some(4)));
    }
}
