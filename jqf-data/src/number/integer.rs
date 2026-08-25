//! Arbitrary-precision integers, stored as signed decimal text.
//!
//! Values that fit in `i64` keep the machine value and its digits inline. Wider values keep the spelling on the heap.
//! Equal canonical bytes always pick the same arm, so `Eq` / `Hash` / `Ord` over [`Integer::as_str`] cannot disagree.
//! [`Integer::boxed_from_i64`] is the exception: it forces the heap arm so a machine value can have an allocation
//! identity.
//!
//! [`Integer::parse`] turns `-0` into `0`. A document that must keep negative zero stores the spelling `-0` instead.

use alloc::borrow::Cow;
use alloc::string::String;

use super::NumericError;

/// The widest canonical `i64` spelling: `i64::MIN` is 19 digits plus a sign.
const MACHINE_TEXT_CAP: usize = 20;

/// The hundred two-digit decimal pairs `00`..=`99`, so the spelling of `n` is `DIGIT_PAIRS[2 * n..2 * n + 2]`.
///
/// This is what lets [`MachineInt::render`] retire two digits per division instead of one.
const DIGIT_PAIRS: &[u8; 200] = b"00010203040506070809\
                                  10111213141516171819\
                                  20212223242526272829\
                                  30313233343536373839\
                                  40414243444546474849\
                                  50515253545556575859\
                                  60616263646566676869\
                                  70717273747576777879\
                                  80818283848586878889\
                                  90919293949596979899";

/// The inline machine-integer arm: the exact `i64` and its canonical decimal spelling, both stored inline.
///
/// The spelling is fixed at construction and immutable afterwards, except for [`Integer::trim_trailing_zeroes`], which
/// rewrites both halves together. Two routes fix it — [`MachineInt::render`] derives it from the value,
/// [`MachineInt::adopt`] keeps canonical bytes the caller already holds — and the test matrix below pins them to the
/// same bytes.
#[derive(Clone, Copy, Debug)]
struct MachineInt {
    value: i64,
    text: [u8; MACHINE_TEXT_CAP],
    len: u8,
}

impl MachineInt {
    /// Derives `value`'s canonical decimal spelling into the inline buffer.
    ///
    /// The route for a value with no spelling yet (an arithmetic result, a trimmed coefficient); a caller holding
    /// canonical digits takes [`Self::adopt`]. The width is resolved up front from [`decimal_width`], so digits are
    /// written backwards at their final offsets in one pass, two per division via [`DIGIT_PAIRS`] — every arithmetic
    /// result pays this render, so it stays cheap.
    fn render(value: i64) -> Self {
        let negative = value < 0;
        let mut magnitude = value.unsigned_abs();
        let sign_len = usize::from(negative);
        let len = sign_len + decimal_width(magnitude);
        let mut text = [0_u8; MACHINE_TEXT_CAP];
        if negative {
            text[0] = b'-';
        }
        let mut cursor = len;
        while magnitude >= 100 {
            let pair = pair_offset(magnitude % 100);
            magnitude /= 100;
            cursor -= 2;
            text[cursor] = DIGIT_PAIRS[pair];
            text[cursor + 1] = DIGIT_PAIRS[pair + 1];
        }
        // One or two digits are left, and `len` already reserved room for exactly as many as `decimal_width` counted.
        if magnitude >= 10 {
            let pair = pair_offset(magnitude);
            text[sign_len] = DIGIT_PAIRS[pair];
            text[sign_len + 1] = DIGIT_PAIRS[pair + 1];
        } else {
            // `len` reserved exactly one byte for this final digit.
            text[sign_len] = b'0' + u8::try_from(magnitude).expect("the final digit is below 10");
        }
        Self {
            value,
            text,
            // The buffer holds at most `MACHINE_TEXT_CAP` bytes.
            len: u8::try_from(len).expect("the inline buffer holds at most 20 bytes"),
        }
    }

    /// Keeps `significant` (prefixed by a minus when `negative`) as the inline spelling of `value`, instead of deriving
    /// the digits a second time.
    ///
    /// The caller guarantees the digits are canonical and fit the inline buffer — a document integer is spelled once,
    /// by whoever wrote it.
    fn adopt(value: i64, negative: bool, significant: &str) -> Self {
        debug_assert!(
            usize::from(negative) + significant.len() <= MACHINE_TEXT_CAP,
            "a spelling wider than the inline buffer belongs to the heap arm"
        );
        let mut text = [0_u8; MACHINE_TEXT_CAP];
        let sign_len = usize::from(negative);
        if negative {
            text[0] = b'-';
        }
        let len = sign_len + significant.len();
        text[sign_len..len].copy_from_slice(significant.as_bytes());
        Self {
            value,
            text,
            // The caller checked the spelling against `MACHINE_TEXT_CAP`.
            len: u8::try_from(len).expect("the inline buffer holds at most 20 bytes"),
        }
    }

    /// Builds the machine arm for already-canonical signed decimal text, or `None` when the value does not qualify: the
    /// spelling must fit the inline buffer, must not be `-0` (which stays on the heap arm so its retained spelling
    /// survives [`Number::integer`]'s machine fold), and must parse as an `i64`.
    ///
    /// The accepted text is retained verbatim, so the caller must have canonicalized it first (`007` would otherwise be
    /// kept, not normalized to `7`). The guards here still refuse anything [`Self::render`] cannot emit — a leading
    /// `+` in particular, which `i64::from_str` would accept. Digits are accumulated in one pass by
    /// [`machine_from_parts`] rather than a second `i64` parse; every materialized machine integer travels this path.
    fn from_canonical(text: &str) -> Option<Self> {
        if text.len() > MACHINE_TEXT_CAP || text == "-0" {
            return None;
        }
        let (negative, significant) = match text.strip_prefix('-') {
            Some(digits) => (true, digits),
            None => (false, text),
        };
        // `machine_from_parts` declines on any non-digit byte, which is the same refusal the leading-byte guard used to
        // give a non-canonical spelling (a leading `+` cannot reach `significant` here).
        machine_from_parts(negative, significant)
    }

    fn as_str(&self) -> &str {
        // Every byte is an ASCII sign or digit, written by `render` from the value or by `adopt` from canonical bytes a
        // caller already held.
        core::str::from_utf8(&self.text[..usize::from(self.len)])
            .expect("the inline buffer holds only ASCII sign or digit bytes")
    }
}

/// The two storages an [`Integer`] may use. Over [`Integer::as_str`] the choice is invisible — both lend the same
/// canonical bytes. It IS observable elsewhere: [`Integer::as_machine`] answers only for the machine arm (a retained
/// heap-arm `-0` answers `None` despite being zero), and allocation identity differs — which is why
/// [`Integer::boxed_from_i64`] forces the heap arm to give a machine value one.
#[derive(Clone, Debug)]
enum IntegerRepr {
    Machine(MachineInt),
    Big(String),
}

/// Arbitrary-precision integer as signed decimal text.
///
/// Values in `i64` range keep the machine value inline. Wider values keep the heap spelling. `Eq`, `Hash`, and `Ord`
/// compare [`Integer::as_str`].
#[derive(Clone, Debug)]
pub struct Integer(IntegerRepr);

impl Integer {
    /// Validates that text is already in jqf's canonical integer spelling without allocating or normalizing it.
    pub(crate) fn validate_canonical(canonical: &str) -> Result<(), NumericError> {
        let bytes = canonical.as_bytes();
        let digits = match bytes.first() {
            Some(b'-') => &bytes[1..],
            Some(b'+') => return Err(NumericError::InvalidDigit),
            Some(_) => bytes,
            None => return Err(NumericError::Empty),
        };
        if digits.is_empty() {
            return Err(NumericError::Empty);
        }
        // `-0` is accepted: strict JSON decode retains a negative zero's sign for byte-exact rendering. It is still
        // semantically zero (`Integer::parse` normalizes it to `0`), while the byte-order `Eq`/`Hash`/`Ord` keep `-0`
        // and `0` distinct. A leading zero in a longer magnitude (`00`, `-00`) stays rejected.
        if !digits.iter().all(u8::is_ascii_digit) || (digits.len() > 1 && digits.first() == Some(&b'0')) {
            return Err(NumericError::InvalidDigit);
        }
        Ok(())
    }

    /// Builds the canonical integer for a machine value without allocating — the arithmetic engine's result
    /// constructor.
    #[must_use]
    pub fn from_i64(value: i64) -> Self {
        Self(IntegerRepr::Machine(MachineInt::render(value)))
    }

    /// The exact `i64` value of this integer, or `None` when it does not fit.
    ///
    /// A field read on the machine arm; `i64::from_str` on the heap arm. The retained `-0` answers `Some(0)` — it is
    /// zero.
    #[must_use]
    pub fn to_i64(&self) -> Option<i64> {
        match &self.0 {
            IntegerRepr::Machine(machine) => Some(machine.value),
            IntegerRepr::Big(text) => {
                // `i64::MIN` is the widest spelling that fits (19 digits plus a sign), so a longer one declines before
                // paying a parse.
                if text.len() > 20 || (text.len() == 20 && !text.starts_with('-')) {
                    return None;
                }
                text.parse::<i64>().ok()
            }
        }
    }

    /// A BOXED (heap-arm) integer holding `value`'s canonical spelling.
    ///
    /// Path mode uses this to give a COMPUTED machine integer an allocation identity — the heap arm is the one arm
    /// that always allocates.
    #[must_use]
    pub fn boxed_from_i64(value: i64) -> Self {
        let machine = MachineInt::render(value);
        Self(IntegerRepr::Big(String::from(machine.as_str())))
    }

    /// The value of the MACHINE arm, when this integer is one.
    ///
    /// The arm discriminator, distinct from [`Self::to_i64`]: a heap-arm `-0` answers `Some(0)` to `to_i64` but is NOT
    /// a machine integer — its retained spelling must survive.
    #[must_use]
    pub(crate) fn as_machine(&self) -> Option<i64> {
        match &self.0 {
            IntegerRepr::Machine(machine) => Some(machine.value),
            IntegerRepr::Big(_) => None,
        }
    }

    /// Parse a signed base-ten integer. Leading zeroes are dropped.
    ///
    /// A leading `+` is accepted here. The decoder still owns its own number grammar.
    pub fn parse(spelling: &str) -> Result<Self, NumericError> {
        let (negative, digits) = match spelling.as_bytes().first() {
            Some(b'-') => (true, &spelling[1..]),
            Some(b'+') => (false, &spelling[1..]),
            Some(_) => (false, spelling),
            None => return Err(NumericError::Empty),
        };
        if digits.is_empty() {
            return Err(NumericError::Empty);
        }
        let significant = digits.trim_start_matches('0');
        if significant.is_empty() {
            // `trim_start_matches` consumed the whole magnitude, so every byte of it was the digit `0`.
            return Ok(Self::from_i64(0));
        }
        // `machine_from_parts` validates and converts in one pass; the heap arm below only copies bytes, so it keeps
        // the explicit digit check.
        if let Some(machine) = machine_from_parts(negative, significant) {
            return Ok(Self(IntegerRepr::Machine(machine)));
        }
        if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(NumericError::InvalidDigit);
        }
        let mut canonical = String::new();
        canonical
            .try_reserve_exact(significant.len() + usize::from(negative))
            .map_err(|_| NumericError::Allocation)?;
        if negative {
            canonical.push('-');
        }
        canonical.push_str(significant);
        Ok(Self(IntegerRepr::Big(canonical)))
    }

    /// Constructs an integer from canonical signed decimal text.
    pub fn from_canonical(canonical: String) -> Result<Self, NumericError> {
        Self::validate_canonical(&canonical)?;
        Self::from_validated_canonical(Cow::Owned(canonical))
    }

    /// Constructs an integer from BORROWED canonical signed decimal text, allocating only when the value lands in the
    /// big-integer arm. The document-decode path uses this: a materialized machine integer pays no heap copy at all,
    /// where `from_canonical` would clone the text first.
    pub(crate) fn from_canonical_ref(canonical: &str) -> Result<Self, NumericError> {
        Self::validate_canonical(canonical)?;
        Self::from_validated_canonical(Cow::Borrowed(canonical))
    }

    /// Canonicalizes an owned signed decimal digit buffer without copying it.
    ///
    /// The caller has already checked that the buffer contains an optional leading minus sign followed by at least one
    /// ASCII digit.
    pub(crate) fn from_decimal_digits(mut digits: String) -> Self {
        // The caller's contract is an optional minus followed by at least one ASCII digit; a sign-only or empty buffer
        // would silently fold to zero below instead of naming the broken precondition.
        debug_assert!(
            digits.len() > usize::from(digits.starts_with('-')),
            "a digit buffer must carry at least one digit after its sign"
        );
        let sign_len = usize::from(digits.starts_with('-'));
        let significant = digits[sign_len..].bytes().position(|byte| byte != b'0');
        let Some(significant) = significant else {
            return Self::from_i64(0);
        };
        if significant != 0 {
            let start = sign_len + significant;
            digits.drain(sign_len..start);
        }
        // Both arms consume the owned buffer without a fresh allocation (the machine arm reads it in place, the heap
        // arm moves it), so the fallible constructor below cannot answer `Allocation` here.
        Self::from_validated_canonical(Cow::Owned(digits))
            .expect("an owned buffer reaches either arm without allocating")
    }

    /// Picks the storage arm for text already known to be canonical — the module doc's arm-as-function law, with
    /// [`Self::boxed_from_i64`] as its one exception.
    ///
    /// Fallible because the heap arm owns its spelling: borrowed text must be copied into it, and that copy is reserved
    /// up front so an allocator refusal answers [`NumericError::Allocation`] instead of aborting.
    fn from_validated_canonical(canonical: Cow<'_, str>) -> Result<Self, NumericError> {
        if let Some(machine) = MachineInt::from_canonical(&canonical) {
            return Ok(Self(IntegerRepr::Machine(machine)));
        }
        match canonical {
            Cow::Owned(text) => Ok(Self(IntegerRepr::Big(text))),
            Cow::Borrowed(text) => {
                let mut owned = String::new();
                owned
                    .try_reserve_exact(text.len())
                    .map_err(|_| NumericError::Allocation)?;
                owned.push_str(text);
                Ok(Self(IntegerRepr::Big(owned)))
            }
        }
    }

    /// Removes every trailing zero from a nonzero canonical integer in place.
    ///
    /// A zero reports nothing removed and is left alone: its only digit IS a trailing zero, so trimming it would mint
    /// the uncanonical `""` or `"-"`.
    pub(crate) fn trim_trailing_zeroes(&mut self) -> usize {
        if matches!(self.as_str(), "0" | "-0") {
            return 0;
        }
        let trimmed = {
            let text = self.as_str();
            let sign_len = usize::from(text.starts_with('-'));
            text[sign_len..].bytes().rev().take_while(|byte| *byte == b'0').count()
        };
        if trimmed == 0 {
            return 0;
        }
        match &mut self.0 {
            IntegerRepr::Machine(machine) => {
                let mut value = machine.value;
                for _ in 0..trimmed {
                    value /= 10;
                }
                *machine = MachineInt::render(value);
            }
            IntegerRepr::Big(text) => {
                text.truncate(text.len() - trimmed);
                // The trim can leave a magnitude that now fits the inline arm (`10…0` with twenty-odd digits trims to
                // `1`). It must move there, or two equal integers would sit in different arms.
                if let Some(machine) = MachineInt::from_canonical(text) {
                    self.0 = IntegerRepr::Machine(machine);
                }
            }
        }
        trimmed
    }

    /// Returns canonical signed decimal text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            IntegerRepr::Machine(machine) => machine.as_str(),
            IntegerRepr::Big(text) => text,
        }
    }
}

/// How many decimal digits `magnitude` spells: `1` for zero, `⌊log10⌋ + 1` otherwise. At most 20, so an `i64`
/// magnitude plus a sign always fits [`MACHINE_TEXT_CAP`].
fn decimal_width(magnitude: u64) -> usize {
    // `checked_ilog10` answers `None` only for zero, which spells one digit.
    magnitude.checked_ilog10().map_or(1, |log| log as usize + 1)
}

/// Where the two-digit spelling of `value` starts in [`DIGIT_PAIRS`].
///
/// Every caller has already reduced `value` below 100, so the offset is inside the table; the assert makes a future
/// caller that forgets fail loudly in debug instead of silently rendering a wrong pair.
fn pair_offset(value: u64) -> usize {
    debug_assert!(value < 100, "pair_offset is defined for two-digit values");
    usize::try_from(value).expect("a two-digit pair fits usize") * 2
}

/// The machine arm for a canonical `(sign, significant digits)` pair, without building the joined spelling first.
///
/// `None` means only that this arm cannot hold the spelling — too wide, past `i64`, or not digits at all; the caller
/// decides which. The magnitude is accumulated by hand because `u64::from_str` accepts a leading `+`, which the
/// canonical grammar rejects.
fn machine_from_parts(negative: bool, significant: &str) -> Option<MachineInt> {
    if significant.len() + usize::from(negative) > MACHINE_TEXT_CAP {
        return None;
    }
    let mut magnitude: u64 = 0;
    for byte in significant.bytes() {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
        magnitude = magnitude.checked_mul(10)?.checked_add(u64::from(digit))?;
    }
    let value = if negative {
        // `i64::MIN`'s magnitude has no positive counterpart, so negate in the unsigned domain and cast the result.
        0_i64.checked_sub_unsigned(magnitude)?
    } else {
        i64::try_from(magnitude).ok()?
    };
    Some(MachineInt::adopt(value, negative, significant))
}

impl PartialEq for Integer {
    // Byte-order equality over the canonical spelling (module doc): the retained `-0` and `0` are distinct spellings
    // and compare UNEQUAL here; semantic equality is defused at the comparison layer, never here.
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Integer {}

impl core::hash::Hash for Integer {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl Ord for Integer {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl PartialOrd for Integer {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::{Integer, NumericError};
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::hash::{Hash, Hasher};

    /// A deterministic FNV-1a hasher: `core::hash::Hash` needs a `Hasher`, and this crate is `no_std`.
    #[derive(Default)]
    struct Fnv(u64);

    impl Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            let mut state = if self.0 == 0 { 0xcbf2_9ce4_8422_2325 } else { self.0 };
            for byte in bytes {
                state ^= u64::from(*byte);
                state = state.wrapping_mul(0x0000_0100_0000_01b3);
            }
            self.0 = state;
        }
    }

    fn hash_of(integer: &Integer) -> u64 {
        let mut hasher = Fnv::default();
        integer.hash(&mut hasher);
        hasher.finish()
    }

    /// The blindness matrix's value set: zero, small magnitudes, the ±10^k digit boundaries, 2^53±1, the `i64`
    /// extremes, one step beyond each, the 19-versus-20-digit fits/doesn't-fit pair, a magnitude far past `i64`, and
    /// the retained `-0` that is the ONLY spelling the machine arm refuses.
    ///
    /// Each row is `(canonical spelling, the exact i64 value or None)`.
    const MATRIX: &[(&str, Option<i64>)] = &[
        ("0", Some(0)),
        ("1", Some(1)),
        ("-1", Some(-1)),
        ("9", Some(9)),
        ("-9", Some(-9)),
        ("10", Some(10)),
        ("-10", Some(-10)),
        ("99", Some(99)),
        ("100", Some(100)),
        ("-100", Some(-100)),
        ("1000", Some(1000)),
        ("9007199254740991", Some(9_007_199_254_740_991)),
        ("9007199254740992", Some(9_007_199_254_740_992)),
        ("9007199254740993", Some(9_007_199_254_740_993)),
        ("-9007199254740993", Some(-9_007_199_254_740_993)),
        // 18 digits, 19 digits: both fit.
        ("999999999999999999", Some(999_999_999_999_999_999)),
        ("1000000000000000000", Some(1_000_000_000_000_000_000)),
        // The extremes: `i64::MAX` is 19 digits, `i64::MIN` 19 plus a sign.
        ("9223372036854775807", Some(i64::MAX)),
        ("-9223372036854775808", Some(i64::MIN)),
        // One step beyond each: a 19-digit magnitude that does NOT fit, and a 20-character spelling that does not
        // either.
        ("9223372036854775808", None),
        ("-9223372036854775809", None),
        // A 20-digit magnitude: fits the inline BUFFER, not the value.
        ("99999999999999999999", None),
        ("12345678901234567890123456789", None),
        // The retained negative zero: canonical, semantically zero, and refused by the machine arm so it can keep its
        // own distinct bytes.
        ("-0", Some(0)),
    ];

    /// Every construction route that reaches an integer must agree, byte for byte and arm for arm, with every other one
    /// — pinning the module doc's arm-as-function law.
    ///
    /// The test also pins the two ways a machine arm fixes its spelling to each other: `from_i64` DERIVES the digits
    /// from the value and every other route KEEPS the digits it was handed, and the `from_i64` row asserts they land on
    /// the same bytes for every value in the matrix. The `as_machine` assertions are the arm half: every route lands on
    /// the machine arm exactly when the value fits `i64`, except `from_canonical`'s retained `-0`, which must stay on
    /// the heap arm so [`Number::integer`] cannot fold it onto the inline zero.
    #[test]
    fn every_construction_route_agrees_on_bytes_arm_and_value() {
        for (canonical, value) in MATRIX {
            let from_canonical = Integer::from_canonical(String::from(*canonical)).expect("canonical");
            assert_eq!(from_canonical.as_str(), *canonical, "from_canonical {canonical}");
            assert_eq!(from_canonical.to_i64(), *value, "to_i64 {canonical}");
            assert_eq!(
                from_canonical.as_machine(),
                if *canonical == "-0" { None } else { *value },
                "from_canonical arm {canonical}"
            );

            // `parse` normalizes `-0` to `0` (the program-literal law) and is otherwise the identity on canonical text.
            let parsed = Integer::parse(canonical).expect("parse");
            let parsed_expected = if *canonical == "-0" { "0" } else { *canonical };
            assert_eq!(parsed.as_str(), parsed_expected, "parse {canonical}");
            assert_eq!(parsed.as_machine(), *value, "parse arm {canonical}");

            // `from_decimal_digits` owns an already-signed digit buffer, and must land on the same value from a
            // redundantly-zero-padded spelling.
            let padded = {
                let mut text = String::new();
                let unsigned = canonical.strip_prefix('-');
                if unsigned.is_some() {
                    text.push('-');
                }
                text.push_str("000");
                text.push_str(unsigned.unwrap_or(canonical));
                text
            };
            let from_digits = Integer::from_decimal_digits(padded);
            assert_eq!(from_digits.as_str(), parsed_expected, "digits {canonical}");
            assert_eq!(from_digits.as_machine(), *value, "digits arm {canonical}");

            // `from_i64` is the arithmetic result constructor; where the value fits, it must produce the same integer
            // as every other route.
            if let Some(value) = *value {
                let from_i64 = Integer::from_i64(value);
                assert_eq!(from_i64.as_str(), parsed_expected, "from_i64 {canonical}");
                assert_eq!(from_i64, parsed, "from_i64 == parse {canonical}");
                assert_eq!(hash_of(&from_i64), hash_of(&parsed), "hash {canonical}");
                assert_eq!(from_i64.to_i64(), Some(value), "round trip {canonical}");
                assert_eq!(from_i64.as_machine(), Some(value), "from_i64 arm {canonical}");
            }

            // A clone is byte-, value- and hash-identical in either arm.
            let cloned = from_canonical.clone();
            assert_eq!(cloned.as_str(), from_canonical.as_str(), "clone {canonical}");
            assert_eq!(cloned, from_canonical, "clone eq {canonical}");
            assert_eq!(hash_of(&cloned), hash_of(&from_canonical), "clone hash {canonical}");
        }
    }

    /// `Ord` keeps the byte order the derived `String` implementation had — it is a STORAGE order, not the numeric
    /// one (the numeric order is the engine's `total_cmp`, which never consults this).
    #[test]
    fn ordering_is_the_canonical_byte_order_across_both_arms() {
        let mut sorted: Vec<Integer> = MATRIX
            .iter()
            .map(|(text, _)| Integer::from_canonical(String::from(*text)).expect("canonical"))
            .collect();
        sorted.sort();
        let mut previous: Option<String> = None;
        for integer in &sorted {
            if let Some(previous) = &previous {
                assert!(
                    previous.as_str() <= integer.as_str(),
                    "byte order broken at {}",
                    integer.as_str()
                );
            }
            previous = Some(String::from(integer.as_str()));
        }
        // The one pair with equal VALUE and different BYTES stays distinct here, exactly as it did when the storage was
        // a bare `String`.
        let machine_zero = Integer::from_i64(0);
        let retained_negative_zero = Integer::from_canonical(String::from("-0")).expect("canonical");
        assert_ne!(machine_zero, retained_negative_zero);
        assert_eq!(machine_zero.to_i64(), retained_negative_zero.to_i64());
    }

    /// `parse` accepts and rejects the same spellings whichever arm answers, and names the same error for each
    /// rejection.
    ///
    /// The machine arm validates its OWN digits — the magnitude accumulator refuses every byte that is not one —
    /// and the arbitrary-precision arm, which only copies bytes, keeps an explicit check. Two validators is two chances
    /// to disagree, so every rejected magnitude is exercised in a short form and a wide form: once short enough for the
    /// machine arm to look at, and once padded past the inline buffer so only the wide arm sees it.
    #[test]
    fn parse_rejects_the_same_spellings_in_both_arms() {
        // A magnitude — the part after the one optional leading sign — that is not digits. `+5` and `-5` are here
        // as MAGNITUDES, so the spellings built from them carry a second sign, and that pair is the one that bites:
        // `u64::from_str` ACCEPTS a leading `+`, so an arm that delegated its digit check to it would quietly read
        // `-+5` as `-5`.
        let rejected = [
            "a", "1a", "1 ", " 1", "1_0", "1+2", "0x10", "1.5", "1e5", "١٢٣", "+5", "-5",
        ];
        for magnitude in rejected {
            for sign in ["-", "+"] {
                let short = alloc::format!("{sign}{magnitude}");
                assert_eq!(Integer::parse(&short), Err(NumericError::InvalidDigit), "{short}");
                // The same magnitude past the inline buffer, so the machine arm declines on WIDTH before it reads a
                // byte of it.
                let wide = alloc::format!("{sign}123456789012345678901234567890{magnitude}");
                assert_eq!(Integer::parse(&wide), Err(NumericError::InvalidDigit), "{wide}");
            }
            // And unsigned, for the magnitudes that are a whole spelling on their own.
            if !magnitude.starts_with(['+', '-']) {
                assert_eq!(
                    Integer::parse(magnitude),
                    Err(NumericError::InvalidDigit),
                    "{magnitude}"
                );
            }
        }

        // No digits at all, in every sign shape.
        for empty in ["", "-", "+"] {
            assert_eq!(Integer::parse(empty), Err(NumericError::Empty), "{empty}");
        }

        // The accepted neighbours of those rejections: a sign belongs at the FRONT of a spelling, and redundant leading
        // zeroes normalize away in both arms.
        let accepted = [
            ("+5", "5"),
            ("-5", "-5"),
            ("007", "7"),
            ("-007", "-7"),
            ("+007", "7"),
            ("-0", "0"),
            ("+0", "0"),
            ("-0000", "0"),
            ("0000000000000000000000000000000000000005", "5"),
            ("0000000000000000000012345678901234567890", "12345678901234567890"),
        ];
        for (spelling, canonical) in accepted {
            let parsed = Integer::parse(spelling).expect("accepted");
            assert_eq!(parsed.as_str(), canonical, "{spelling}");
        }
    }

    /// `from_canonical` KEEPS the bytes it accepts, so a non-canonical spelling must be refused rather than retained.
    ///
    /// `+5` is the row that bites: `i64::from_str` accepts a leading `+`, so an arm that trusted it would answer five
    /// while RETAINING the bytes `+5` — and the retained spelling is exactly what `Eq`, `Hash` and `Ord` read, so
    /// that integer would compare unequal to every other five. `007` is the same defect spelled with redundant zeroes.
    /// Each appears twice, once short enough for the machine arm to look at and once padded past the inline buffer so
    /// only the wide arm sees it.
    #[test]
    fn from_canonical_refuses_non_canonical_spellings_in_both_arms() {
        let signed = ["+5", "+0000000000000000000000000000005"];
        let zero_padded = ["007", "0000000000000000000000000000007"];
        for spelling in signed.iter().chain(&zero_padded) {
            assert_eq!(
                Integer::from_canonical(String::from(*spelling)),
                Err(NumericError::InvalidDigit),
                "{spelling}"
            );
        }
        // The machine arm keeps its own guard against the sign, because it is the layer a future caller could reach
        // without `validate_canonical` in front of it and the only one `i64::from_str`'s laxity could fool. A redundant
        // leading zero is NOT this layer's to catch — `0` is a byte `render` can emit, so the spelling is refused
        // above, at the validator that knows what a whole canonical magnitude looks like.
        for spelling in signed {
            assert!(
                super::MachineInt::from_canonical(spelling).is_none(),
                "machine arm kept {spelling}"
            );
        }
    }

    /// `render` derives the same bytes a digit-at-a-time reference does, for every width, every digit in every
    /// position, and both signs.
    ///
    /// The construction matrix above pins `render` against `adopt` for the 24 values it lists; this pins it against an
    /// INDEPENDENT spelling over a set wide enough to catch an off-by-one in the width, a transposed pair, or a digit
    /// left in a staging buffer — the three ways a two-digits-at-a-time render can be wrong while still agreeing on
    /// short values.
    #[test]
    fn render_agrees_with_a_digit_at_a_time_reference() {
        /// The obvious spelling: one division per digit, reversed at the end.
        fn reference(value: i64) -> String {
            let mut digits = Vec::new();
            let mut magnitude = value.unsigned_abs();
            loop {
                digits.push(b'0' + u8::try_from(magnitude % 10).expect("one digit"));
                magnitude /= 10;
                if magnitude == 0 {
                    break;
                }
            }
            digits.reverse();
            let mut text = String::new();
            if value < 0 {
                text.push('-');
            }
            text.push_str(core::str::from_utf8(&digits).expect("ascii digits"));
            text
        }

        let mut values: Vec<i64> = (-1000..=1000).collect();
        // Every power-of-ten boundary and its neighbours, both signs: the widths where a `⌊log10⌋ + 1` width is
        // easiest to get wrong.
        let mut power: i64 = 1;
        loop {
            for offset in [-1, 0, 1] {
                if let Some(value) = power.checked_add(offset) {
                    values.push(value);
                    values.push(-value);
                }
            }
            match power.checked_mul(10) {
                Some(next) => power = next,
                None => break,
            }
        }
        // The extremes, `2^53` either side, and a value with a zero in every interior position (a staged-buffer bug
        // survives values without one).
        values.extend([
            i64::MAX,
            i64::MAX - 1,
            i64::MIN,
            i64::MIN + 1,
            9_007_199_254_740_991,
            9_007_199_254_740_993,
            102_030_405_060_708_090,
            -102_030_405_060_708_090,
            100_000_000_000_000_000,
        ]);
        for value in values {
            let rendered = super::MachineInt::render(value);
            assert_eq!(rendered.as_str(), reference(value), "render {value}");
            assert_eq!(rendered.value, value, "value {value}");
        }
    }

    /// The fourth construction site: trimming can move a value BETWEEN arms.
    #[test]
    fn trimming_trailing_zeroes_re_picks_the_arm() {
        // A heap-arm coefficient whose trim lands it inside `i64` range must move to the machine arm, or two equal
        // integers would sit in different arms and the arm-as-function law would break.
        let mut wide = Integer::from_canonical(String::from("10000000000000000000000000")).expect("canonical");
        assert_eq!(wide.to_i64(), None);
        assert_eq!(wide.trim_trailing_zeroes(), 25);
        assert_eq!(wide.as_str(), "1");
        assert_eq!(wide.to_i64(), Some(1));
        assert_eq!(wide.as_machine(), Some(1), "the trim re-picks the machine arm");
        assert_eq!(wide, Integer::from_i64(1));
        assert_eq!(hash_of(&wide), hash_of(&Integer::from_i64(1)));

        // A machine-arm trim updates value and spelling together.
        let mut machine = Integer::from_i64(-102_000);
        assert_eq!(machine.trim_trailing_zeroes(), 3);
        assert_eq!(machine.as_str(), "-102");
        assert_eq!(machine.to_i64(), Some(-102));
        assert_eq!(machine.as_machine(), Some(-102));

        // Nothing to trim leaves both halves alone.
        let mut untouched = Integer::from_i64(-9_223_372_036_854_775_807);
        assert_eq!(untouched.trim_trailing_zeroes(), 0);
        assert_eq!(untouched.as_str(), "-9223372036854775807");

        // A heap-arm trim that stays out of `i64` range keeps the heap arm.
        let mut still_wide =
            Integer::from_canonical(String::from("123456789012345678901234567890000")).expect("canonical");
        assert_eq!(still_wide.trim_trailing_zeroes(), 4);
        assert_eq!(still_wide.as_str(), "12345678901234567890123456789");
        assert_eq!(still_wide.to_i64(), None);
        assert_eq!(still_wide.as_machine(), None);

        // A zero's only digit IS a trailing zero, so trimming it would mint the uncanonical `""` or `"-"`; both
        // spellings are left whole instead.
        let mut zero = Integer::from_i64(0);
        assert_eq!(zero.trim_trailing_zeroes(), 0);
        assert_eq!(zero.as_str(), "0");
        let mut negative_zero = Integer::from_canonical_ref("-0").expect("canonical");
        assert_eq!(negative_zero.trim_trailing_zeroes(), 0);
        assert_eq!(negative_zero.as_str(), "-0");
    }

    /// `boxed_from_i64` is the one deliberate exception to the arm-as-function law: a machine VALUE on the Big arm, so
    /// path mode's allocation-identity check sees a computed number as an independent object. The value, spelling,
    /// equality and hash contracts of the arm it does NOT take must still hold.
    #[test]
    fn boxed_from_i64_keeps_a_machine_value_on_the_heap_arm() {
        let boxed = Integer::boxed_from_i64(5);
        assert_eq!(boxed.as_str(), "5");
        assert_eq!(boxed.to_i64(), Some(5));
        assert_eq!(boxed, Integer::from_i64(5));
        assert_eq!(hash_of(&boxed), hash_of(&Integer::from_i64(5)));
        // The arm is the whole point: the inline integer shares no allocation, and `boxed_from_i64` exists to give a
        // computed number one anyway.
        assert_eq!(boxed.as_machine(), None, "boxed_from_i64 never takes the machine arm");
    }
}
