//! Stop-set longest-prefix scan and UTF-8 lane kernels.
//!
//! [`prefix_len`] is the longest prefix containing no byte of a compile-time [`StopSet`], monomorphized per set. UTF-8
//! *lane* kernels live here; the windowed walk lives with the JSON crate.
//!
//! # Unsafe
//!
//! This module holds the NEON/SSE2/AVX2 kernels. Every `unsafe` block has a
//! SAFETY note; each lane loop checks `len - offset >= W` before the load.
//! Other modules also contain `unsafe` (`product` source attach, `erased` fallible box).

#![allow(unsafe_code)]

/// A compile-time stop set: the bytes at which a [`prefix_len`] scan must halt. Zero-sized; every field is an
/// associated constant, so each specialization folds the intrinsic comparison chain at compile time.
///
/// # Law
///
/// A scan stops at exactly the first byte `b` with [`Self::hit`]`(b) == true` and admits every byte before it. The
/// module's generic alignment oracle instantiates this for every declared set, so a wrong kernel is a test failure,
/// never a silent wrong answer.
pub trait StopSet: Copy {
    /// Exact-match stop bytes. The first [`Self::EQ_LEN`] entries are live; the rest are ignored (a fixed-size array
    /// keeps the descriptor a plain compile-time constant).
    const EQ: [u8; 8];
    /// Number of live entries in [`Self::EQ`].
    const EQ_LEN: u8;
    /// Halt on `byte < LT` (`None`: no lower bound). The C0-control shape is `Some(0x20)`.
    const LT: Option<u8>;
    /// Halt on `byte >= GE` (`None`: no upper bound). The non-ASCII shape is `Some(0x80)`.
    const GE: Option<u8>;
    /// True for an all-in-set run (the whitespace shape): a lane is clean iff EVERY byte is one of [`Self::EQ`]. The
    /// lane check then uses the min/max reduction instead of equality enumeration, because the complement (the real
    /// stop set) is not enumerable.
    const ALL: bool;

    /// Whether `byte` is in the stop set — the scalar predicate that is the ground truth for the wide kernels and the
    /// alignment oracle.
    #[must_use]
    #[expect(
        clippy::inline_always,
        reason = "the scalar predicate must fold into the monomorphized scan tails exactly as \
                  the hand-written const predicates did; a non-inlined call per byte would \
                  change the generated code"
    )]
    #[inline(always)]
    fn hit(byte: u8) -> bool {
        let mut i = 0;
        while i < Self::EQ_LEN {
            if Self::EQ[i as usize] == byte {
                return true;
            }
            i += 1;
        }
        if let Some(lt) = Self::LT
            && byte < lt
        {
            return true;
        }
        if let Some(ge) = Self::GE
            && byte >= ge
        {
            return true;
        }
        false
    }

    /// Whether a scan must STOP at `byte`: the scalar predicate with the all-in-set polarity applied. For an ordinary
    /// stop set this is [`Self::hit`]; for an all-in-set run (whitespace) the stop set is the COMPLEMENT of `EQ`, so
    /// the scan stops at the first byte that is NOT one of the run's bytes.
    #[must_use]
    #[expect(
        clippy::inline_always,
        reason = "see `hit`: the polarity wrapper must fold into the scan tails too"
    )]
    #[inline(always)]
    fn stop(byte: u8) -> bool {
        if Self::ALL { !Self::hit(byte) } else { Self::hit(byte) }
    }
}

/// The JSON text-escape set: `"`, `\`, C0 controls (`< 0x20`), DEL (`0x7f`). TOML basic strings need exactly this set
/// (the TOML spec escapes DEL the way JSON does), and jqft's JSON-shaped strings do too.
#[derive(Clone, Copy)]
pub struct Escape;
impl StopSet for Escape {
    const EQ: [u8; 8] = [b'"', b'\\', 0x7f, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 3;
    const LT: Option<u8> = Some(0x20);
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// TOML basic strings: JSON's escape set exactly, so this is [`Escape`] under the name the TOML grammar reads by.
pub use Escape as TomlBasicString;

/// The decode-side plain JSON string run: `"`, `\`, C0 controls, DEL (`0x7f`), or any byte `>= 0x80`. Unlike
/// [`StringContent`], non-ASCII bytes are NOT content here — the run it delimits is the copy-safe plain-string
/// region. DEL is a stop so the decoder's canonicality probe can observe it at the run boundary instead of memchr-ing
/// every run.
#[derive(Clone, Copy)]
pub struct PlainString;
impl StopSet for PlainString {
    const EQ: [u8; 8] = [b'"', b'\\', 0x7f, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 3;
    const LT: Option<u8> = Some(0x20);
    const GE: Option<u8> = Some(0x80);
    const ALL: bool = false;
}

/// The decode-side JSON string CONTENT run: `"`, `\`, a C0 control, or DEL (`0x7f`). Non-ASCII bytes are content —
/// the block this delimits is what the block UTF-8 validator checks. DEL is a stop so a unicode block does not need a
/// second memchr for the canonicality probe.
#[derive(Clone, Copy)]
pub struct StringContent;
impl StopSet for StringContent {
    const EQ: [u8; 8] = [b'"', b'\\', 0x7f, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 3;
    const LT: Option<u8> = Some(0x20);
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// The container-structural bytes: `"`, `{`, `[`, `}`, `]`. Everything else — whitespace, scalars, punctuation — is
/// noise a container skip can pass in bulk.
#[derive(Clone, Copy)]
pub struct Structural;
impl StopSet for Structural {
    const EQ: [u8; 8] = [b'"', b'{', b'[', b'}', b']', 0, 0, 0];
    const EQ_LEN: u8 = 5;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// The bare-word terminators: JSON whitespace plus `,`, `]`, `}`. A word (`true`, `123`, `null`) is passed whole and
/// the skip stops exactly at the delimiter that ends it.
#[derive(Clone, Copy)]
pub struct Delimiter;
impl StopSet for Delimiter {
    const EQ: [u8; 8] = [b' ', b'\t', b'\n', b'\r', b',', b']', b'}', 0];
    const EQ_LEN: u8 = 7;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// JSON structural whitespace (RFC 8259: space, tab, LF, CR) as an all-in-set run — the whitespace shape. jqft's
/// whitespace is byte-identical to this set.
#[derive(Clone, Copy)]
pub struct Ws;
impl StopSet for Ws {
    const EQ: [u8; 8] = [b' ', b'\t', b'\n', b'\r', 0, 0, 0, 0];
    const EQ_LEN: u8 = 4;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = true;
}

/// The NDJSON physical record terminators: `\n` and `\r`. A JSON string cannot contain either raw, so a record boundary
/// needs no quote state.
#[derive(Clone, Copy)]
pub struct NdjsonFrame;
impl StopSet for NdjsonFrame {
    const EQ: [u8; 8] = [b'\n', b'\r', 0, 0, 0, 0, 0, 0];
    const EQ_LEN: u8 = 2;
    const LT: Option<u8> = None;
    const GE: Option<u8> = None;
    const ALL: bool = false;
}

/// Longest prefix of `bytes` containing no byte of stop set `S`: the whole 16/32-byte lanes through the arch kernel,
/// which returns the exact first hit when a lane contains one, then a scalar tail over only the leftover that is
/// shorter than a lane. There is deliberately no scalar head — a head is a per-call-site short-run choice, and the
/// callers that measure one (JSON's whitespace, structural, and delimiter scans) run it themselves before this.
#[must_use]
pub fn prefix_len<S: StopSet>(bytes: &[u8]) -> usize {
    let wide = {
        #[cfg(target_arch = "aarch64")]
        {
            // SAFETY: AArch64 guarantees NEON and the kernel checks every load.
            unsafe { aarch64::wide::<S>(bytes.as_ptr(), bytes.len()) }
        }
        #[cfg(target_arch = "x86_64")]
        {
            if x86_64::avx2() {
                // SAFETY: the AVX2 kernel requires the feature `avx2()` just verified, and it checks every load.
                unsafe { x86_64::avx2::wide::<S>(bytes.as_ptr(), bytes.len()) }
            } else {
                // SAFETY: x86-64 guarantees SSE2 and the kernel checks every load.
                unsafe { x86_64::sse2::wide::<S>(bytes.as_ptr(), bytes.len()) }
            }
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        {
            0
        }
    };
    // A hitting wide lane already named the exact stop byte. The leftover shorter than a lane is the only region that
    // still needs a scalar walk.
    if wide == bytes.len() || S::stop(bytes[wide]) {
        wide
    } else {
        wide + bytes[wide..].iter().take_while(|byte| !S::stop(**byte)).count()
    }
}

/// The 16-byte lanes of the UTF-8 scan see continuity only WITHIN a lane: the mask shifts cannot look before position
/// 0. This builds the first three positions' required-continuation mask from the previous lane's last three bytes,
///   using the same rule the in-lane shifts encode — position `i` must be a continuation when a 2/3/4-byte lead sits at
///   `i - 1`, a 3/4-byte lead at `i - 2`, or a 4-byte lead at `i - 3` — over the same coarse lead ranges the SIMD masks
///   classify, so the XOR against `is_cont` stays consistent.
///
/// Resolving `i - 1`, `i - 2`, `i - 3` against the previous lane's positions (new position 0 follows `p15`) gives
/// exactly:
///
/// | new position | `i - 1` | `i - 2` | `i - 3` | boundary term                       |
/// |--------------|---------|---------|---------|-------------------------------------|
/// | 0            | `p15`   | `p14`   | `p13`   | `lead234(p15) \| lead34(p14) \| lead4(p13)` |
/// | 1            | pos 0   | `p15`   | `p14`   | `lead34(p15) \| lead4(p14)`         |
/// | 2            | pos 1   | pos 0   | `p15`   | `lead4(p15)`                        |
///
/// The `i - 1` slots for positions 1 and 2 are in-lane, so the shifted masks already cover them; only the terms that
/// reach back past position 0 belong here. A 3-byte lead in `p15` therefore demands continuations at BOTH new positions
/// 0 and 1, and a 4-byte lead in `p15` at 0, 1 and 2.
#[expect(
    clippy::similar_names,
    reason = "lead234/lead34/lead4 are the UTF-8 lead classes; the digits ARE the meaning"
)]
fn boundary_cont_mask(prev16: &[u8; 16]) -> [u8; 16] {
    let mut mask = [0_u8; 16];
    let p13 = prev16[13];
    let p14 = prev16[14];
    let p15 = prev16[15];
    let lead234 = |byte: u8| (0xC0..=0xF7).contains(&byte);
    let lead34 = |byte: u8| (0xE0..=0xF7).contains(&byte);
    let lead4 = |byte: u8| (0xF0..=0xF7).contains(&byte);
    if lead234(p15) || lead34(p14) || lead4(p13) {
        mask[0] = 0xFF;
    }
    if lead34(p15) || lead4(p14) {
        mask[1] = 0xFF;
    }
    if lead4(p15) {
        mask[2] = 0xFF;
    }
    mask
}

/// NEON kernels. Public only so the windowed first-invalid UTF-8 scan can live beside its single consumer while the
/// hand-written kernels stay here; the scan surface itself is [`prefix_len`] and the lane fns below, not this module's
/// shape.
#[cfg(target_arch = "aarch64")]
pub mod aarch64 {
    use core::arch::aarch64::{
        uint8x16_t, vandq_u8, vbslq_u8, vceqq_u8, vcgeq_u8, vcltq_u8, vdupq_n_u8, veorq_u8, vextq_u8, vld1q_u8,
        vmaxvq_u8, vminvq_u8, vmvnq_u8, vorrq_u8,
    };

    const LANE_INDEX: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    /// First byte of a 16-byte lane that is in stop set `S`, or `None` when the lane is clean. For [`StopSet::ALL`] the
    /// hit is the first byte *not* in the set — the same polarity [`super::StopSet::stop`] uses. The comparison chain
    /// is built from `S`'s associated constants; the hit index is `vbsl` of the index vector against a 16-sentinel,
    /// then `vminvq`.
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn first_hit<S: super::StopSet>(lane: &[u8; 16]) -> Option<usize> {
        // SAFETY: the caller guarantees 16 readable bytes.
        let v = unsafe { vld1q_u8(lane.as_ptr()) };
        let mut exceptional = vdupq_n_u8(0);
        let mut i = 0;
        while i < S::EQ_LEN {
            exceptional = vorrq_u8(exceptional, vceqq_u8(v, vdupq_n_u8(S::EQ[i as usize])));
            i += 1;
        }
        if let Some(lt) = S::LT {
            exceptional = vorrq_u8(exceptional, vcltq_u8(v, vdupq_n_u8(lt)));
        }
        if let Some(ge) = S::GE {
            // `ge - 1` on the left reproduces the hand-written `0x7f < v` spelling of the non-ASCII shape. A set
            // declaring `GE = Some(0)` would underflow to `0xFF` and silently match nothing — the contract is GE >=
            // 1, which every current set satisfies.
            debug_assert!(ge >= 1);
            exceptional = vorrq_u8(exceptional, vcltq_u8(vdupq_n_u8(ge - 1), v));
        }
        // All-in-set (whitespace): a hit is a byte that missed the equality chain. Invert the mask so the select/min
        // below sees 0xFF at those positions, same as the ordinary stop-set polarity.
        let hit = if S::ALL { vmvnq_u8(exceptional) } else { exceptional };
        // SAFETY: `LANE_INDEX` is 16 live bytes.
        let indices = unsafe { vld1q_u8(LANE_INDEX.as_ptr()) };
        let selected = vbslq_u8(hit, indices, vdupq_n_u8(16));
        let first = vminvq_u8(selected);
        (first < 16).then_some(usize::from(first))
    }

    /// Longest prefix containing no byte of `S`, over whole 16-byte lanes. A hitting lane returns `offset +
    /// first_hit_index`; a leftover shorter than a lane is left to the caller's scalar tail.
    #[target_feature(enable = "neon")]
    pub(crate) unsafe fn wide<S: super::StopSet>(pointer: *const u8, len: usize) -> usize {
        let mut offset = 0_usize;
        while len - offset >= 16 {
            // SAFETY: the loop condition proves 16 readable bytes at `offset`.
            let lane = unsafe { &*pointer.add(offset).cast::<[u8; 16]>() };
            if let Some(hit) = unsafe { first_hit::<S>(lane) } {
                return offset + hit;
            }
            offset += 16;
        }
        offset
    }

    /// Whether any byte of `v` starts or continues an invalid UTF-8 sequence, given the previous 16 bytes (`prev16`)
    /// and the next 16 (`next16`, zero padded past the end — safe because 0x00 is never a valid continuation).
    ///
    /// The mask is the union of: continuity errors (a continuation where none is required, or a required continuation
    /// missing), invalid lead bytes (C0/C1, F5..=FF), and the second-byte range laws for E0/ED/F0/F4.
    ///
    /// # Safety
    ///
    /// NEON is an `AArch64` guarantee; the caller bounds every load.
    #[expect(
        clippy::similar_names,
        reason = "e0_bad/ed_bad/f0_bad/f4_bad are named for the hex lead bytes whose second-byte law they check"
    )]
    #[target_feature(enable = "neon")]
    #[must_use]
    pub unsafe fn lane_has_invalid(v: &[u8; 16], prev16: &[u8; 16], next16: &[u8; 16]) -> bool {
        let v = load(v);
        let boundary = load(&super::boundary_cont_mask(prev16));
        let next16 = load(next16);
        let is_cont = vceqq_u8(vandq_u8(v, vdupq_n_u8(0xC0)), vdupq_n_u8(0x80));
        let is_lead2 = vceqq_u8(vandq_u8(v, vdupq_n_u8(0xE0)), vdupq_n_u8(0xC0));
        let is_lead3 = vceqq_u8(vandq_u8(v, vdupq_n_u8(0xF0)), vdupq_n_u8(0xE0));
        let is_lead4 = vceqq_u8(vandq_u8(v, vdupq_n_u8(0xF8)), vdupq_n_u8(0xF0));
        let zero = vdupq_n_u8(0);
        // A byte at position i must be a continuation when a 2/3/4-byte lead sits one position before it, a 3/4-byte
        // lead two before, or a 4-byte lead three before. `vextq(zero, mask, n)` right-shifts a mask's set positions by
        // `16 - n`: n = 15 shifts 1, n = 14 shifts 2, n = 13 shifts 3.
        let must_cont = vorrq_u8(
            vorrq_u8(vextq_u8(zero, is_lead2, 15), vextq_u8(zero, is_lead3, 15)),
            vextq_u8(zero, is_lead4, 15),
        );
        let must_cont = vorrq_u8(
            must_cont,
            vorrq_u8(vextq_u8(zero, is_lead3, 14), vextq_u8(zero, is_lead4, 14)),
        );
        let must_cont = vorrq_u8(must_cont, vextq_u8(zero, is_lead4, 13));
        let must_cont = vorrq_u8(must_cont, boundary);
        let bad_cont = veorq_u8(is_cont, must_cont);
        let invalid_lead = vorrq_u8(
            vceqq_u8(vandq_u8(v, vdupq_n_u8(0xFE)), vdupq_n_u8(0xC0)),
            vcgeq_u8(v, vdupq_n_u8(0xF5)),
        );
        // `vextq(v, next16, 1)` puts the NEXT byte's value at each position.
        let next1 = vextq_u8(v, next16, 1);
        let e0_bad = vandq_u8(vceqq_u8(v, vdupq_n_u8(0xE0)), vcltq_u8(next1, vdupq_n_u8(0xA0)));
        let ed_bad = vandq_u8(vceqq_u8(v, vdupq_n_u8(0xED)), vcgeq_u8(next1, vdupq_n_u8(0xA0)));
        let f0_bad = vandq_u8(vceqq_u8(v, vdupq_n_u8(0xF0)), vcltq_u8(next1, vdupq_n_u8(0x90)));
        let f4_bad = vandq_u8(vceqq_u8(v, vdupq_n_u8(0xF4)), vcgeq_u8(next1, vdupq_n_u8(0x90)));
        let bad = vorrq_u8(
            vorrq_u8(bad_cont, invalid_lead),
            vorrq_u8(vorrq_u8(e0_bad, ed_bad), vorrq_u8(f0_bad, f4_bad)),
        );
        vmaxvq_u8(bad) != 0
    }

    /// Whether the lane can be skipped by the UTF-8 walk: every byte is ASCII and the previous lane's trailing bytes
    /// demand no continuation inside this one. Both conditions together make every error class impossible.
    ///
    /// # Safety
    ///
    /// NEON is an `AArch64` guarantee; the caller bounds every load.
    #[target_feature(enable = "neon")]
    #[must_use]
    pub unsafe fn lane_ascii_clean(v: &[u8; 16], prev16: &[u8; 16]) -> bool {
        let v = load(v);
        let boundary = load(&super::boundary_cont_mask(prev16));
        vmaxvq_u8(vcgeq_u8(v, vdupq_n_u8(0x80))) == 0 && vmaxvq_u8(boundary) == 0
    }

    fn load(lane: &[u8; 16]) -> uint8x16_t {
        // SAFETY: the caller guarantees 16 readable bytes.
        unsafe { vld1q_u8(lane.as_ptr()) }
    }
}

/// SSE2/AVX2 kernels. Public only so the windowed first-invalid UTF-8 scan can live beside its single consumer while
/// the hand-written kernels stay here; the scan surface itself is [`prefix_len`] and the lane fns below, not this
/// module's shape.
#[cfg(target_arch = "x86_64")]
pub mod x86_64 {
    use core::sync::atomic::{AtomicU8, Ordering};

    /// Whether the current CPU exposes AVX2, probed once and cached. This is the runtime-dispatch half: the wider
    /// 256-bit kernels run when the CPU has AVX2, the SSE2 baseline otherwise, byte-identical on both. A stale negative
    /// is impossible (a CPU that gains AVX2 is not a thing), so the cache never re-probes.
    pub fn avx2() -> bool {
        match AVX2.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let detected = detect_avx2();
                AVX2.store(if detected { 1 } else { 2 }, Ordering::Relaxed);
                detected
            }
        }
    }

    static AVX2: AtomicU8 = AtomicU8::new(0);

    /// CPUID- and XGETBV-based AVX2 detection, replicating the checks behind `is_x86_feature_detected!("avx2")` (that
    /// macro is std-only, and this crate is `no_std`): OSXSAVE, the AVX CPUID bit, the XMM/YMM state the OS actually
    /// saves (XCR0), and AVX2's own CPUID leaf 7 bit. The leaf-7 query is gated on the maximum standard leaf: a CPU
    /// without leaf 7 echoes undefined data in EBX, and bit 5 of garbage would dispatch AVX2 code on a machine that
    /// cannot execute it.
    fn detect_avx2() -> bool {
        let basic = core::arch::x86_64::__cpuid(1);
        let osxsave = (basic.ecx & (1 << 27)) != 0;
        let avx = (basic.ecx & (1 << 28)) != 0;
        if !(osxsave && avx) {
            return false;
        }
        // SAFETY: XGETBV is safe to execute whenever OSXSAVE is set, which is exactly the guard checked above.
        let xcr0 = unsafe { core::arch::x86_64::_xgetbv(0) };
        if xcr0 & 0x6 != 0x6 {
            return false;
        }
        // Leaf 7 exists only when the max standard leaf reports it; std's replicated detector checks this before
        // querying, and so does this. (__cpuid(1).eax is the version signature, not the max leaf.)
        let max_leaf = core::arch::x86_64::__cpuid(0).eax;
        if max_leaf < 7 {
            return false;
        }
        let extended = core::arch::x86_64::__cpuid(7);
        (extended.ebx & (1 << 5)) != 0
    }

    /// The 128-bit baseline kernel family (SSE2 is an x86-64 guarantee).
    pub mod sse2 {
        #![expect(
            clippy::cast_ptr_alignment,
            reason = "every pointer cast here feeds `_mm_loadu_si128`, whose load has no alignment precondition"
        )]

        use core::arch::x86_64::{
            __m128i, _mm_and_si128, _mm_cmpeq_epi8, _mm_cmpgt_epi8, _mm_loadu_si128, _mm_movemask_epi8, _mm_or_si128,
            _mm_set1_epi8, _mm_setzero_si128, _mm_slli_si128, _mm_srli_si128, _mm_xor_si128,
        };

        const SIGN: i8 = i8::MIN; // 0x80

        /// Unsigned `v < c`, via the xor-sign trick (SSE2's comparison is signed).
        ///
        /// The constant is the LEFT operand: `_mm_cmpgt_epi8(a, b)` is `a > b`, so `c ^ 0x80 > v ^ 0x80` is unsigned `c
        /// > v`, i.e. `v < c`. Passing `v` first would compute `v > c` instead — the same two instructions, the
        /// opposite predicate.
        unsafe fn lt_u(v: __m128i, c: u8) -> __m128i {
            // SAFETY: SSE2 is guaranteed by the x86-64 baseline this module is
            // `cfg`-gated on, so these intrinsics are always available; they are
            // pure register operations and touch no memory.
            unsafe {
                _mm_cmpgt_epi8(
                    _mm_set1_epi8((c ^ 0x80).cast_signed()),
                    _mm_xor_si128(v, _mm_set1_epi8(SIGN)),
                )
            }
        }

        /// Unsigned `v >= c`.
        unsafe fn ge_u(v: __m128i, c: u8) -> __m128i {
            // SAFETY: `lt_u` needs only the SSE2 baseline this module is gated on,
            // and takes register values with no further precondition.
            let lt = unsafe { lt_u(v, c) };
            // SAFETY: SSE2 baseline as above; pure register operations.
            unsafe { _mm_cmpeq_epi8(lt, _mm_setzero_si128()) }
        }

        /// First byte of a 16-byte lane that is in stop set `S`, or `None` when the lane is clean. For
        /// [`super::super::StopSet::ALL`] the hit is the first byte *not* in the set. The comparison chain folds from
        /// `S`'s constants; the `(0x20, 0x80)` plain-string pair uses the single signed compare whose negative lanes
        /// ARE the `>= 0x80` bytes. The hit index is `movemask` plus `trailing_zeros`.
        #[expect(
            clippy::inline_always,
            reason = "the fixed 16-byte lane kernel must fold into the scan loop; the lint's \
                      general size heuristic does not apply to the fixed-width SIMD compare chain"
        )]
        #[inline(always)]
        pub(crate) unsafe fn first_hit<S: super::super::StopSet>(lane: &[u8; 16]) -> Option<usize> {
            // SAFETY: the caller guarantees 16 readable bytes.
            let v = unsafe { _mm_loadu_si128(lane.as_ptr().cast::<__m128i>()) };
            // SAFETY: intrinsic calls are unsafe operations; the lane pointer
            // is valid and the surrounding kernels guarantee the loads.
            let mut exceptional = unsafe { _mm_setzero_si128() };
            let mut i = 0;
            while i < S::EQ_LEN {
                exceptional = unsafe {
                    _mm_or_si128(
                        exceptional,
                        _mm_cmpeq_epi8(v, _mm_set1_epi8(S::EQ[i as usize].cast_signed())),
                    )
                };
                i += 1;
            }
            let range = match (S::LT, S::GE) {
                (Some(0x20), Some(0x80)) => {
                    // Signed negative lanes are non-ASCII and compare below 0x20.
                    unsafe { _mm_cmpgt_epi8(_mm_set1_epi8(0x20), v) }
                }
                (Some(lt), None) => unsafe { lt_u(v, lt) },
                (None, Some(ge)) => unsafe { ge_u(v, ge) },
                (Some(lt), Some(ge)) => unsafe { _mm_or_si128(lt_u(v, lt), ge_u(v, ge)) },
                (None, None) => unsafe { _mm_setzero_si128() },
            };
            exceptional = unsafe { _mm_or_si128(exceptional, range) };
            // `_mm_movemask_epi8` sets only the low 16 bits, one per lane.
            let mask = unsafe { _mm_movemask_epi8(exceptional) } as u32;
            let bits = if S::ALL {
                // All-in-set: the mask is 0xFFFF when every byte is in the set; a hit is the first clear bit.
                (!mask) & 0xFFFF
            } else {
                mask
            };
            (bits != 0).then_some(bits.trailing_zeros() as usize)
        }

        /// Longest prefix containing no byte of `S`, over whole 16-byte lanes. A hitting lane returns `offset +
        /// first_hit_index`; a leftover shorter than a lane is left to the caller's scalar tail.
        #[expect(
            clippy::inline_always,
            reason = "the fixed-width lane walk must fold into the scan loop; a non-inlined call \
                      per lane would change the generated code (see `first_hit`)"
        )]
        #[inline(always)]
        pub(crate) unsafe fn wide<S: super::super::StopSet>(pointer: *const u8, len: usize) -> usize {
            let mut offset = 0_usize;
            while len - offset >= 16 {
                // SAFETY: the loop condition proves 16 readable bytes at `offset`.
                let lane = unsafe { &*pointer.add(offset).cast::<[u8; 16]>() };
                if let Some(hit) = unsafe { first_hit::<S>(lane) } {
                    return offset + hit;
                }
                offset += 16;
            }
            offset
        }

        /// Whether the lane can be skipped by the UTF-8 walk: every byte is ASCII and the previous lane's trailing
        /// bytes demand no continuation inside this one. Both conditions together make every error class impossible.
        ///
        /// # Safety
        ///
        /// SSE2 is an x86-64 guarantee; the caller bounds every load.
        #[must_use]
        pub unsafe fn lane_ascii_clean(v: &[u8; 16], prev16: &[u8; 16]) -> bool {
            let v = load(v);
            let boundary = load(&super::super::boundary_cont_mask(prev16));
            // SAFETY: SSE2 baseline as above; `ge_u` and the masks are register
            // operations over lanes `load` already built from `&[u8; 16]`.
            unsafe { (_mm_movemask_epi8(ge_u(v, 0x80)) | _mm_movemask_epi8(boundary)) == 0 }
        }

        /// Whether any byte of `v` starts or continues an invalid UTF-8 sequence, given the previous 16 bytes and the
        /// next 16 (zero padded past the end).
        ///
        /// # Safety
        ///
        /// SSE2 is an x86-64 guarantee; the caller bounds every load.
        #[expect(
            clippy::similar_names,
            reason = "e0_bad/ed_bad/f0_bad/f4_bad are named for the hex lead bytes whose second-byte law they check"
        )]
        #[must_use]
        pub unsafe fn lane_has_invalid(v: &[u8; 16], prev16: &[u8; 16], next16: &[u8; 16]) -> bool {
            let v = load(v);
            let boundary = load(&super::super::boundary_cont_mask(prev16));
            let next16 = load(next16);
            // SAFETY: SSE2 baseline as above; every intrinsic below is a pure
            // register operation over lanes `load` already built from `&[u8; 16]`, so no memory is touched here.
            unsafe {
                let is_cont = _mm_cmpeq_epi8(
                    _mm_and_si128(v, _mm_set1_epi8(0xC0_u8.cast_signed())),
                    _mm_set1_epi8(0x80_u8.cast_signed()),
                );
                let is_lead2 = _mm_cmpeq_epi8(
                    _mm_and_si128(v, _mm_set1_epi8(0xE0_u8.cast_signed())),
                    _mm_set1_epi8(0xC0_u8.cast_signed()),
                );
                let is_lead3 = _mm_cmpeq_epi8(
                    _mm_and_si128(v, _mm_set1_epi8(0xF0_u8.cast_signed())),
                    _mm_set1_epi8(0xE0_u8.cast_signed()),
                );
                let is_lead4 = _mm_cmpeq_epi8(
                    _mm_and_si128(v, _mm_set1_epi8(0xF8_u8.cast_signed())),
                    _mm_set1_epi8(0xF0_u8.cast_signed()),
                );
                // A byte at position i must be a continuation when a 2/3/4-byte lead sits one position before it, a
                // 3/4-byte lead two before, or a 4-byte lead three before. The lane is little-endian, so byte i is bits
                // `8i..8i+8`: `_mm_slli_si128(mask, n)` is the LOOK-BACK (`dst[i] = mask[i - n]`, zeros below n),
                // matching the NEON `vextq_u8(zero, mask, 16 - n)`. `_mm_srli_si128` is the opposite direction — a
                // lookahead — and would ask position i about the lead at i + n.
                let must_cont = _mm_or_si128(
                    _mm_or_si128(_mm_slli_si128(is_lead2, 1), _mm_slli_si128(is_lead3, 1)),
                    _mm_slli_si128(is_lead4, 1),
                );
                let must_cont = _mm_or_si128(
                    must_cont,
                    _mm_or_si128(_mm_slli_si128(is_lead3, 2), _mm_slli_si128(is_lead4, 2)),
                );
                let must_cont = _mm_or_si128(must_cont, _mm_slli_si128(is_lead4, 3));
                let must_cont = _mm_or_si128(must_cont, boundary);
                let bad_cont = _mm_xor_si128(is_cont, must_cont);
                let invalid_lead = _mm_or_si128(
                    _mm_cmpeq_epi8(
                        _mm_and_si128(v, _mm_set1_epi8(0xFE_u8.cast_signed())),
                        _mm_set1_epi8(0xC0_u8.cast_signed()),
                    ),
                    ge_u(v, 0xF5),
                );
                // The lookahead is the mirror of `must_cont`'s look-back: `srli(v, 1)` puts the NEXT byte's value at
                // each position except the last, and `slli(next16, 15)` fills that last position with `next16`'s first
                // byte.
                let n1 = _mm_or_si128(_mm_srli_si128(v, 1), _mm_slli_si128(next16, 15));
                let e0_bad = _mm_and_si128(_mm_cmpeq_epi8(v, _mm_set1_epi8(0xE0_u8.cast_signed())), lt_u(n1, 0xA0));
                let ed_bad = _mm_and_si128(_mm_cmpeq_epi8(v, _mm_set1_epi8(0xED_u8.cast_signed())), ge_u(n1, 0xA0));
                let f0_bad = _mm_and_si128(_mm_cmpeq_epi8(v, _mm_set1_epi8(0xF0_u8.cast_signed())), lt_u(n1, 0x90));
                let f4_bad = _mm_and_si128(_mm_cmpeq_epi8(v, _mm_set1_epi8(0xF4_u8.cast_signed())), ge_u(n1, 0x90));
                let bad = _mm_or_si128(
                    _mm_or_si128(bad_cont, invalid_lead),
                    _mm_or_si128(_mm_or_si128(e0_bad, ed_bad), _mm_or_si128(f0_bad, f4_bad)),
                );
                _mm_movemask_epi8(bad) != 0
            }
        }

        fn load(lane: &[u8; 16]) -> __m128i {
            // SAFETY: the caller guarantees 16 readable bytes.
            unsafe { _mm_loadu_si128(lane.as_ptr().cast::<__m128i>()) }
        }
    }

    /// The 256-bit kernel family behind the runtime `avx2()` probe.
    pub mod avx2 {
        #![expect(
            clippy::cast_ptr_alignment,
            reason = "every pointer cast here feeds `_mm256_loadu_si256`, whose load has no alignment precondition"
        )]

        use core::arch::x86_64::{
            __m256i, _mm256_and_si256, _mm256_cmpeq_epi8, _mm256_cmpgt_epi8, _mm256_loadu_si256, _mm256_movemask_epi8,
            _mm256_or_si256, _mm256_permute2x128_si256, _mm256_set1_epi8, _mm256_setzero_si256, _mm256_slli_si256,
            _mm256_srli_si256, _mm256_xor_si256,
        };

        const SIGN: i8 = i8::MIN; // 0x80

        /// Unsigned `v < c`, via the xor-sign trick (AVX2's byte compare is signed, exactly like SSE2's). See the SSE2
        /// [`sse2::lt_u`] twin.
        unsafe fn lt_u(v: __m256i, c: u8) -> __m256i {
            // SAFETY: called from `#[target_feature(enable = "avx2")]` kernels
            // only; pure register operations with no memory touched.
            unsafe {
                _mm256_cmpgt_epi8(
                    _mm256_set1_epi8((c ^ 0x80).cast_signed()),
                    _mm256_xor_si256(v, _mm256_set1_epi8(SIGN)),
                )
            }
        }

        /// Unsigned `v >= c`.
        unsafe fn ge_u(v: __m256i, c: u8) -> __m256i {
            let lt = unsafe { lt_u(v, c) };
            // SAFETY: register-only; `lt_u` already holds under the same target-feature precondition.
            unsafe { _mm256_cmpeq_epi8(lt, _mm256_setzero_si256()) }
        }

        /// First byte of a 32-byte lane that is in stop set `S`, or `None` when the lane is clean. For
        /// [`super::super::StopSet::ALL`] the hit is the first byte *not* in the set. The comparison chain folds from
        /// `S`'s constants; the `(0x20, 0x80)` plain-string pair uses the single signed compare, exactly like the SSE2
        /// kernel. The hit index is `movemask` plus `trailing_zeros`.
        #[target_feature(enable = "avx2")]
        #[allow(
            unused_unsafe,
            reason = "rustc 1.96 marks the x86_64 intrinsics safe on linux-gnu (the docker \
                      battery compiles them) while they stay unsafe on windows-msvc and older \
                      rustc; the wrappers are vestigial where the intrinsics are safe, so the \
                      lint fires on one target and not the other and cannot be `expect`ed"
        )]
        pub(crate) unsafe fn first_hit<S: super::super::StopSet>(lane: &[u8; 32]) -> Option<usize> {
            // SAFETY: the caller guarantees 32 readable bytes.
            let v = unsafe { _mm256_loadu_si256(lane.as_ptr().cast::<__m256i>()) };
            let mut exceptional = _mm256_setzero_si256();
            let mut i = 0;
            while i < S::EQ_LEN {
                exceptional = unsafe {
                    _mm256_or_si256(
                        exceptional,
                        _mm256_cmpeq_epi8(v, _mm256_set1_epi8(S::EQ[i as usize].cast_signed())),
                    )
                };
                i += 1;
            }
            let range = match (S::LT, S::GE) {
                (Some(0x20), Some(0x80)) => {
                    // Signed negative lanes are non-ASCII and compare below 0x20.
                    _mm256_cmpgt_epi8(_mm256_set1_epi8(0x20), v)
                }
                (Some(lt), None) => unsafe { lt_u(v, lt) },
                (None, Some(ge)) => unsafe { ge_u(v, ge) },
                (Some(lt), Some(ge)) => unsafe { _mm256_or_si256(lt_u(v, lt), ge_u(v, ge)) },
                (None, None) => _mm256_setzero_si256(),
            };
            exceptional = unsafe { _mm256_or_si256(exceptional, range) };
            // `_mm256_movemask_epi8` sets 32 bits, all-ones when every byte matched the compare chain.
            let mask = unsafe { _mm256_movemask_epi8(exceptional) } as u32;
            let bits = if S::ALL { !mask } else { mask };
            (bits != 0).then_some(bits.trailing_zeros() as usize)
        }

        /// Longest prefix containing no byte of `S`, over whole 32-byte lanes, then one 16-byte SSE2 remainder. A
        /// leftover shorter than 16 is left to the caller's scalar tail.
        #[target_feature(enable = "avx2")]
        pub(crate) unsafe fn wide<S: super::super::StopSet>(pointer: *const u8, len: usize) -> usize {
            let mut offset = 0_usize;
            while len - offset >= 32 {
                // SAFETY: the loop condition proves 32 readable bytes at `offset`.
                let lane = unsafe { &*pointer.add(offset).cast::<[u8; 32]>() };
                if let Some(hit) = unsafe { first_hit::<S>(lane) } {
                    return offset + hit;
                }
                offset += 32;
            }
            if len - offset >= 16 {
                // SAFETY: AVX2 implies SSE2; 16 readable bytes at `offset`.
                let lane = unsafe { &*pointer.add(offset).cast::<[u8; 16]>() };
                if let Some(hit) = unsafe { super::sse2::first_hit::<S>(lane) } {
                    return offset + hit;
                }
                offset += 16;
            }
            offset
        }

        /// The continuation-demand mask for a NEW lane's first three positions, computed from the previous lane's last
        /// three bytes, and the same mask for a 32-byte lane's position 16..18, computed from the lane's own bytes
        /// 13..15. `_mm256_slli_si256` shifts each 128-bit lane independently, so a lead ending a 16-byte half does NOT
        /// reach the next half: position 16..18 need the same boundary terms the outer boundary provides at 0..2. Both
        /// halves share `boundary_cont_mask`.
        fn boundary(v: &[u8; 32], prev16: &[u8; 16]) -> __m256i {
            let mut arr = [0_u8; 32];
            let outer = super::super::boundary_cont_mask(prev16);
            arr[0] = outer[0];
            arr[1] = outer[1];
            arr[2] = outer[2];
            let mut mid_prev = [0_u8; 16];
            mid_prev[13] = v[13];
            mid_prev[14] = v[14];
            mid_prev[15] = v[15];
            let middle = super::super::boundary_cont_mask(&mid_prev);
            arr[16] = middle[0];
            arr[17] = middle[1];
            arr[18] = middle[2];
            load(&arr)
        }

        /// Whether the 32-byte lane can be skipped by the UTF-8 walk: every byte is ASCII and the previous lane's
        /// trailing bytes demand no continuation inside this one. With the lane all-ASCII, the middle boundary (bytes
        /// 13..15, which are ASCII) is empty by construction, so only the outer boundary needs checking.
        ///
        /// # Safety
        ///
        /// The caller verifies AVX2 with `avx2()` first and bounds every load.
        #[target_feature(enable = "avx2")]
        #[must_use]
        pub unsafe fn lane_ascii_clean(v: &[u8; 32], prev16: &[u8; 16]) -> bool {
            let v = load(v);
            let outer = {
                let mut arr = [0_u8; 32];
                let o = super::super::boundary_cont_mask(prev16);
                arr[0] = o[0];
                arr[1] = o[1];
                arr[2] = o[2];
                load(&arr)
            };
            // SAFETY: `ge_u` is register-only under the AVX2 precondition.
            unsafe { (_mm256_movemask_epi8(ge_u(v, 0x80)) | _mm256_movemask_epi8(outer)) == 0 }
        }

        /// Whether any byte of the 32-byte lane starts or continues an invalid UTF-8 sequence, given the previous 16
        /// bytes and the next 32 (zero padded past the end). The exact mask algebra of the SSE2 kernel, over two lanes
        /// at once; the only added term is the middle boundary.
        ///
        /// # Safety
        ///
        /// The caller verifies AVX2 with `avx2()` first and bounds every load.
        #[expect(
            clippy::similar_names,
            reason = "e0_bad/ed_bad/f0_bad/f4_bad are named for the hex lead bytes whose second-byte law they check"
        )]
        #[target_feature(enable = "avx2")]
        #[allow(
            unused_unsafe,
            reason = "see `first_hit`: the intrinsics' safety differs by target in rustc 1.96, so \
                      the vestigial wrappers fire `unused_unsafe` only on linux-gnu and cannot \
                      be `expect`ed"
        )]
        #[must_use]
        pub unsafe fn lane_has_invalid(v: &[u8; 32], prev16: &[u8; 16], next32: &[u8; 32]) -> bool {
            let boundary = boundary(v, prev16);
            let v = load(v);
            let next32 = load(next32);
            let is_cont = unsafe {
                _mm256_cmpeq_epi8(
                    _mm256_and_si256(v, _mm256_set1_epi8(0xC0_u8.cast_signed())),
                    _mm256_set1_epi8(0x80_u8.cast_signed()),
                )
            };
            let is_lead2 = unsafe {
                _mm256_cmpeq_epi8(
                    _mm256_and_si256(v, _mm256_set1_epi8(0xE0_u8.cast_signed())),
                    _mm256_set1_epi8(0xC0_u8.cast_signed()),
                )
            };
            let is_lead3 = unsafe {
                _mm256_cmpeq_epi8(
                    _mm256_and_si256(v, _mm256_set1_epi8(0xF0_u8.cast_signed())),
                    _mm256_set1_epi8(0xE0_u8.cast_signed()),
                )
            };
            let is_lead4 = unsafe {
                _mm256_cmpeq_epi8(
                    _mm256_and_si256(v, _mm256_set1_epi8(0xF8_u8.cast_signed())),
                    _mm256_set1_epi8(0xF0_u8.cast_signed()),
                )
            };
            // A byte at position i must be a continuation when a 2/3/4-byte lead sits one position before it, a
            // 3/4-byte lead two before, or a 4-byte lead three before. `_mm256_slli_si256` shifts each 128-bit half
            // independently, so the boundary mask carries both the outer (prev16) and the middle (lane bytes 13..15)
            // terms.
            let must_cont = unsafe {
                let m = _mm256_or_si256(
                    _mm256_or_si256(_mm256_slli_si256::<1>(is_lead2), _mm256_slli_si256::<1>(is_lead3)),
                    _mm256_slli_si256::<1>(is_lead4),
                );
                let m = _mm256_or_si256(
                    m,
                    _mm256_or_si256(_mm256_slli_si256::<2>(is_lead3), _mm256_slli_si256::<2>(is_lead4)),
                );
                _mm256_or_si256(m, _mm256_slli_si256::<3>(is_lead4))
            };
            let must_cont = unsafe { _mm256_or_si256(must_cont, boundary) };
            let bad_cont = unsafe { _mm256_xor_si256(is_cont, must_cont) };
            let invalid_lead = unsafe {
                _mm256_or_si256(
                    _mm256_cmpeq_epi8(
                        _mm256_and_si256(v, _mm256_set1_epi8(0xFE_u8.cast_signed())),
                        _mm256_set1_epi8(0xC0_u8.cast_signed()),
                    ),
                    ge_u(v, 0xF5),
                )
            };
            // The lookahead is the mirror of `must_cont`'s look-back: byte i+1 at position i. `_mm256_srli_si256(v, 1)`
            // is half-local, so position 15 and 31 both read 0; `_mm256_permute2x128_si256` then brings v[16] to
            // position 15 (the high half shifted left) and next32[0] to position 31 (the next lane's low half),
            // matching the SSE2 kernel's `srli(v,1) | slli(next16,15)`.
            let n1 = unsafe {
                let swapped = _mm256_permute2x128_si256::<0x21>(v, next32);
                _mm256_or_si256(_mm256_srli_si256::<1>(v), _mm256_slli_si256::<15>(swapped))
            };
            let e0_bad = unsafe {
                _mm256_and_si256(
                    _mm256_cmpeq_epi8(v, _mm256_set1_epi8(0xE0_u8.cast_signed())),
                    lt_u(n1, 0xA0),
                )
            };
            let ed_bad = unsafe {
                _mm256_and_si256(
                    _mm256_cmpeq_epi8(v, _mm256_set1_epi8(0xED_u8.cast_signed())),
                    ge_u(n1, 0xA0),
                )
            };
            let f0_bad = unsafe {
                _mm256_and_si256(
                    _mm256_cmpeq_epi8(v, _mm256_set1_epi8(0xF0_u8.cast_signed())),
                    lt_u(n1, 0x90),
                )
            };
            let f4_bad = unsafe {
                _mm256_and_si256(
                    _mm256_cmpeq_epi8(v, _mm256_set1_epi8(0xF4_u8.cast_signed())),
                    ge_u(n1, 0x90),
                )
            };
            let bad = unsafe {
                let lead = _mm256_or_si256(bad_cont, invalid_lead);
                let second = _mm256_or_si256(_mm256_or_si256(e0_bad, ed_bad), _mm256_or_si256(f0_bad, f4_bad));
                _mm256_or_si256(lead, second)
            };
            unsafe { _mm256_movemask_epi8(bad) != 0 }
        }

        fn load(lane: &[u8; 32]) -> __m256i {
            // SAFETY: the caller guarantees 32 readable bytes.
            unsafe { _mm256_loadu_si256(lane.as_ptr().cast::<__m256i>()) }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Every declared stop set, driven through the generic alignment oracle below. Adding a set here is what gives its
    /// adopter alignment-exhaustive verification for free.
    const DECLARED_SETS: &[&dyn SetOracle] = &[
        &Escape,
        &PlainString,
        &StringContent,
        &Structural,
        &Delimiter,
        &Ws,
        &NdjsonFrame,
    ];

    /// Object-safe shim so the alignment oracle can drive heterogeneous sets from one loop.
    trait SetOracle {
        fn check(&self, bytes: &[u8], start: usize, end: usize);
        #[cfg(target_arch = "x86_64")]
        fn check_sse2_avx2_identity(&self, bytes: &[u8], start: usize, end: usize);
    }

    impl<S: StopSet> SetOracle for S {
        fn check(&self, bytes: &[u8], start: usize, end: usize) {
            let slice = &bytes[start..end];
            let wide = prefix_len::<S>(slice);
            let expected = slice.iter().take_while(|b| !S::stop(**b)).count();
            assert_eq!(
                wide,
                expected,
                "prefix_len mismatch for {} at {start}..{end} of {bytes:?}",
                core::any::type_name::<S>(),
            );
        }

        #[cfg(target_arch = "x86_64")]
        fn check_sse2_avx2_identity(&self, bytes: &[u8], start: usize, end: usize) {
            let slice = &bytes[start..end];
            // SAFETY: the caller gates on `x86_64::avx2()`, so both kernel
            // families' features are present; the drivers bound their loads.
            let sse2_run = unsafe {
                let wide = x86_64::sse2::wide::<S>(slice.as_ptr(), slice.len());
                wide + slice[wide..].iter().take_while(|b| !S::stop(**b)).count()
            };
            let avx2_run = unsafe {
                let wide = x86_64::avx2::wide::<S>(slice.as_ptr(), slice.len());
                wide + slice[wide..].iter().take_while(|b| !S::stop(**b)).count()
            };
            assert_eq!(
                sse2_run,
                avx2_run,
                "{} diverged at {start}..{end} of {bytes:?}",
                core::any::type_name::<S>(),
            );
        }
    }

    /// A 20-byte buffer with the stop at every offset 0..=19: `prefix_len` must name that offset, matching a scalar
    /// oracle, so a hitting 16-byte lane that returned only its start would fail at offsets 1..=15.
    #[test]
    fn prefix_len_agrees_with_scalar_at_every_offset_in_a_20_byte_buffer() {
        for position in 0..=19 {
            let mut bytes = [b'a'; 20];
            bytes[position] = b'"';
            let got = prefix_len::<PlainString>(&bytes);
            let expected = bytes.iter().take_while(|byte| !PlainString::stop(**byte)).count();
            assert_eq!(got, expected, "PlainString stop at {position}");
            assert_eq!(got, position, "PlainString stop at {position}");
        }
        // ALL polarity: hit is the first byte NOT in the set.
        for position in 0..=19 {
            let mut bytes = [b' '; 20];
            bytes[position] = b'x';
            let got = prefix_len::<Ws>(&bytes);
            let expected = bytes.iter().take_while(|byte| !Ws::stop(**byte)).count();
            assert_eq!(got, expected, "Ws stop at {position}");
            assert_eq!(got, position, "Ws stop at {position}");
        }
    }

    /// The generic alignment oracle: for every declared set, `prefix_len` must agree with the scalar predicate at every
    /// alignment and length up to the corpus cap. A wrong kernel is a test failure here, on every architecture the
    /// kernels exist for.
    #[test]
    fn every_declared_set_agrees_with_its_scalar_predicate_at_every_alignment() {
        // Hand-picked adversarial seeds per shape, then pseudo-random corpora biased toward the sets' own bytes so runs
        // cross the 16-byte lane boundary and end in the scalar tail often.
        for set in DECLARED_SETS {
            let mut corpus: Vec<Vec<u8>> = vec![
                Vec::new(),
                b"a".to_vec(),
                b"\"".to_vec(),
                b"\\".to_vec(),
                b"\x1f".to_vec(),
                b"\x20".to_vec(),
                b"\x7f".to_vec(),
                b"\x80".to_vec(),
                b"plain text".to_vec(),
                b"{\"k\": 1}".to_vec(),
                b" \t\n\r a".to_vec(),
                b"a,b\"c\r\nd".to_vec(),
                b"<tag>&amp;</tag>".to_vec(),
                b"\x00\x1f\x7f\xff".to_vec(),
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
                    let r = mix(&mut state);
                    bytes.push(match r % 8 {
                        0..=3 => b" \t\n\r\"'<&,;[{\x00\x7f\xef\xf0"[((r >> 8) % 16) as usize],
                        _ => ((r >> 16) & 0xFF) as u8,
                    });
                }
                corpus.push(bytes);
            }
            for bytes in &corpus {
                for start in 0..=bytes.len().min(3) {
                    for end in start..=bytes.len().min(start + 48) {
                        set.check(bytes, start, end);
                    }
                }
            }
        }
    }

    /// The same oracle over the exact byte classes the JSON kernels' contract tests pin: each stop-set member placed at
    /// every position around the first and second lane boundaries.
    #[test]
    fn declared_sets_agree_at_lane_boundaries() {
        let terminators: &[u8] = &[
            b'"', b'\\', b'<', b'&', b',', b'\t', b'\r', b'\n', b'}', b']', 0x00, 0x01, 0x1f, 0x7f, 0x80, 0xff,
        ];
        for set in DECLARED_SETS {
            for &terminator in terminators {
                for position in 0..40 {
                    let mut bytes = vec![b'a'; 40];
                    bytes[position] = terminator;
                    set.check(&bytes, 0, bytes.len());
                }
            }
        }
    }

    /// The AVX2 dispatch's whole contract is byte-identity: the same answer from the 32-byte kernels and the 16-byte
    /// SSE2 kernels, over every declared set. Only runs where AVX2 is actually present — the dockerized linux-amd64
    /// lane is the one machine that can see it. A machine without AVX2 skips the body, so the test is a no-op there,
    /// never a false green.
    #[cfg(target_arch = "x86_64")]
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the AVX2-vs-SSE2 byte-identity sweep drives the same adversarial corpora \
                  through every kernel pair sequentially — one long sequential test is the \
                  contract, and only an x86_64 clippy can even see it (the linux-amd64 lane)"
    )]
    fn avx2_and_sse2_kernels_are_byte_identical() {
        if !x86_64::avx2() {
            return;
        }
        let mut state = 0x2d1b_5a64_9f3c_77e1_u64;
        let mix = |state: &mut u64| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };
        for set in DECLARED_SETS {
            for len in 0..160 {
                let mut bytes = Vec::with_capacity(len);
                for _ in 0..len {
                    let r = mix(&mut state);
                    bytes.push((r & 0xFF) as u8);
                }
                set.check_sse2_avx2_identity(&bytes, 0, len);
            }
        }
    }
}
