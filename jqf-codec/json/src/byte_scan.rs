//! JSON's byte-scan surface: the stop-set scans from `jqf-codec-core::byte_scan`, wrapped with the JSON-specific
//! predicates, scalar heads, and the `ascii` arm that JSON's own scan shapes need.
//!
//! The kernels themselves live in core: [`prefix_len`] is stop-set-parameterized and monomorphized per set, so each
//! specialization compiles to the hand-written kernel it replaced, verified at extraction by diffing the generated
//! assembly of `escape_prefix_len` and its siblings before and after.

#[cfg(test)]
use jqf_codec_core::byte_scan::{Delimiter, Structural};
use jqf_codec_core::byte_scan::{Escape, PlainString, StopSet, StringContent, Ws, prefix_len};

/// The RFC 7464 record separator, `0x1E`: the one byte that bounds a json-seq unit, raw and unescapable inside a JSON
/// text.
#[derive(Clone, Copy)]
pub(crate) struct Rs;
impl StopSet for Rs {
    const EQ: [u8; 8] = [0x1e, 0, 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 1;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// The JSON text-escape predicate shared by every encoder scan and the escaping writer. DEL is not a control character
/// JSON requires escaping, so an encoder written from the grammar alone would emit the raw byte and disagree with
/// [`crate::json_escape_byte`] on any string containing it. Sharing the predicate is what keeps the unescaped-fast-
/// path scans and the escaping writer from ever disagreeing about the set.
pub(crate) const fn needs_escape(byte: u8) -> bool {
    matches!(byte, b'"' | b'\\' | 0x00..=0x1f | 0x7f)
}

/// Longest prefix of `bytes` containing no byte a strict JSON encoder must escape (`"`, `\`, C0 controls, DEL). The
/// wide kernel stops at exactly the tail predicate's boundary (a 0x7f byte must halt a run the tail would halt, at
/// every alignment), so a run returned here is safe to copy verbatim.
///
/// Under `ascii`, every byte at or above `0x80` must also be escaped, so the scan additionally halts at the first
/// non-ASCII byte. The ascii arm is a plain scalar walk: it is the formatting flag's rare path, and threading the set
/// through the SIMD kernels would forfeit their default-speed budget for a run that never uses them.
pub(crate) fn escape_prefix_len(bytes: &[u8], ascii: bool) -> usize {
    if ascii {
        return bytes
            .iter()
            .take_while(|byte| !needs_escape(**byte) && **byte < 0x80)
            .count();
    }
    // Scalar head, matching the three sibling scans: encoder strings are often a few bytes, and the wide kernel's setup
    // costs more than it saves on those. A run of eight or more reaches SIMD.
    let mut n = 0;
    while n < 8 {
        match bytes.get(n) {
            Some(&byte) if !needs_escape(byte) => n += 1,
            _ => return n,
        }
    }
    8 + prefix_len::<Escape>(&bytes[8..])
}

/// Longest prefix of `bytes` containing no byte that must terminate a plain JSON string run (`"`, `\`, C0 controls,
/// DEL, or any byte >= 0x80).
///
/// No scalar head: this feeds the general string walks, where the first scan usually covers the whole string and an
/// eight-byte head is pure added cost for every run of eight bytes or more (an exactly-eight-byte key pays the full
/// head AND the kernel's setup). Short-run call sites use [`plain_string_prefix_len_short`] instead. Inlined always:
/// the callers are the per-segment string walks, where the call itself is measurable.
#[expect(
    clippy::inline_always,
    reason = "the per-segment scan must fold into the string walk it serves"
)]
#[inline(always)]
pub(crate) fn plain_string_prefix_len(bytes: &[u8]) -> usize {
    prefix_len::<PlainString>(bytes)
}

/// [`plain_string_prefix_len`] with a scalar head, mirroring the whitespace scan's tiering: for the runs between
/// escapes in escape-dense strings — a few bytes long — the wide kernel's setup costs more than the bytes it saves.
/// Only a run of at least eight bytes reaches the SIMD lane. Reserved for call sites whose runs are known short; the
/// general walks pay the head as a tax on every longer string.
#[expect(
    clippy::inline_always,
    reason = "the per-segment scan must fold into the string walk it serves"
)]
#[inline(always)]
pub(crate) fn plain_string_prefix_len_short(bytes: &[u8]) -> usize {
    let mut n = 0;
    while n < 8 {
        match bytes.get(n) {
            Some(&byte) if !PlainString::hit(byte) => n += 1,
            _ => return n,
        }
    }
    8 + prefix_len::<PlainString>(&bytes[8..])
}

/// Longest prefix of `bytes` containing no byte that ends a plain JSON string content run (`"`, `\`, a C0 control, or
/// DEL). Unlike [`plain_string_prefix_len`], non-ASCII bytes are CONTENT here: the caller (the decode-side string walk)
/// uses this to delimit the block it hands to the block UTF-8 validator, which is the SIMD path for unicode-heavy
/// string content.
pub(crate) fn string_content_prefix_len(bytes: &[u8]) -> usize {
    prefix_len::<StringContent>(bytes)
}

/// Longest prefix of `bytes` containing no byte that opens, closes, or splits a container token (`"`, `{`, `[`, `}`,
/// `]`). Everything else — whitespace, scalars, punctuation — is noise the skip can pass in bulk.
///
/// This is the out-of-string arm of the container skip: post-validation every byte outside a string is exactly the
/// non-structural set or one of the five structural bytes, so a run returned here is safe to advance past without
/// looking at a byte.
///
/// Test-side only, hence the gate: the sole caller is the `cfg(test)` `ValueSkip` container walker in `scoped.rs`,
/// beside this module's own kernel-alignment tests.
#[cfg(test)]
pub(crate) fn structural_prefix_len(bytes: &[u8]) -> usize {
    // Scalar head, mirroring the whitespace scan: container noise usually comes in short runs (a quoted key or value
    // every few bytes), where the wide kernel's setup would cost more than the bytes it saves. Only a run of at least
    // eight bytes reaches the SIMD lane.
    let mut n = 0;
    while n < 8 {
        match bytes.get(n) {
            Some(&byte) if !Structural::hit(byte) => n += 1,
            _ => return n,
        }
    }
    8 + prefix_len::<Structural>(&bytes[8..])
}

/// Longest prefix of `bytes` containing no byte that terminates a bare-word value (JSON whitespace, `,`, `]`, `}`) —
/// the `is_delimiter` predicate read as a bulk scan, so a word (`true`, `123`, `null`) is passed whole and the skip
/// stops exactly at the delimiter that ends it.
///
/// Test-side only, hence the gate: the sole caller is the `cfg(test)` `bare_word_run` helper in `scoped.rs`, beside
/// this module's own kernel-alignment tests.
#[cfg(test)]
pub(crate) fn delimiter_prefix_len(bytes: &[u8]) -> usize {
    // Scalar head, for the same reason as `structural_prefix_len`: bare words are short, and the wide kernel pays for
    // itself only on longer runs.
    let mut n = 0;
    while n < 8 {
        match bytes.get(n) {
            Some(&byte) if !Delimiter::hit(byte) => n += 1,
            _ => return n,
        }
    }
    8 + prefix_len::<Delimiter>(&bytes[8..])
}

/// Whether `byte` is JSON structural whitespace (RFC 8259: space, tab, LF, CR).
pub(crate) const fn is_json_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// Longest prefix of `bytes` that is JSON whitespace.
///
/// Tiered for the shape real formatting produces: ordinary formatting leaves runs of a few bytes, which the scalar head
/// exits after one or two compares; a run of eight or more bytes — deep indentation, blank-line runs — engages the
/// arch-width kernel, which skips whole 16-byte lanes. The scalar tail finishes the boundary lane, so the first
/// non-whitespace byte is found exactly. A pure prefix scan: validate-first error positions and resumable-window
/// semantics are unchanged.
#[allow(
    clippy::match_same_arms,
    reason = "end-of-input and first-non-whitespace both answer `n`: the run ended, and the \
              answer is the same either way"
)]
pub(crate) fn ws_prefix_len(bytes: &[u8]) -> usize {
    let mut n = 0;
    while n < 8 {
        match bytes.get(n) {
            None => return n,
            Some(&byte) if is_json_ws(byte) => n += 1,
            Some(_) => return n,
        }
    }
    8 + prefix_len::<Ws>(&bytes[8..])
}

/// Longest prefix of `bytes` that is JSON trivia: whitespace plus — when the grammar arms comments — `//` line and
/// `/* */` block comments. With comments off this is exactly [`ws_prefix_len`]: the one predictable branch on
/// `grammar.comments` is the whole price of the dial, and the strict-JSON hot path pays nothing else.
///
/// A comment that never closes (an unterminated `/*`) consumes the rest of the buffer as trivia; the parse then fails
/// at the missing value/close, so the malformed input is still rejected — only the error position moves.
///
/// When `comments` is `Some`, each skipped comment's TEXT (the bytes after the opening marker with exactly one
/// following space stripped, the TOML §3.15 extraction law mirrored) is appended to it. The collection is allocated
/// only when a comment actually appears and only when the caller arms it (the zero-cost-empty law).
///
/// Returns the prefix length plus whether the scan ended INSIDE a comment whose terminator (`\n` for `//`, `*/` for
/// `/*`) was not found before the buffer end. The caller owns the distinction that follows: a buffer that IS the whole
/// source means the comment genuinely runs to EOF, while a granted window cut mid-run means the terminator lies beyond
/// it and the scan must be retried over a larger window (`JsonParseState::trivia_advance`).
pub(crate) fn trivia_prefix_len(
    bytes: &[u8],
    grammar: crate::storage::JsonGrammar,
    mut comments: Option<&mut alloc::vec::Vec<alloc::string::String>>,
) -> Result<(usize, bool), jqf_resource::ResourceError> {
    let mut n = 0;
    loop {
        let ws = ws_prefix_len(&bytes[n..]);
        n += ws;
        // JSON5's whitespace set: U+2028/U+2029 (line separators) and U+FEFF (whitespace, never a stripped BOM) join
        // the RFC 8259 four. Each is a three-byte sequence; the lead-byte dispatch is the one JSON5 price this scan
        // pays.
        if grammar.json5 && n < bytes.len() {
            match (bytes.get(n), bytes.get(n + 1), bytes.get(n + 2)) {
                (Some(0xe2), Some(0x80), Some(0xa8 | 0xa9)) | (Some(0xef), Some(0xbb), Some(0xbf)) => n += 3,
                _ => {}
            }
        }
        if n >= bytes.len() || !grammar.comments {
            return Ok((n, false));
        }
        match (bytes.get(n), bytes.get(n + 1)) {
            (Some(b'/'), Some(b'/')) => {
                let start = n + 2;
                n += 2;
                while n < bytes.len() && bytes[n] != b'\n' {
                    n += 1;
                }
                let open = n == bytes.len();
                if let Some(out) = comments.as_deref_mut() {
                    // A `\r` immediately before the `\n` is half of a CRLF line break, not content — the TOML twin
                    // strips it too. A `\r` at EOF (no `\n`) has no break to belong to.
                    let mut text = &bytes[start..n];
                    if !open {
                        text = text.strip_suffix(b"\r").unwrap_or(text);
                    }
                    push_comment(out, text)?;
                }
                if open {
                    return Ok((n, true));
                }
            }
            (Some(b'/'), Some(b'*')) => {
                let start = n + 2;
                n += 2;
                let mut closed = false;
                while n + 1 < bytes.len() {
                    if bytes[n] == b'*' && bytes[n + 1] == b'/' {
                        closed = true;
                        break;
                    }
                    n += 1;
                }
                if closed {
                    n += 2;
                } else {
                    // An unterminated block comment consumes the WHOLE remaining buffer (the scan above stops one byte
                    // short); the parse then fails at the missing value/close, so the malformed input is still rejected
                    // — only the error position moves. When this buffer is a granted window rather than the whole
                    // source, the caller retries over a larger window instead of acting on the truncation.
                    n = bytes.len();
                }
                if let Some(out) = comments.as_deref_mut() {
                    let text = &bytes[start..n];
                    let text = if closed {
                        let text = text.strip_suffix(b"*/").unwrap_or(text);
                        text.strip_suffix(b" ").unwrap_or(text)
                    } else {
                        text
                    };
                    push_comment(out, text)?;
                }
                if !closed {
                    return Ok((n, true));
                }
            }
            _ => return Ok((n, false)),
        }
    }
}

/// Appends one comment's text to a run through FALLIBLE reservations.
///
/// Both the run and each text are sized by the input — a comment-heavy document controls how much this grows — so
/// neither may grow through an aborting allocation.
fn push_comment(
    run: &mut alloc::vec::Vec<alloc::string::String>,
    bytes: &[u8],
) -> Result<(), jqf_resource::ResourceError> {
    let text = comment_text(bytes)?;
    run.try_reserve(1)?;
    run.push(text);
    Ok(())
}

/// One comment's text in the `<fmt>.comment@1` payload shape: the bytes after the opening marker (`//` or `/*`), with
/// exactly one immediately following space stripped. Line comments run to the line break (not including it); block
/// comments arrive with their closing `*/` (and one trailing space) already removed by the caller, so `/* foo */` and
/// `// foo` both answer `foo`.
fn comment_text(bytes: &[u8]) -> Result<alloc::string::String, jqf_resource::ResourceError> {
    let bytes = match bytes.first() {
        Some(b' ') => &bytes[1..],
        _ => bytes,
    };
    let lossy = alloc::string::String::from_utf8_lossy(bytes);
    let mut text = alloc::string::String::new();
    text.try_reserve_exact(lossy.len())?;
    text.push_str(&lossy);
    Ok(text)
}

// The windowed first-invalid-UTF-8 scan: the type and driver live here, beside their only consumer, while the
// hand-written lane kernels stay in `jqf-codec-core::byte_scan` (`x86_64`/`aarch64` below).

#[cfg(target_arch = "aarch64")]
use jqf_codec_core::byte_scan::aarch64;
#[cfg(target_arch = "x86_64")]
use jqf_codec_core::byte_scan::x86_64;

/// Bytes of a multi-byte UTF-8 sequence that were truncated by a window boundary. Non-empty exactly when a lead (plus
/// up to three validated continuations) ended the previous window and the source has not ended; `bytes[0]` is the lead,
/// `bytes[1..len]` are the continuations already validated. The lead's absolute position is `window_base - len`.
#[derive(Clone, Copy)]
pub(crate) struct Utf8Carry {
    bytes: [u8; 4],
    len: u8,
}

impl Utf8Carry {
    /// The empty carry: no truncated sequence pending.
    pub(crate) const EMPTY: Self = Self { bytes: [0; 4], len: 0 };

    /// Whether no truncated sequence is pending. A non-`final_chunk` scan that returned no error leaves this false
    /// exactly when the window's last sequence needs bytes the window did not have.
    #[must_use]
    pub(crate) const fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl Default for Utf8Carry {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// Scans `bytes` (one admission window, at absolute offset `base`) for the first byte that cannot begin or continue a
/// valid UTF-8 sequence, resuming any sequence truncated by the previous window from `carry`.
///
/// Returns the ABSOLUTE position of the first invalid byte — which may sit in the carried prefix (`base - carry.len`,
/// when a carried lead's sequence is invalidated by this window's first byte). `final_chunk` must be true only for the
/// last window of the source: a truncated sequence at its end is then an error at the lead, and is otherwise carried
/// into `carry` for the next window. On return with no error, `carry` holds any newly truncated sequence (or is empty).
///
/// The error positions agree with `str::from_utf8`'s `valid_up_to`: an invalid sequence is reported at its lead byte, a
/// stray continuation at itself, and a truncated final sequence at its lead.
pub(crate) fn utf8_first_invalid(bytes: &[u8], base: usize, carry: &mut Utf8Carry, final_chunk: bool) -> Option<usize> {
    // The carried lead's first continuation is the only special-range pair the SIMD lanes cannot see (the lead is
    // before this window), so check it once.
    if carry.len == 1
        && let Some(&first) = bytes.first()
        && first & 0xC0 == 0x80
        && special_second_violated(carry.bytes[0], first)
    {
        return Some(base - 1);
    }
    // A non-continuation first byte after a carried lead is flagged by the first lane's must_cont check (prev1 is the
    // lead); the fallback reports the lead.
    #[cfg(target_arch = "x86_64")]
    {
        if x86_64::avx2() {
            // SAFETY: the AVX2 kernels require the feature `avx2()` just verified, and the caller guarantees their
            // loads.
            unsafe {
                utf8_lane_scan::<32>(
                    bytes,
                    base,
                    carry,
                    final_chunk,
                    x86_64::avx2::lane_ascii_clean,
                    x86_64::avx2::lane_has_invalid,
                )
            }
        } else {
            // SAFETY: x86-64 guarantees SSE2 and the caller guarantees the loads.
            unsafe {
                utf8_lane_scan::<16>(
                    bytes,
                    base,
                    carry,
                    final_chunk,
                    x86_64::sse2::lane_ascii_clean,
                    x86_64::sse2::lane_has_invalid,
                )
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: AArch64 guarantees NEON and the caller guarantees the loads.
        unsafe {
            utf8_lane_scan::<16>(
                bytes,
                base,
                carry,
                final_chunk,
                aarch64::lane_ascii_clean,
                aarch64::lane_has_invalid,
            )
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        // SAFETY: the dummy kernels touch no memory and the caller guarantees the load bounds.
        unsafe {
            utf8_lane_scan::<16>(
                bytes,
                base,
                carry,
                final_chunk,
                scalar_ascii_clean_dummy,
                scalar_has_invalid_dummy,
            )
        }
    }
}

/// A UTF-8 lane driver that scans `bytes` in `W`-byte lanes, resuming any sequence truncated by the previous window
/// from `carry`. `W` is 16 for the NEON/SSE2 kernels and 32 for the AVX2 kernels; the caller selects the kernels to
/// match. The carry/seed/scalar-fallback machinery is shared, so the two widths differ only in the kernels and the lane
/// stride.
///
/// `ascii_clean` answers whether a whole lane is provably error-free (all ASCII, no boundary demand); `has_invalid`
/// answers whether a lane contains any invalid byte, given the previous 16 bytes and the next `W` (zero padded past the
/// end). Both are `#[target_feature]` kernels and both require their feature to be enabled — the caller's `SAFETY`
/// note covers that, because calling a `#[target_feature]` function through this generic pointer is what keeps the
/// feature-gated code from leaking into the caller.
///
/// # SAFETY
///
/// Both kernels are only sound under their declared `target_feature`, which the caller has verified before passing them
/// here; this function itself must never be instantiated with a kernel whose feature is absent.
unsafe fn utf8_lane_scan<const W: usize>(
    bytes: &[u8],
    base: usize,
    carry: &mut Utf8Carry,
    final_chunk: bool,
    ascii_clean: unsafe fn(&[u8; W], &[u8; 16]) -> bool,
    has_invalid: unsafe fn(&[u8; W], &[u8; 16], &[u8; W]) -> bool,
) -> Option<usize> {
    let mut pos = 0;
    let mut prev16 = init_prev16(*carry);
    while bytes.len() - pos >= W {
        let v = load_lane::<W>(bytes, pos);
        // Fast path: a lane whose bytes are all ASCII, with no pending continuation demanded from the previous lane,
        // provably contains no UTF-8 error — no continuation byte, lead byte, or special-range lead can exist below
        // 0x80, and the boundary mask is empty. Skipping the state machine here also skips the lookahead load, which is
        // the dominant per-lane cost on ASCII-heavy documents.
        let lane_clean = unsafe { ascii_clean(&v, &prev16) };
        if lane_clean {
            prev16 = lane_tail::<W>(&v);
            pos += W;
            continue;
        }
        let next = if bytes.len() - pos >= 2 * W {
            load_lane::<W>(bytes, pos + W)
        } else {
            padded_next::<W>(bytes, pos + W)
        };
        let bad = unsafe { has_invalid(&v, &prev16, &next) };
        if bad {
            let to = (pos + W).min(bytes.len());
            let (seed, mut local) = if pos == 0 {
                (carry_to_seed(*carry, base), core::mem::take(carry))
            } else {
                // The previous lanes were SIMD-clean: seed the walk from their last three bytes and discard the (empty)
                // window carry.
                (derive_seed(bytes, pos, base), Utf8Carry::EMPTY)
            };
            if let Some(found) = scalar_utf8(bytes, pos, to, seed, base, &mut local, final_chunk && to == bytes.len()) {
                return Some(found);
            }
            // False positive only when the padded lookahead flagged a lead truncated at the window tail. The next
            // lane's prev16 context resumes the sequence, so the local carry is discarded.
            prev16 = lane_tail::<W>(&v);
            pos += W;
            continue;
        }
        prev16 = lane_tail::<W>(&v);
        pos += W;
    }
    let seed = if pos == 0 {
        carry_to_seed(*carry, base)
    } else {
        derive_seed(bytes, pos, base)
    };
    scalar_utf8(bytes, pos, bytes.len(), seed, base, carry, final_chunk)
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn scalar_ascii_clean_dummy(_: &[u8; 16], _: &[u8; 16]) -> bool {
    false
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn scalar_has_invalid_dummy(_: &[u8; 16], _: &[u8; 16], _: &[u8; 16]) -> bool {
    true
}

/// The pending-sequence seed the scalar walk resumes with: the absolute position of the lead, the lead byte, the
/// continuation bytes seen so far, the seen count, and the total continuations the lead demands.
type Pending = (usize, u8, [u8; 3], usize, usize);

/// Seeds the walk from a window carry: the lead's absolute position is `base - carry.len` because the carried bytes are
/// exactly the previous window's trailing sequence.
fn carry_to_seed(carry: Utf8Carry, base: usize) -> Option<Pending> {
    if carry.len > 0 {
        let lead = carry.bytes[0];
        let total = continuation_count(lead).unwrap_or(0);
        Some((
            base - usize::from(carry.len),
            lead,
            [carry.bytes[1], carry.bytes[2], carry.bytes[3]],
            usize::from(carry.len - 1),
            total,
        ))
    } else {
        None
    }
}

/// Seeds the walk at a region boundary from the previous region's last three bytes. The SIMD lanes are clean before a
/// boundary, so a lead within the last three bytes that still demands continuations is the only pending that can exist;
/// anything else completed before the boundary.
fn derive_seed(bytes: &[u8], pos: usize, base: usize) -> Option<Pending> {
    debug_assert!(pos >= 3);
    let p1 = bytes[pos - 1];
    let p2 = bytes[pos - 2];
    let p3 = bytes[pos - 3];
    if let Some(total) = continuation_count(p1) {
        Some((base + pos - 1, p1, [0; 3], 0, total))
    } else if p1 & 0xC0 == 0x80 {
        if let Some(total) = continuation_count(p2) {
            if total >= 2 {
                Some((base + pos - 2, p2, [p1, 0, 0], 1, total))
            } else {
                None // a lead2 completed at p1
            }
        } else if p2 & 0xC0 == 0x80 {
            if continuation_count(p3) == Some(3) {
                Some((base + pos - 3, p3, [p2, p1, 0], 2, 3))
            } else {
                None // the 3-byte sequence completed at p2, or p1 is stray
            }
        } else {
            None
        }
    } else {
        None
    }
}

/// Validates `bytes[start..limit]` as UTF-8 from a KNOWN pending state (`seed`), so the walk never starts mid-sequence.
/// Returns the absolute position of the first invalid byte; on a valid-but-truncated end with `final_chunk` false, the
/// pending sequence is stored into `out_carry`.
fn scalar_utf8(
    bytes: &[u8],
    start: usize,
    limit: usize,
    seed: Option<Pending>,
    base: usize,
    out_carry: &mut Utf8Carry,
    final_chunk: bool,
) -> Option<usize> {
    let mut pending: Option<Pending> = seed;
    let mut p = start;
    while p < limit {
        let byte = bytes[p];
        if let Some((lead_abs, lead, seen_bytes, seen, total)) = pending {
            if byte & 0xC0 == 0x80 {
                let consumed = seen + 1;
                if consumed == 1 && special_second_violated(lead, byte) {
                    return Some(lead_abs);
                }
                let mut seen_bytes = seen_bytes;
                seen_bytes[consumed - 1] = byte;
                if consumed == total {
                    pending = None;
                } else {
                    pending = Some((lead_abs, lead, seen_bytes, consumed, total));
                }
            } else {
                return Some(lead_abs);
            }
        } else if byte < 0x80 {
            // ASCII: no constraint.
        } else if byte & 0xC0 == 0x80 {
            // A continuation with no sequence to continue.
            return Some(base + p);
        } else if let Some(total) = continuation_count(byte) {
            pending = Some((base + p, byte, [0; 3], 0, total));
        } else {
            // C0/C1 overlong leads and F5..=FF are invalid on their own.
            return Some(base + p);
        }
        p += 1;
    }
    if let Some((lead_abs, lead, seen_bytes, seen, _total)) = pending {
        if final_chunk {
            return Some(lead_abs);
        }
        out_carry.bytes[0] = lead;
        let mut i = 0;
        while i < seen {
            out_carry.bytes[1 + i] = seen_bytes[i];
            i += 1;
        }
        out_carry.len = u8::try_from(seen + 1).unwrap_or(4);
        return None;
    }
    out_carry.len = 0;
    None
}

/// Continuations a lead byte demands, or `None` for a byte that cannot begin a scalar (ASCII, continuations, C0/C1,
/// F5..=FF).
fn continuation_count(byte: u8) -> Option<usize> {
    match byte {
        0xc2..=0xdf => Some(1),
        0xe0..=0xef => Some(2),
        0xf0..=0xf4 => Some(3),
        _ => None,
    }
}

/// The second-byte range laws that keep 3- and 4-byte sequences out of the overlong and surrogate encodings and below
/// U+10FFFF. Applied only to the FIRST continuation of a sequence.
fn special_second_violated(lead: u8, second: u8) -> bool {
    match lead {
        0xE0 => second < 0xA0,
        0xED => second > 0x9F,
        0xF0 => second < 0x90,
        0xF4 => second > 0x8F,
        _ => false,
    }
}

fn init_prev16(carry: Utf8Carry) -> [u8; 16] {
    let mut prev = [0_u8; 16];
    prev[16 - usize::from(carry.len)..16].copy_from_slice(&carry.bytes[..usize::from(carry.len)]);
    prev
}

fn load_lane<const W: usize>(bytes: &[u8], offset: usize) -> [u8; W] {
    let mut lane = [0_u8; W];
    lane.copy_from_slice(&bytes[offset..offset + W]);
    lane
}

fn padded_next<const W: usize>(bytes: &[u8], offset: usize) -> [u8; W] {
    let mut lane = [0_u8; W];
    let available = bytes.len().saturating_sub(offset).min(W);
    lane[..available].copy_from_slice(&bytes[offset..offset + available]);
    lane
}

/// The trailing 16 bytes of a lane, which is what the NEXT lane's boundary mask reads (its last three bytes are the
/// previous lane's last three). For a 16-byte lane this is the lane itself; for a 32-byte lane it is its upper half.
fn lane_tail<const W: usize>(lane: &[u8; W]) -> [u8; 16] {
    let mut tail = [0_u8; 16];
    tail.copy_from_slice(&lane[W - 16..]);
    tail
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// The scalar predicate `escape_prefix_len` must agree with: the wide kernel only stops at the tail's own boundary.
    fn scalar_escape_prefix(bytes: &[u8]) -> usize {
        bytes.iter().take_while(|byte| !needs_escape(**byte)).count()
    }

    /// The scalar whitespace prefix the tiered kernel must agree with.
    fn scalar_ws_prefix(bytes: &[u8]) -> usize {
        bytes.iter().take_while(|byte| is_json_ws(**byte)).count()
    }

    #[test]
    fn ws_prefix_agrees_with_the_scalar_predicate_at_every_alignment() {
        let mut corpus: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b" ".to_vec(),
            b"\t".to_vec(),
            b"\n".to_vec(),
            b"\r".to_vec(),
            b"\x0b".to_vec(),
            b"  a".to_vec(),
            b"\n\n\n".to_vec(),
            b"\r\n  \t".to_vec(),
            b" \t\n\r a".to_vec(),
            b"{\"k\": 1}".to_vec(),
        ];
        let mut state = 0xa076_1d64_78bd_642f_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 0..48 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                // Bias the corpus toward whitespace and near-whitespace so runs cross the 8-byte scalar-head boundary
                // and the 16-byte lane boundary often.
                let r = mix(&mut state);
                bytes.push(match r % 8 {
                    0..=4 => b" \t\n\r"[((r >> 8) & 3) as usize],
                    _ => ((r >> 16) & 0xFF) as u8,
                });
            }
            corpus.push(bytes);
        }
        for bytes in &corpus {
            for start in 0..=bytes.len().min(3) {
                for end in start..=bytes.len().min(start + 48) {
                    let slice = &bytes[start..end];
                    assert_eq!(
                        ws_prefix_len(slice),
                        scalar_ws_prefix(slice),
                        "ws_prefix mismatch at {start}..{end} of {bytes:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn escape_prefix_agrees_with_the_scalar_predicate_at_every_alignment() {
        let mut corpus: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"\"".to_vec(),
            b"\\".to_vec(),
            b"\x1f".to_vec(),
            b"\x20".to_vec(),
            b"\x7f".to_vec(),
            b"\x80".to_vec(),
            b"\x7e".to_vec(),
            b"plain text".to_vec(),
            b"\"escaped\"".to_vec(),
            b"a\x7fb\x80c\x1fd".to_vec(),
        ];
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 0..48 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                bytes.push((mix(&mut state) & 0xFF) as u8);
            }
            corpus.push(bytes);
        }
        for bytes in &corpus {
            for start in 0..=bytes.len().min(3) {
                for end in start..=bytes.len().min(start + 40) {
                    let slice = &bytes[start..end];
                    assert_eq!(
                        escape_prefix_len(slice, false),
                        scalar_escape_prefix(slice),
                        "escape_prefix mismatch at {start}..{end} of {bytes:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn structural_prefix_agrees_with_the_scalar_predicate_at_every_alignment() {
        let mut corpus: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"plain".to_vec(),
            b"{\"a\":1}".to_vec(),
            b"[1,[2]]".to_vec(),
            b"\"\"".to_vec(),
            b"a}b[".to_vec(),
            b"\x00\x7f\x80".to_vec(),
        ];
        let mut state = 0xb10c_5a2d_3e4f_6071_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 0..48 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                bytes.push((mix(&mut state) & 0xFF) as u8);
            }
            corpus.push(bytes);
        }
        let scalar = |bytes: &[u8]| {
            bytes
                .iter()
                .take_while(|byte| !matches!(byte, b'"' | b'{' | b'[' | b'}' | b']'))
                .count()
        };
        for bytes in &corpus {
            for start in 0..=bytes.len().min(3) {
                for end in start..=bytes.len().min(start + 40) {
                    let slice = &bytes[start..end];
                    assert_eq!(
                        structural_prefix_len(slice),
                        scalar(slice),
                        "structural_prefix mismatch at {start}..{end} of {bytes:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn delimiter_prefix_agrees_with_the_scalar_predicate_at_every_alignment() {
        let mut corpus: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"123,".to_vec(),
            b"true".to_vec(),
            b"null}".to_vec(),
            b"\"a\"".to_vec(),
            b" \t\n\r".to_vec(),
            b"a]b}c".to_vec(),
            b"\x1f\x80\xff".to_vec(),
        ];
        let mut state = 0x2b7e_1516_2829_3031_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 0..48 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                bytes.push((mix(&mut state) & 0xFF) as u8);
            }
            corpus.push(bytes);
        }
        let scalar = |bytes: &[u8]| {
            bytes
                .iter()
                .take_while(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}'))
                .count()
        };
        for bytes in &corpus {
            for start in 0..=bytes.len().min(3) {
                for end in start..=bytes.len().min(start + 40) {
                    let slice = &bytes[start..end];
                    assert_eq!(
                        delimiter_prefix_len(slice),
                        scalar(slice),
                        "delimiter_prefix mismatch at {start}..{end} of {bytes:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn escape_prefix_keeps_the_plain_run_contract() {
        // A run returned by the kernel must contain no byte the writer would escape, and must stop at the first one.
        for byte in 0_u8..=u8::MAX {
            let bytes = [b'a', b'b', byte, b'c'];
            let run = escape_prefix_len(&bytes, false);
            assert!(run <= bytes.len(), "run overran at byte {byte}");
            if run < bytes.len() {
                assert!(needs_escape(bytes[run]), "run stopped early at byte {byte}");
            }
            for (prior, &passed) in bytes[..run].iter().enumerate() {
                assert!(!needs_escape(passed), "run skipped byte {prior} at {byte}");
            }
        }
    }

    #[test]
    fn plain_prefix_keeps_its_own_contract() {
        for byte in 0_u8..=u8::MAX {
            let bytes = [byte, b'x'];
            let run = plain_string_prefix_len(&bytes);
            assert!(run <= 2);
            if run < 2 {
                let stopped = bytes[run];
                assert!(
                    !(0x20..=0x7e).contains(&stopped) || matches!(stopped, b'"' | b'\\'),
                    "plain run stopped at a plain byte {stopped:#x} at {byte}"
                );
            }
            if run > 0 {
                let passed = bytes[0];
                assert!(
                    matches!(passed, 0x20..=0x7e) && !matches!(passed, b'"' | b'\\'),
                    "plain run admitted {passed:#x} at {byte}"
                );
            }
        }
    }

    #[test]
    fn escape_prefix_stops_inside_a_lane_whose_bytes_are_all_below_space() {
        // A whole 16-byte lane of control bytes has no byte above 0x20, so an x86 kernel whose `< 0x20` compare is
        // transposed into `> 0x20` sees an empty mask and skips the lane wholesale — past sixteen bytes the writer
        // must escape. The scalar tail cannot recover: it resumes AT the skipped lane's end.
        for control in [0x00_u8, 0x01, 0x09, 0x0a, 0x0d, 0x1f] {
            let lane = [control; 16];
            assert_eq!(
                escape_prefix_len(&lane, false),
                0,
                "a lane of {control:#x} must not be admitted"
            );
            let mut mixed = vec![b'a'; 16];
            mixed.extend_from_slice(&lane);
            assert_eq!(
                escape_prefix_len(&mixed, false),
                16,
                "the run must stop at the {control:#x} lane"
            );
        }
        // The complement: a lane of bytes that need no escaping must still be admitted whole, including the 0x20 and
        // 0x7e boundaries.
        for plain in [0x20_u8, 0x21, 0x5b, 0x7e, 0x80, 0xff] {
            let lane = [plain; 16];
            assert_eq!(
                escape_prefix_len(&lane, false),
                16,
                "a lane of {plain:#x} must be admitted whole"
            );
        }
    }

    #[test]
    fn prefix_scans_agree_with_the_scalar_predicates_at_lane_boundaries() {
        // Adversarial placement: each terminator at every position around the first and second lane boundaries, over
        // both prefix scans.
        for terminator in [b'"', b'\\', 0x00, 0x01, 0x1f, 0x7f, 0x80, 0xff] {
            for position in 0..40 {
                let mut bytes = vec![b'a'; 40];
                bytes[position] = terminator;
                assert_eq!(
                    escape_prefix_len(&bytes, false),
                    scalar_escape_prefix(&bytes),
                    "escape_prefix with {terminator:#x} at {position}"
                );
                let expected_plain = bytes
                    .iter()
                    .take_while(|byte| matches!(byte, 0x20..=0x7e) && !matches!(byte, b'"' | b'\\'))
                    .count();
                assert_eq!(
                    plain_string_prefix_len(&bytes),
                    expected_plain,
                    "plain_prefix with {terminator:#x} at {position}"
                );
                assert_eq!(
                    plain_string_prefix_len_short(&bytes),
                    expected_plain,
                    "plain_prefix_short with {terminator:#x} at {position}"
                );
            }
        }
    }
    const HIGH: [u8; 8] = [0xC3, 0xE2, 0xF0, 0xED, 0x80, 0x82, 0x9F, 0xFF];

    fn oracle(bytes: &[u8]) -> Option<usize> {
        match core::str::from_utf8(bytes) {
            Ok(_) => None,
            Err(error) => Some(error.valid_up_to()),
        }
    }

    fn whole_scan(bytes: &[u8]) -> Option<usize> {
        let mut carry = Utf8Carry::EMPTY;
        utf8_first_invalid(bytes, 0, &mut carry, true)
    }

    #[test]
    fn utf8_kernel_agrees_with_from_utf8_over_adversarial_corpora() {
        // Every single byte, and every two-byte window.
        for first in 0_u8..=u8::MAX {
            let bytes = [first];
            assert_eq!(whole_scan(&bytes), oracle(&bytes), "byte {first}");
        }
        for first in 0_u8..=u8::MAX {
            for second in 0_u8..=u8::MAX {
                let bytes = [first, second];
                assert_eq!(whole_scan(&bytes), oracle(&bytes), "bytes {first} {second}");
            }
        }
        // Every truncation of the interesting 2-4 byte sequences.
        for lead in [0xC2, 0xDF, 0xE0, 0xE1, 0xEC, 0xED, 0xEE, 0xF0, 0xF1, 0xF4, 0xF5] {
            for tail in 0_u8..=u8::MAX {
                let bytes = [lead, tail];
                assert_eq!(whole_scan(&bytes), oracle(&bytes), "lead {lead:#x} tail {tail}");
                let bytes = [lead, tail, 0x80];
                assert_eq!(whole_scan(&bytes), oracle(&bytes), "lead {lead:#x} tail {tail} x2");
                let bytes = [lead, tail, 0x80, 0x80];
                assert_eq!(whole_scan(&bytes), oracle(&bytes), "lead {lead:#x} tail {tail} x3");
            }
        }
        // A pseudo-random corpus with bias toward high bytes.
        let mut state = 0xd1b5_4a32_d192_ed03_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 0..40 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                let r = mix(&mut state);
                bytes.push(if r % 4 == 0 {
                    ((r >> 8) & 0xFF) as u8
                } else {
                    HIGH[((r >> 16) & 7) as usize]
                });
            }
            assert_eq!(whole_scan(&bytes), oracle(&bytes), "random len {len}: {bytes:?}");
        }
    }

    #[test]
    fn utf8_kernel_windows_agree_with_the_whole_buffer() {
        // Splitting a buffer at every boundary must give the same first invalid byte as validating it whole, with the
        // carry crossing windows.
        let seeds: &[&[u8]] = &[
            b"",
            b"hello",
            "héllo wörld".as_bytes(),
            "日本語テキスト".as_bytes(),
            b"{\"k\":\"\xC3\xA9\",\"n\":123}",
            b"truncated \xE2\x82",
            b"bad \xE2\x82\x28 here",
            b"overlong \xC0\xAF",
            b"surrogate \xED\xA0\x80",
            b"\xF4\x90\x80\x80 past range",
            b"\xF0\x90\x80\x80 valid four",
            b"\xE2\x82\xAC euro",
            "mix: 中文 + é + \u{1F600} emoji".as_bytes(),
            b"\x22\xC3\x2B ra, u",
            b"\x22\xC3\x28 ra, u",
        ];
        for seed in seeds {
            let expected = oracle(seed);
            for split in 0..=seed.len() {
                let mut carry = Utf8Carry::EMPTY;
                let mut found = None;
                if let Some(pos) = utf8_first_invalid(&seed[..split], 0, &mut carry, false) {
                    found = Some(pos);
                }
                if found.is_none()
                    && let Some(pos) = utf8_first_invalid(&seed[split..], split, &mut carry, true)
                {
                    found = Some(pos);
                }
                assert_eq!(
                    found, expected,
                    "window split at {split} of {seed:?} (carry len {})",
                    carry.len
                );
            }
        }
    }

    #[test]
    fn utf8_ascii_fast_path_is_clean_and_honors_carried_leads() {
        // A buffer long enough to cross several 16-byte lanes, pure ASCII: the fast path must never report an error at
        // any window split.
        let ascii = b"{\"catalog\":[{\"name\":\"item-000\",\"kind\":\"x\"}],\"n\":1234567890}";
        assert!(ascii.len() >= 48, "fixture must span several lanes");
        for split in 0..=ascii.len() {
            let mut carry = Utf8Carry::EMPTY;
            assert_eq!(
                utf8_first_invalid(&ascii[..split], 0, &mut carry, false),
                None,
                "ascii prefix window at {split}"
            );
            assert_eq!(
                utf8_first_invalid(&ascii[split..], split, &mut carry, true),
                None,
                "ascii suffix window at {split}"
            );
        }

        // A 3-byte lead truncated at a 16-byte lane boundary, followed by a pure-ASCII lane: the fast path sees a
        // non-empty boundary mask (the carried lead demands a continuation at lane position 0) and must report the
        // error at the lead rather than skipping the lane.
        let mut bytes = vec![b'a'; 15];
        bytes.push(0xE2);
        bytes.extend(core::iter::repeat_n(b'b', 16));
        let lead_pos = 15;
        let mut carry = Utf8Carry::EMPTY;
        assert_eq!(
            utf8_first_invalid(&bytes[..16], 0, &mut carry, false),
            None,
            "the truncated lead is carried, not reported"
        );
        assert_eq!(carry.len, 1, "the 3-byte lead is pending");
        assert_eq!(
            utf8_first_invalid(&bytes[16..], 16, &mut carry, true),
            Some(lead_pos),
            "a non-continuation after the carried lead is reported at its lead"
        );

        // The same layout with ONE continuation after the carried lead is still an error: 0xE2 demands two, and the
        // second slot holds `b`. The boundary mask must demand a continuation at new-lane position 1 as well as
        // position 0, or the lane is skipped and the truncation is silently accepted.
        let mut short = vec![b'a'; 15];
        short.push(0xE2);
        short.push(0x82);
        short.extend(core::iter::repeat_n(b'b', 15));
        assert_eq!(oracle(&short), Some(lead_pos), "fixture is invalid UTF-8");
        let mut carry = Utf8Carry::EMPTY;
        assert_eq!(utf8_first_invalid(&short[..16], 0, &mut carry, false), None);
        assert_eq!(carry.len, 1);
        assert_eq!(
            utf8_first_invalid(&short[16..], 16, &mut carry, true),
            Some(lead_pos),
            "a carried 3-byte lead one continuation short is reported at its lead"
        );

        // The same layout with BOTH continuations present is clean: the boundary mask still routes through the state
        // machine, which completes the sequence without an error.
        let mut ok = vec![b'a'; 15];
        ok.push(0xE2);
        ok.push(0x82);
        ok.push(0xAC);
        ok.extend(core::iter::repeat_n(b'b', 14));
        assert_eq!(oracle(&ok), None, "fixture is valid UTF-8");
        let mut carry = Utf8Carry::EMPTY;
        assert_eq!(utf8_first_invalid(&ok[..16], 0, &mut carry, false), None);
        assert_eq!(carry.len, 1);
        assert_eq!(
            utf8_first_invalid(&ok[16..], 16, &mut carry, true),
            None,
            "a carried lead completed by both continuations stays clean"
        );
    }

    #[test]
    fn utf8_fast_path_never_skips_a_lane_of_bytes_above_ascii() {
        // A 16-byte lane whose bytes are ALL above 0x80 must not be taken for an ASCII lane. On x86 the ASCII test is
        // an unsigned `>= 0x80` compare; with its operands transposed it reads as `<= 0x80`, which is empty for such a
        // lane and skips the whole state machine.
        for byte in 0x81_u8..=0xFF {
            let lane = [byte; 16];
            assert_eq!(whole_scan(&lane), oracle(&lane), "full lane of {byte:#x}");
            // The same lane sandwiched between ASCII lanes, so the fast path is entered with a clean carry and a
            // lane-aligned start.
            let mut padded = vec![b'a'; 16];
            padded.extend_from_slice(&lane);
            padded.extend(core::iter::repeat_n(b'a', 16));
            assert_eq!(
                whole_scan(&padded),
                oracle(&padded),
                "ascii-sandwiched lane of {byte:#x}"
            );
        }
        // Valid multi-byte text filling a whole lane must still be accepted: the fix must not turn "not ASCII" into
        // "not valid".
        let euro_lane = "€€€€€".as_bytes(); // 5 x 3 bytes = 15
        let mut valid = euro_lane.to_vec();
        valid.push(b'a');
        assert_eq!(valid.len(), 16);
        assert_eq!(whole_scan(&valid), None, "a full lane of valid UTF-8");
        let two_byte_lane = "éééééééé".as_bytes(); // 8 x 2 bytes = 16
        assert_eq!(two_byte_lane.len(), 16);
        assert_eq!(whole_scan(two_byte_lane), None, "a full lane of two-byte scalars");
    }

    #[test]
    fn utf8_lane_boundary_leads_demand_every_continuation_they_owe() {
        // A multi-byte lead placed at lane positions 13, 14 and 15 owes continuations that land in the NEXT lane, where
        // only the boundary mask can demand them. Every truncation depth is checked against `from_utf8`, so both under-
        // and over-rejection fail.
        for (lead, conts) in [
            (0xE2_u8, &[0x82_u8, 0xAC][..]), // U+20AC
            (0xF0, &[0x90, 0x80, 0x80][..]), // U+10000
            (0xF4, &[0x8F, 0xBF, 0xBF][..]), // U+10FFFF
            (0xC3, &[0xA9][..]),             // U+00E9
        ] {
            for lead_pos in [13_usize, 14, 15] {
                for supplied in 0..=conts.len() {
                    let mut bytes = vec![b'a'; lead_pos];
                    bytes.push(lead);
                    bytes.extend_from_slice(&conts[..supplied]);
                    // Pad well past the lead's lane so the scan runs whole lanes on both sides of the boundary.
                    while bytes.len() < 48 {
                        bytes.push(b'b');
                    }
                    let expected = oracle(&bytes);
                    assert_eq!(
                        whole_scan(&bytes),
                        expected,
                        "lead {lead:#x} at {lead_pos} with {supplied} continuations"
                    );
                    // And the same bytes split into two windows at every boundary near the sequence, so the carry path
                    // sees it too.
                    for split in lead_pos..=(lead_pos + conts.len() + 1) {
                        let mut carry = Utf8Carry::EMPTY;
                        let mut found = utf8_first_invalid(&bytes[..split], 0, &mut carry, false);
                        if found.is_none() {
                            found = utf8_first_invalid(&bytes[split..], split, &mut carry, true);
                        }
                        assert_eq!(
                            found, expected,
                            "lead {lead:#x} at {lead_pos} with {supplied} continuations, \
                             split at {split}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn utf8_kernel_agrees_with_from_utf8_across_several_lane_boundaries() {
        // The existing corpora stop short of 40 bytes, so a mask that is wrong only at a lane boundary can hide. These
        // buffers span three lanes and place high bytes densely enough that leads land on every in-lane position.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 40..96 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                let r = mix(&mut state);
                bytes.push(if r % 3 == 0 {
                    b'a'
                } else {
                    HIGH[((r >> 16) & 7) as usize]
                });
            }
            assert_eq!(
                whole_scan(&bytes),
                oracle(&bytes),
                "lane-crossing corpus len {len}: {bytes:?}"
            );
            for split in [0, 1, 15, 16, 17, 31, 32, 33, len] {
                let expected = oracle(&bytes);
                let mut carry = Utf8Carry::EMPTY;
                let mut found = utf8_first_invalid(&bytes[..split], 0, &mut carry, false);
                if found.is_none() {
                    found = utf8_first_invalid(&bytes[split..], split, &mut carry, true);
                }
                assert_eq!(
                    found, expected,
                    "lane-crossing corpus len {len} split {split}: {bytes:?}"
                );
            }
        }
    }

    #[test]
    fn utf8_kernel_truncated_lead_is_carried_and_reported_at_its_lead() {
        let lead_pos = 5;
        for (lead, conts, complete, expect_error) in [
            // (lead, continuations in window 2, bytes that complete it, whether it is still truncated)
            (0xC2, &[0x80][..], &[][..], false),
            (0xE2, &[0x82][..], &[0xAC][..], true),
            (0xE2, &[0x82, 0xAC][..], &[][..], false),
            (0xF0, &[0x90, 0x80][..], &[0x80][..], true),
        ] {
            let mut bytes = vec![b'a'; lead_pos];
            bytes.push(lead);
            bytes.extend_from_slice(conts);
            // Split so the lead lands in the first (non-final) window; the continuations land in the second.
            let split = lead_pos + 1;
            let mut carry = Utf8Carry::EMPTY;
            assert_eq!(
                utf8_first_invalid(&bytes[..split], 0, &mut carry, false),
                None,
                "truncated lead {lead:#x} must carry"
            );
            assert_eq!(usize::from(carry.len), 1, "carry must hold just the lead");
            assert_eq!(carry.bytes[0], lead);
            assert_eq!(
                utf8_first_invalid(&bytes[split..], split, &mut carry, true),
                if expect_error { Some(lead_pos) } else { None },
                "lead {lead:#x} over conts {conts:?} on the final chunk"
            );
            // The same bytes with the sequence completed are valid.
            let mut done = vec![b'a'; lead_pos];
            done.push(lead);
            done.extend_from_slice(conts);
            done.extend_from_slice(complete);
            let mut carry = Utf8Carry::EMPTY;
            assert_eq!(utf8_first_invalid(&done[..split], 0, &mut carry, false), None);
            assert_eq!(utf8_first_invalid(&done[split..], split, &mut carry, true), None);
        }
    }

    /// The alignment oracle for the seq framing stop set this file owns: the wide kernel must agree with the scalar
    /// predicate at every alignment and length, so a wrong kernel is a test failure here.
    #[test]
    fn rs_prefix_agrees_with_the_scalar_predicate_at_every_alignment() {
        let mut corpus: Vec<Vec<u8>> = vec![Vec::new(), b"\x1e".to_vec(), b"a\x1eb".to_vec(), b"a b\n".to_vec()];
        let mut state = 0x5eed_5eed_5eed_5eed_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for len in 0..48 {
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                let r = mix(&mut state);
                bytes.push(if r % 4 == 0 { 0x1e } else { (r & 0xFF) as u8 });
            }
            corpus.push(bytes);
        }
        for bytes in &corpus {
            for start in 0..=bytes.len().min(3) {
                for end in start..=bytes.len().min(start + 48) {
                    let slice = &bytes[start..end];
                    assert_eq!(
                        prefix_len::<Rs>(slice),
                        slice.iter().take_while(|b| !Rs::stop(**b)).count(),
                        "Rs mismatch at {start}..{end} of {bytes:?}"
                    );
                }
            }
        }
    }
}
