//! RFC 8949 §5.6.1 map-key equivalence and the decoder's uniqueness ledger.
//!
//! The generic dialect validates map-key uniqueness BEFORE any recognized-tag normalization or projection. Keys fall
//! into six DISTINCT numeric groups — basic integer, float, tag-2 bignum, tag-3 bignum, tag-4 decimal, tag-5 bigfloat
//! — plus text, bytes, arrays, maps, other tags, and simple values. Keys in different groups are never equal even
//! when numerically identical (`0 ≠ 0.0`, a tagged bignum never equals an untagged integer). The RFC does not settle
//! whether tags 2–5 form one group or several; the resolution here is that they are mutually distinct — only
//! within-tag normalization applies.
//!
//! The decoder parses every map key into a [`KeyValue`] and records it in a [`KeySet`]. Uniqueness is decided by a
//! 64-bit fingerprint plus an exact equivalence comparison on fingerprint collisions; a collision is only declared
//! after the exact comparison, so the fingerprint's collision resistance bounds work rather than deciding correctness.
//!
//! Numeric normalization is exact without expanding the power: factors of 10 (decimal) or 2 (bigfloat) are removed from
//! the arbitrary-precision coefficient while the exponent is adjusted, and two normalized pairs are equal exactly when
//! both parts are. Leading magnitude zeroes in tag-2/3 byte strings are ignored by construction ([`crate::big::Big`]
//! has no leading-zero representation). Floats compare by their widened binary64 bits: `-0.0 == +0.0`, and NaNs are
//! equal only when their 52-bit significands match after right-zero-extension to 64 bits.

extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

use crate::big::Big;

/// One key's full §5.6.1 identity, parsed from the wire during map decode.
#[derive(Clone, Debug)]
pub(crate) enum KeyValue {
    /// Basic integer: sign plus magnitude.
    BasicInt {
        /// Whether the value is negative.
        negative: bool,
        /// Magnitude.
        magnitude: Big,
    },
    /// A widened binary64 float payload (bits as stored).
    Float(u64),
    /// A text string, bytewise.
    Text(Vec<u8>),
    /// A byte string, bytewise.
    Bytes(Vec<u8>),
    /// An ordered array, element by element.
    Array(Vec<KeyValue>),
    /// A map as an unordered pair set.
    Map(Vec<(KeyValue, KeyValue)>),
    /// Any other tag: by tag number then content.
    Tagged {
        /// The tag number.
        number: u64,
        /// The tagged content.
        content: Box<KeyValue>,
    },
    /// Tag-2 bignum magnitude (value `n`).
    Bignum2(Big),
    /// Tag-3 bignum magnitude (value `-1 - n`).
    Bignum3(Big),
    /// Tag-4 decimal: `coefficient * 10^exponent`.
    Decimal {
        /// Signed coefficient.
        coefficient: Big,
        /// Base-ten exponent (arbitrary precision).
        exponent: Big,
    },
    /// Tag-5 bigfloat: `mantissa * 2^exponent`.
    Bigfloat {
        /// Signed mantissa.
        mantissa: Big,
        /// Base-two exponent (arbitrary precision).
        exponent: Big,
    },
    /// A simple value by number.
    Simple(u8),
}

/// Dismantles a key tree onto a worklist instead of recursing into it.
///
/// A map key may be a container, so a key's depth is the DOCUMENT's depth, and the derived drop glue recursed once per
/// level: a deep key ended the process with a stack overflow when it was dropped — on every path, including the error
/// paths where a key parses and is then refused by the ledger, and including the drop of the ledger itself. Nothing
/// observable changes; only the order the shells are freed in.
///
/// The moves this costs: a type with a manual `Drop` cannot have its fields moved out by pattern, so the two
/// `take_as_*` readers below and the codec's tag fold take their payload with [`core::mem::replace`] instead.
impl Drop for KeyValue {
    fn drop(&mut self) {
        // A scalar owns no child, so the common key allocates nothing here.
        let mut worklist = Vec::new();
        take_children(self, &mut worklist);
        while let Some(mut value) = worklist.pop() {
            take_children(&mut value, &mut worklist);
            // `value` drops with empty shells: the glue has nothing to recurse into, and this same impl runs over a
            // childless value.
        }
    }
}

/// Moves one value's children onto the worklist, leaving an empty shell behind.
fn take_children(value: &mut KeyValue, worklist: &mut Vec<KeyValue>) {
    match value {
        KeyValue::Array(items) => worklist.append(items),
        KeyValue::Map(pairs) => {
            for (key, value) in core::mem::take(pairs) {
                worklist.push(key);
                worklist.push(value);
            }
        }
        KeyValue::Tagged { content, .. } => {
            worklist.push(core::mem::replace(content.as_mut(), KeyValue::Simple(0)));
        }
        _ => {}
    }
}

impl KeyValue {
    /// The exact signed value when the key is a basic integer.
    #[must_use]
    pub(crate) fn take_as_exponent(mut self) -> Option<Big> {
        Self::take_basic(&mut self)
    }

    /// The exact signed value when the key is an integer (basic or bignum).
    #[must_use]
    pub(crate) fn take_as_integer(mut self) -> Option<Big> {
        match &mut self {
            Self::Bignum2(magnitude) => Some(core::mem::replace(magnitude, Big::zero())),
            Self::Bignum3(magnitude) => Some(core::mem::replace(magnitude, Big::zero()).add_small(1).negated()),
            _ => Self::take_basic(&mut self),
        }
    }

    /// The shared basic-integer extraction; every other key kind is `None`.
    fn take_basic(key: &mut Self) -> Option<Big> {
        let Self::BasicInt { negative, magnitude } = key else {
            return None;
        };
        let magnitude = core::mem::replace(magnitude, Big::zero());
        Some(if *negative { magnitude.negated() } else { magnitude })
    }
}

impl KeyValue {
    /// The §5.6.1 equivalence of two keys.
    ///
    /// The descent is an explicit goal stack, NOT recursion: a key carries the document's depth, and this runs on the
    /// ledger's duplicate path, where a pair of deep keys is trivially built (`{[[[…]]]: 1, [[[…]]]: 2}` — equal
    /// fingerprints, so the exact comparison runs). Most goals are plain CONJUNCTS; a map's pair set is the one goal
    /// that SEARCHES, and it searches on the same stack ([`Goal::Match`]) rather than in a nested call, so nested map
    /// keys cost no native frames either.
    #[must_use]
    pub(crate) fn equivalent(&self, other: &Self) -> bool {
        // A scalar pair schedules nothing, so the common comparison allocates nothing.
        let mut goals: Vec<Goal<'_>> = Vec::new();
        let mut compare = Some((self, other));
        loop {
            if let Some((left, right)) = compare.take()
                && !left.shallow_equivalent(right, &mut goals)
                && !backtrack(&mut goals)
            {
                return false;
            }
            match goals.pop() {
                None => return true,
                Some(Goal::Compare(left, right)) => compare = Some((left, right)),
                Some(Goal::Match(state)) => {
                    if !advance_match(state, &mut goals) && !backtrack(&mut goals) {
                        return false;
                    }
                }
            }
        }
    }

    /// Compares one value pair's OWN payload and schedules their children as further goals. Nothing here descends.
    #[must_use]
    fn shallow_equivalent<'a>(&'a self, other: &'a Self, goals: &mut Vec<Goal<'a>>) -> bool {
        match (self, other) {
            (
                Self::BasicInt {
                    negative: left_neg,
                    magnitude: left,
                },
                Self::BasicInt {
                    negative: right_neg,
                    magnitude: right,
                },
            ) => left_neg == right_neg && left == right,
            (Self::Float(left), Self::Float(right)) => float_equivalent(*left, *right),
            (Self::Bignum2(left), Self::Bignum2(right)) | (Self::Bignum3(left), Self::Bignum3(right)) => left == right,
            (
                Self::Decimal {
                    coefficient: left_c,
                    exponent: left_e,
                },
                Self::Decimal {
                    coefficient: right_c,
                    exponent: right_e,
                },
            ) => {
                let (left_c, left_e) = normalize_decimal(left_c, left_e);
                let (right_c, right_e) = normalize_decimal(right_c, right_e);
                left_c == right_c && left_e == right_e
            }
            (
                Self::Bigfloat {
                    mantissa: left_m,
                    exponent: left_e,
                },
                Self::Bigfloat {
                    mantissa: right_m,
                    exponent: right_e,
                },
            ) => {
                let (left_m, left_e) = normalize_bigfloat(left_m, left_e);
                let (right_m, right_e) = normalize_bigfloat(right_m, right_e);
                left_m == right_m && left_e == right_e
            }
            (Self::Text(left), Self::Text(right)) | (Self::Bytes(left), Self::Bytes(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => {
                if left.len() != right.len() {
                    return false;
                }
                // Pushed in reverse so the goals come off the stack in element order, as the zipped `all` visited them.
                goals.extend(
                    left.iter()
                        .zip(right.iter())
                        .rev()
                        .map(|(left, right)| Goal::Compare(left, right)),
                );
                true
            }
            (Self::Map(left), Self::Map(right)) => {
                if left.len() != right.len() {
                    return false;
                }
                goals.push(Goal::Match(PairMatch {
                    left,
                    right,
                    taken: vec![false; right.len()],
                    index: 0,
                    candidate: 0,
                    trial: false,
                }));
                true
            }
            (
                Self::Tagged {
                    number: left_n,
                    content: left_c,
                },
                Self::Tagged {
                    number: right_n,
                    content: right_c,
                },
            ) => {
                if left_n != right_n {
                    return false;
                }
                goals.push(Goal::Compare(left_c.as_ref(), right_c.as_ref()));
                true
            }
            (Self::Simple(left), Self::Simple(right)) => left == right,
            // Numeric groups are distinct from one another and from every other kind; a cross-arm pair is never equal.
            _ => false,
        }
    }

    /// A 64-bit fingerprint that folds the numeric group so distinct groups never collide by construction, and folds
    /// equal keys to equal values.
    #[must_use]
    pub(crate) fn fingerprint(&self) -> u64 {
        let mut hash = FNV_OFFSET;
        self.canonical(&mut hash);
        hash
    }

    /// Folds this value into `hash`, driving the descent from an explicit task stack rather than recursing: this runs
    /// on EVERY key the ledger sees, so it is both the hot path and a path a container-valued key carries the
    /// document's depth into. A scalar key schedules nothing and allocates nothing.
    fn canonical(&self, hash: &mut u64) {
        let mut tasks: Vec<CanonicalTask<'_>> = Vec::new();
        // One fresh accumulator per map-pair digest being folded, and one digest list per OPEN map (a map folds its
        // pairs as an unordered SET).
        let mut accumulators: Vec<u64> = Vec::new();
        let mut open_maps: Vec<Vec<u64>> = Vec::new();
        fold_one(self, hash, &mut tasks, &mut open_maps);
        while let Some(task) = tasks.pop() {
            match task {
                CanonicalTask::Fold(value) => {
                    let hash = accumulators.last_mut().unwrap_or(&mut *hash);
                    fold_one(value, hash, &mut tasks, &mut open_maps);
                }
                CanonicalTask::Digest(value) => {
                    accumulators.push(FNV_OFFSET);
                    tasks.push(CanonicalTask::EndDigest);
                    tasks.push(CanonicalTask::Fold(value));
                }
                CanonicalTask::EndDigest => {
                    // Both stacks are non-empty by construction: this task is only ever scheduled by the map arm of
                    // `fold_one`.
                    if let Some(digest) = accumulators.pop()
                        && let Some(open) = open_maps.last_mut()
                    {
                        open.push(digest);
                    }
                }
                CanonicalTask::EndMap => {
                    if let Some(digests) = open_maps.pop() {
                        let hash = accumulators.last_mut().unwrap_or(&mut *hash);
                        fold_pair_set(hash, digests);
                    }
                }
            }
        }
    }
}

/// One scheduled step of the canonical fold.
enum CanonicalTask<'a> {
    /// Fold this value into the innermost open accumulator.
    Fold(&'a KeyValue),
    /// Fold this value into a FRESH accumulator: one half of a map pair's order-independent digest.
    Digest(&'a KeyValue),
    /// Close the fresh accumulator, handing its hash to the open map.
    EndDigest,
    /// Every pair digest is in: fold them as a SET into the map's own accumulator.
    EndMap,
}

/// Folds one value's OWN bytes into `hash` and schedules its children. Nothing here descends: a container schedules and
/// returns.
fn fold_one<'a>(
    value: &'a KeyValue,
    hash: &mut u64,
    tasks: &mut Vec<CanonicalTask<'a>>,
    open_maps: &mut Vec<Vec<u64>>,
) {
    let kind: u8 = match value {
        KeyValue::BasicInt { .. } => 0x00,
        KeyValue::Float(_) => 0x01,
        KeyValue::Text(_) => 0x02,
        KeyValue::Bytes(_) => 0x03,
        KeyValue::Array(_) => 0x04,
        KeyValue::Map(_) => 0x05,
        KeyValue::Tagged { .. } => 0x06,
        KeyValue::Bignum2(_) => 0x07,
        KeyValue::Bignum3(_) => 0x08,
        KeyValue::Decimal { .. } => 0x09,
        KeyValue::Bigfloat { .. } => 0x0a,
        KeyValue::Simple(_) => 0x0b,
    };
    fold(hash, kind);
    match value {
        KeyValue::BasicInt { negative, magnitude } => {
            fold(hash, u8::from(*negative));
            fold_big(hash, magnitude);
        }
        KeyValue::Float(bits) => {
            let canonical_bits = canonical_float_bits(*bits);
            fold_u64(hash, canonical_bits);
        }
        KeyValue::Text(bytes) | KeyValue::Bytes(bytes) => fold_bytes(hash, bytes),
        KeyValue::Array(items) => {
            // Pushed in reverse so the children fold in element order.
            for item in items.iter().rev() {
                tasks.push(CanonicalTask::Fold(item));
            }
        }
        KeyValue::Map(pairs) => {
            open_maps.push(Vec::with_capacity(pairs.len().saturating_mul(2)));
            tasks.push(CanonicalTask::EndMap);
            for (key, value) in pairs.iter().rev() {
                tasks.push(CanonicalTask::Digest(value));
                tasks.push(CanonicalTask::Digest(key));
            }
        }
        KeyValue::Tagged { number, content } => {
            fold_u64(hash, *number);
            tasks.push(CanonicalTask::Fold(content));
        }
        KeyValue::Bignum2(magnitude) | KeyValue::Bignum3(magnitude) => {
            fold_big(hash, magnitude);
        }
        KeyValue::Decimal { coefficient, exponent } => {
            let (coefficient, exponent) = normalize_decimal(coefficient, exponent);
            fold_big(hash, &coefficient);
            fold_big(hash, &exponent);
        }
        KeyValue::Bigfloat { mantissa, exponent } => {
            let (mantissa, exponent) = normalize_bigfloat(mantissa, exponent);
            fold_big(hash, &mantissa);
            fold_big(hash, &exponent);
        }
        KeyValue::Simple(value) => fold(hash, *value),
    }
}

/// Folds one map's collected pair digests — key hash then value hash, in wire order — into `hash` as an unordered
/// SET: the pairs' serialized digests sort before they fold, so two spellings of the same pair set fold equally.
fn fold_pair_set(hash: &mut u64, digests: Vec<u64>) {
    let mut serialized: Vec<[u8; 16]> = Vec::with_capacity(digests.len() / 2);
    let mut digests = digests.into_iter();
    while let (Some(key), Some(value)) = (digests.next(), digests.next()) {
        let mut buffer = [0_u8; 16];
        buffer[..8].copy_from_slice(&key.to_le_bytes());
        buffer[8..].copy_from_slice(&value.to_le_bytes());
        serialized.push(buffer);
    }
    serialized.sort_unstable();
    for bytes in &serialized {
        fold_bytes(hash, bytes);
    }
}

/// Folds a `Big`'s sign and canonical limbs into the fingerprint.
///
/// The limbs are the canonical form already (no leading zero limb), so equal values fold equally without rendering
/// decimal — which is quadratic in the magnitude's length and would otherwise run once per bignum map key.
fn fold_big(hash: &mut u64, value: &Big) {
    fold(hash, u8::from(value.is_negative()));
    for limb in value.limbs() {
        fold_u64(hash, u64::from(*limb));
    }
}

/// Folds a byte slice into the fingerprint.
fn fold_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        fold(hash, *byte);
    }
}

fn fold(hash: &mut u64, byte: u8) {
    *hash ^= u64::from(byte);
    *hash = hash.wrapping_mul(FNV_PRIME);
}

fn fold_u64(hash: &mut u64, value: u64) {
    for byte in value.to_le_bytes() {
        fold(hash, byte);
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The float equivalence: equal values by IEEE-754 arithmetic, with `-0.0` equal to `+0.0` and NaNs equal only when
/// their significands match after right-zero-extension to 64 bits (which the widened binary64 already is).
#[must_use]
fn float_equivalent(left: u64, right: u64) -> bool {
    if left == right {
        return true;
    }
    let is_zero = |bits: u64| f64::from_bits(bits) == 0.0;
    let is_nan = |bits: u64| f64::from_bits(bits).is_nan();
    if is_zero(left) && is_zero(right) {
        return true;
    }
    if is_nan(left) && is_nan(right) {
        return left & 0x000f_ffff_ffff_ffff == right & 0x000f_ffff_ffff_ffff;
    }
    false
}

/// The canonical fingerprint form of a float: `-0.0` and `+0.0` collapse to the positive zero, and NaNs collapse to
/// their significand (sign and the all-ones exponent carry no equivalence).
#[must_use]
fn canonical_float_bits(bits: u64) -> u64 {
    if bits == 0 || bits == 1 << 63 {
        return 0;
    }
    if bits & 0x7ff0_0000_0000_0000 == 0x7ff0_0000_0000_0000 && bits & 0x000f_ffff_ffff_ffff != 0 {
        return bits & 0x000f_ffff_ffff_ffff;
    }
    bits
}

/// Removes every factor of ten from the coefficient, raising the exponent to keep the value exact. Zero coefficients
/// canonicalize to `(0, 0)`.
fn normalize_decimal(coefficient: &Big, exponent: &Big) -> (Big, Big) {
    if coefficient.is_zero() {
        return (Big::zero(), Big::zero());
    }
    let mut coefficient = coefficient.clone();
    let mut exponent = exponent.clone();
    loop {
        let (quotient, remainder) = coefficient.div_rem_small(10);
        if remainder == 0 {
            coefficient = quotient;
            exponent = exponent.add_small(1);
        } else {
            break;
        }
    }
    (coefficient, exponent)
}

/// Removes every factor of two from the mantissa, raising the exponent to keep the value exact. Zero mantissas
/// canonicalize to `(0, 0)`.
fn normalize_bigfloat(mantissa: &Big, exponent: &Big) -> (Big, Big) {
    if mantissa.is_zero() {
        return (Big::zero(), Big::zero());
    }
    let mut mantissa = mantissa.clone();
    let mut exponent = exponent.clone();
    while mantissa.is_even() {
        let (halved, _) = mantissa.div_rem_small(2);
        mantissa = halved;
        exponent = exponent.add_small(1);
    }
    (mantissa, exponent)
}

/// One goal of the iterative §5.6.1 comparison.
enum Goal<'a> {
    /// Both values must be equivalent.
    Compare(&'a KeyValue, &'a KeyValue),
    /// A map's pair set is being matched. The frame sits UNDER the goals of the candidate trial it started, so a
    /// failing trial unwinds onto it and a finished trial pops back to it.
    Match(PairMatch<'a>),
}

/// Two maps compare as unordered pair SETS: every pair of the left must match exactly one pair of the right and vice
/// versa. Pairs are matched by fingerprint first and confirmed exactly on collision — and because the confirmation
/// can fail, the match is a SEARCH: it takes the first candidate that confirms, and moves to the next candidate when
/// one does not.
struct PairMatch<'a> {
    left: &'a [(KeyValue, KeyValue)],
    right: &'a [(KeyValue, KeyValue)],
    /// Right pairs already claimed by an earlier left pair.
    taken: Vec<bool>,
    /// The left pair being matched.
    index: usize,
    /// The right pair under trial (or the point the next scan resumes from).
    candidate: usize,
    /// Whether a candidate trial is in flight.
    trial: bool,
}

/// Advances one map's pair match: banks the trial that just finished, then opens the next candidate's trial. Reports
/// `false` when the current left pair has no candidate left — the map does not match.
fn advance_match<'a>(mut state: PairMatch<'a>, goals: &mut Vec<Goal<'a>>) -> bool {
    if state.trial {
        // Every goal of the trial passed — a failing one would have unwound instead — so the candidate is this left
        // pair's match.
        if let Some(taken) = state.taken.get_mut(state.candidate) {
            *taken = true;
        }
        state.index = state.index.saturating_add(1);
        state.candidate = 0;
        state.trial = false;
    }
    let (lefts, rights) = (state.left, state.right);
    let Some(left) = lefts.get(state.index) else {
        // Every left pair matched: the map is equivalent and the frame is done.
        return true;
    };
    // The fingerprint prefilter only ever SKIPS a comparison that would fail anyway — equivalent values fold equally
    // — so it decides nothing when one candidate is left, and folding a deep pair's digest to prove what that one
    // comparison decides is what made a chain of single-pair maps quadratic.
    let mut remaining = 0_usize;
    for (index, taken) in state.taken.iter().enumerate() {
        if index >= state.candidate && !*taken {
            remaining = remaining.saturating_add(1);
        }
    }
    // Folded once per LEFT pair, not once per candidate.
    let left_prints = (remaining > 1).then(|| (left.0.fingerprint(), left.1.fingerprint()));
    let mut found = None;
    for (index, (right, taken)) in rights.iter().zip(&state.taken).enumerate() {
        if index < state.candidate || *taken {
            continue;
        }
        if let Some((key_print, value_print)) = left_prints
            && (key_print != right.0.fingerprint() || value_print != right.1.fingerprint())
        {
            continue;
        }
        found = Some((index, right));
        break;
    }
    let Some((index, right)) = found else {
        return false;
    };
    state.candidate = index;
    state.trial = true;
    goals.push(Goal::Match(state));
    goals.push(Goal::Compare(&left.1, &right.1));
    goals.push(Goal::Compare(&left.0, &right.0));
    true
}

/// Unwinds a failed candidate trial: drops the goals it scheduled and points the innermost open pair match at its next
/// candidate. Reports `false` when there is no trial to unwind — the comparison as a whole failed.
fn backtrack(goals: &mut Vec<Goal<'_>>) -> bool {
    while let Some(goal) = goals.pop() {
        if let Goal::Match(mut state) = goal {
            state.candidate = state.candidate.saturating_add(1);
            state.trial = false;
            goals.push(Goal::Match(state));
            return true;
        }
    }
    false
}

/// The decoder's map-key uniqueness ledger.
///
/// Membership is decided by the already-computed 64-bit fingerprint in a [`BTreeSet`] — O(log k) per key instead of a
/// linear scan over every recorded key — while the parallel vector keeps the exact keys for the §5.6.1 equivalence
/// confirmation when a fingerprint repeats (a true duplicate or a fingerprint collision). The set bounds the scan; the
/// exact comparison decides.
///
/// ACCOUNTING BOUNDARY: untracked, and deliberately so. A ledger lives inside ONE open map frame and dies when that map
/// closes — it is not session state that outlives a document. Its size follows that map's keys, which the builder is
/// charged for at the same time (each key becomes occurrence key text), and the wire bytes those keys were parsed from
/// are charged already; the ledger's own copy is the third of three, the only one uncharged. Tracking it would mean a
/// fallible insert on the per-key decode path and a second, untrackable index (`BTreeSet` cannot be accounted), for a
/// peak the nesting ceiling and the source length already bound.
#[derive(Default)]
pub(crate) struct KeySet {
    fingerprints: BTreeSet<u64>,
    keys: Vec<(u64, KeyValue)>,
    /// Definite text keys stored as source ranges so the ledger never copies the key bytes. Compared against `source`
    /// at insert time.
    text_keys: Vec<(u64, core::ops::Range<usize>)>,
}

impl KeySet {
    /// Records a key, reporting `Ok(true)` when it is NEW and `Ok(false)` when §5.6.1 declares it a duplicate of an
    /// already-recorded key. `source` is the document bytes the range-stored text keys point into, so a text key's
    /// collision check against them compares BYTES exactly (a shared FNV fingerprint alone would declare a crafted
    /// collision a duplicate).
    pub(crate) fn try_insert(&mut self, source: &[u8], key: KeyValue) -> bool {
        let fingerprint = key.fingerprint();
        if !self.fingerprints.insert(fingerprint) {
            // A fingerprint repeat is either an exact duplicate or a (rare) collision; confirm exactly before declaring
            // either.
            for (existing_fingerprint, existing) in &self.keys {
                if *existing_fingerprint == fingerprint && existing.equivalent(&key) {
                    return false;
                }
            }
            if let KeyValue::Text(bytes) = &key {
                for (existing_fingerprint, existing_range) in &self.text_keys {
                    if *existing_fingerprint == fingerprint && source.get(existing_range.clone()) == Some(bytes) {
                        return false;
                    }
                }
            }
        }
        self.keys.push((fingerprint, key));
        true
    }

    /// Records a definite text key as a source range. `source[range]` is the UTF-8 payload; collisions compare those
    /// slices, never a copy.
    pub(crate) fn try_insert_text(&mut self, source: &[u8], range: core::ops::Range<usize>) -> bool {
        let Some(bytes) = source.get(range.clone()) else {
            return true;
        };
        let fingerprint = text_fingerprint(bytes);
        if !self.fingerprints.insert(fingerprint) {
            for (existing_fingerprint, existing) in &self.text_keys {
                if *existing_fingerprint == fingerprint && source.get(existing.clone()) == Some(bytes) {
                    return false;
                }
            }
            for (existing_fingerprint, existing) in &self.keys {
                if *existing_fingerprint == fingerprint
                    && let KeyValue::Text(existing_bytes) = existing
                    && existing_bytes.as_slice() == bytes
                {
                    return false;
                }
            }
        }
        self.text_keys.push((fingerprint, range));
        true
    }
}

fn text_fingerprint(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    fold(&mut hash, 0x02);
    fold_bytes(&mut hash, bytes);
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(negative: bool, magnitude: u64) -> KeyValue {
        KeyValue::BasicInt {
            negative,
            magnitude: Big::from_u64(magnitude),
        }
    }

    #[test]
    fn text_key_vs_range_entry_duplicate_compares_bytes() {
        // A crafted FNV-1a-64 collision is impractical to construct, so the exact-compare branch between an owned text
        // key and a range-stored entry is exercised the only way reachable through behavior: the same bytes fire the
        // branch and declare the duplicate. A differing-bytes repeat now stays NEW by construction — the materialized
        // range compare decides, never the fingerprint alone.
        let mut keys = KeySet::default();
        assert!(keys.try_insert_text(b"abcd", 0..2));
        assert!(
            !keys.try_insert(b"abcd", KeyValue::Text(b"ab".to_vec())),
            "owned text repeating a range-stored key is a duplicate"
        );
        assert!(
            keys.try_insert(b"abcd", KeyValue::Text(b"cd".to_vec())),
            "a different owned text never collides with the range entry"
        );
    }

    #[test]
    fn numeric_groups_never_collide() {
        let integer = basic(false, 0);
        let float = KeyValue::Float(0);
        let bignum = KeyValue::Bignum2(Big::zero());
        assert_ne!(integer.fingerprint(), float.fingerprint());
        assert_ne!(integer.fingerprint(), bignum.fingerprint());
        assert_ne!(float.fingerprint(), bignum.fingerprint());
        assert!(!integer.equivalent(&float));
        assert!(!integer.equivalent(&bignum));
        assert!(!float.equivalent(&bignum));
    }

    #[test]
    fn integers_equal_by_sign_and_magnitude() {
        assert!(basic(false, 5).equivalent(&basic(false, 5)));
        assert!(!basic(true, 5).equivalent(&basic(false, 5)));
        assert!(!basic(false, 5).equivalent(&basic(false, 6)));
        assert!(KeyValue::Bignum2(Big::from_u64(7)).equivalent(&KeyValue::Bignum2(Big::from_u64(7))));
        assert!(!KeyValue::Bignum2(Big::from_u64(7)).equivalent(&KeyValue::Bignum2(Big::from_u64(8))));
    }

    #[test]
    fn floats_handle_zero_and_nan() {
        assert!(KeyValue::Float(0x8000_0000_0000_0000).equivalent(&KeyValue::Float(0)));
        let nan_a = 0x7ff8_0000_0000_0001;
        let nan_b = 0x7ff8_0000_0000_0001;
        let nan_c = 0x7ff8_0000_0000_0002;
        assert!(KeyValue::Float(nan_a).equivalent(&KeyValue::Float(nan_b)));
        assert!(!KeyValue::Float(nan_a).equivalent(&KeyValue::Float(nan_c)));
        assert!(KeyValue::Float(nan_a).fingerprint() == KeyValue::Float(nan_b).fingerprint());
        assert!(KeyValue::Float(nan_a).fingerprint() != KeyValue::Float(nan_c).fingerprint());
    }

    #[test]
    fn decimal_normalization_makes_alternates_duplicates() {
        let negative_one = Big::from_u64(1).negated();
        let a = KeyValue::Decimal {
            coefficient: Big::from_u64(150),
            exponent: negative_one.clone(),
        };
        let b = KeyValue::Decimal {
            coefficient: Big::from_u64(15),
            exponent: Big::zero(),
        };
        assert!(a.equivalent(&b));
        assert!(a.fingerprint() == b.fingerprint());
        let zero_a = KeyValue::Decimal {
            coefficient: Big::zero(),
            exponent: Big::from_u64(5),
        };
        let zero_b = KeyValue::Decimal {
            coefficient: Big::zero(),
            exponent: Big::from_u64(99).negated(),
        };
        assert!(zero_a.equivalent(&zero_b));
        assert!(!a.equivalent(&zero_a));
    }

    #[test]
    fn bigfloat_normalization_makes_alternates_duplicates() {
        let a = KeyValue::Bigfloat {
            mantissa: Big::from_u64(12),
            exponent: Big::from_u64(1),
        };
        let b = KeyValue::Bigfloat {
            mantissa: Big::from_u64(6),
            exponent: Big::from_u64(2),
        };
        let c = KeyValue::Bigfloat {
            mantissa: Big::from_u64(3),
            exponent: Big::from_u64(3),
        };
        assert!(a.equivalent(&b));
        assert!(a.equivalent(&c));
        assert!(a.fingerprint() == c.fingerprint());
    }

    #[test]
    fn text_and_bytes_are_bytewise() {
        let text_a = KeyValue::Text(b"abc".to_vec());
        let text_b = KeyValue::Text(b"abc".to_vec());
        let text_c = KeyValue::Text(b"abd".to_vec());
        assert!(text_a.equivalent(&text_b));
        assert!(!text_a.equivalent(&text_c));
        assert!(text_a.fingerprint() == text_b.fingerprint());
        // Text and bytes never collide.
        assert!(!text_a.equivalent(&KeyValue::Bytes(b"abc".to_vec())));
        assert_ne!(text_a.fingerprint(), KeyValue::Bytes(b"abc".to_vec()).fingerprint());
    }

    #[test]
    fn key_set_detects_duplicates() {
        let mut set = KeySet::default();
        assert!(set.try_insert(b"", basic(false, 1)));
        assert!(set.try_insert(b"", basic(false, 2)));
        assert!(!set.try_insert(b"", basic(false, 1)));
        // 1 and 1.0 are distinct groups.
        assert!(set.try_insert(b"", KeyValue::Float(0x3ff0_0000_0000_0000)));
    }

    #[test]
    fn maps_compare_as_unordered_sets() {
        let pair = |k: u64, v: u64| (basic(false, k), basic(false, v));
        let left = KeyValue::Map(vec![pair(1, 2), pair(3, 4)]);
        let right = KeyValue::Map(vec![pair(3, 4), pair(1, 2)]);
        let different = KeyValue::Map(vec![pair(1, 2), pair(3, 5)]);
        assert!(left.equivalent(&right));
        assert!(left.fingerprint() == right.fingerprint());
        assert!(!left.equivalent(&different));
    }
}
