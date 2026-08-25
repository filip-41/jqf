//! The search-and-split laws over strings and arrays, shared by more than one caller.
//!
//! One job: answer WHERE a needle sits inside a haystack, and cut a string on a separator. Three laws live here rather
//! than in a builtin module because each has two callers: the codepoint search backs both `_strindices/1` and the
//! string half of `indices/1`, the subsequence search backs `indices/1`'s array half at two needle shapes, and the
//! split law is what the `/` operator and `split/1` BOTH are — `split($s)` is `./$s` for the one-argument form, so a
//! second spelling of the cut would be a second law to keep in step.
//!
//! Every search here is OVERLAPPING, not the non-overlapping answer `str::match_indices` gives: `"aaaa" |
//! indices("aa")` is `[0,1,2]` and `[1,1,1] | indices([1,1])` is `[0,1]`. A non-overlapping scan would drop the middle
//! position of each.
//!
//! String positions count CODEPOINTS and not bytes, which the compat corpus pins directly — `"здравствуй
//! мир!" | index("!")` is `14` over 25 bytes, and `"🇬🇧oo" | indices("o")` is `[2,3]` because the flag is two
//! regional indicators. Byte offsets come out of the search and are converted on the way past; the conversion walks the
//! haystack once in total, because match offsets only ever move forward.
//!
//! Array positions compare by VALUE equality and not by representation, so `[0,1.0,2] | indices(1)` is `[1]`. That
//! equality is [`crate::semantics::order`]'s, which means the search inherits its depth guard for free — and
//! `indices` inherits it, at the same cap and with the same wording: at nesting a program builds (a document cannot
//! carry it, the decoder refusing past the same depth), the answer is `[0]` at 9 000 and `Equality check too deep` at
//! 12 000.
//!
//! Negative space: it owns no type dispatch and no message text — which needle shape reaches which law, and what a
//! wrong shape says, is the calling builtin's; and it owns no regex, which is a later vertical's `_match_impl`.
//!
//! The unit tests below pin the three laws' shape rules against a charged [`ResourceContext`], the way
//! [`crate::semantics::order`] and [`crate::semantics::binary`] pin theirs. They are the second oracle, not the first:
//! what each row ANSWERS is settled by the byte-level compat corpus, and these cases restate the answers a refactor
//! could quietly lose.

use alloc::string::String;

use jqf_data::{Array, Integer, Number, Value, ValueAllocationError};
use jqf_resource::ResourceContext;

use super::depth::equality_message;
use super::order::semantic_eq;
use crate::error::EngineRunError;
use crate::semantics::path::raise;

/// The CODEPOINT positions at which `needle` occurs in `haystack`, overlapping.
///
/// The empty needle answers the empty array rather than every position, which is the answer and the only one that
/// terminates: `"abc" | indices("")` is `[]`.
///
/// # Errors
///
/// Returns an allocation failure when growing the position array fails.
pub fn codepoint_indices(
    haystack: &str,
    needle: &str,
    _resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut positions = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    if needle.is_empty() {
        return Ok(Value::Array(positions));
    }
    // Two cursors over the same haystack: `from` is where the next byte-level search starts and `counted` is how far
    // the codepoint tally has walked.
    // Both only ever advance, so the tally costs one pass over the haystack no matter how many matches land.
    let mut from = 0_usize;
    let mut counted = 0_usize;
    let mut codepoint = 0_i64;
    while let Some(relative) = haystack[from..].find(needle) {
        let offset = from + relative;
        codepoint += i64::try_from(haystack[counted..offset].chars().count()).unwrap_or(i64::MAX);
        counted = offset;
        positions
            .try_push(position(codepoint))
            .map_err(|_| EngineRunError::allocation_failure())?;
        // Advance ONE codepoint, not one match: the search overlaps.
        from = offset
            + haystack[offset..]
                .chars()
                .next()
                .expect("a str match starts at a char boundary")
                .len_utf8();
    }
    Ok(Value::Array(positions))
}

/// The positions at which the `needle` SEQUENCE occurs in `haystack`, overlapping.
///
/// The empty needle answers the empty array, matching the string law (`[1,2] | indices([])` is `[]`).
///
/// # Errors
///
/// Returns `Equality check too deep` when an element pair nests past [`crate::semantics::depth`]'s comparison cap, or
/// an allocation failure.
pub fn subsequence_indices(
    haystack: &Array,
    needle: &Array,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let mut positions = Array::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    let (Some(last_start), false) = (haystack.len().checked_sub(needle.len()), needle.is_empty()) else {
        return Ok(Value::Array(positions));
    };
    // One-element string needles are the `index`/`rindex` hot shape (set membership spellings scan an array with
    // `index($v)` per element). A string only ever equals a string, by UTF-8 bytes, so the per-position comparison is a
    // byte compare on the untagged payload instead of the full total-order dispatch. The guard is `len() == 1` and not
    // merely a string FIRST element: a multi-element needle must compare every element (`["a","x","a","b"] |
    // indices(["a","b"])` is `[2]`, and the first-element-only scan answers `[0,2]`). Every other shape takes the
    // general walk.
    if needle.len() == 1
        && let Some(Value::String(wanted)) = needle.get(0)
    {
        for start in 0..=last_start {
            let Some(found) = haystack.get(start) else {
                break;
            };
            let matches = matches!(found.untagged(), Value::String(candidate) if candidate.as_str() == wanted.as_str());
            if !matches {
                continue;
            }
            let at = i64::try_from(start).unwrap_or(i64::MAX);
            positions
                .try_push(position(at))
                .map_err(|_| EngineRunError::allocation_failure())?;
        }
        return Ok(Value::Array(positions));
    }
    for start in 0..=last_start {
        if !matches_at(haystack, needle, start, resources)? {
            continue;
        }
        let at = i64::try_from(start).unwrap_or(i64::MAX);
        positions
            .try_push(position(at))
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    Ok(Value::Array(positions))
}

/// The FIRST position at which `needle` occurs as a sequence in `haystack`, or `null` — `indices | .[0]`, computed
/// without materializing the position array and with the scan STOPPING at the first match. The one-element string
/// needle keeps `subsequence_indices`'s byte-compare fast path, and every other shape walks `matches_at` with the same
/// depth-guarded equality.
/// An empty needle answers `null`, matching `indices([])` then `.[0]`.
///
/// # Errors
///
/// Returns `Equality check too deep` when an element pair nests past the comparison cap, exactly as `indices` would.
pub fn first_subsequence_position(
    haystack: &Array,
    needle: &Array,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let (Some(last_start), false) = (haystack.len().checked_sub(needle.len()), needle.is_empty()) else {
        return Ok(Value::Null);
    };
    if needle.len() == 1
        && let Some(Value::String(wanted)) = needle.get(0)
    {
        for start in 0..=last_start {
            let Some(found) = haystack.get(start) else {
                break;
            };
            let matches = matches!(
                found.untagged(),
                Value::String(candidate) if candidate.as_str() == wanted.as_str()
            );
            if matches {
                let at = i64::try_from(start).unwrap_or(i64::MAX);
                return Ok(position(at));
            }
        }
        return Ok(Value::Null);
    }
    for start in 0..=last_start {
        if matches_at(haystack, needle, start, resources)? {
            let at = i64::try_from(start).unwrap_or(i64::MAX);
            return Ok(position(at));
        }
    }
    Ok(Value::Null)
}

/// The LAST position at which `needle` occurs as a sequence in `haystack`, or `null` — `indices | .[-1:][0]`,
/// scanning from the END and stopping at the first match found there. Same laws as [`first_subsequence_position`]: the
/// one-element string fast path, the depth-guarded general walk, and `null` for an empty needle (which `indices([])`
/// then `.[-1:][0]` also answers).
///
/// # Errors
///
/// Returns `Equality check too deep` when an element pair nests past the comparison cap, exactly as `indices` would.
pub fn last_subsequence_position(
    haystack: &Array,
    needle: &Array,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let (Some(last_start), false) = (haystack.len().checked_sub(needle.len()), needle.is_empty()) else {
        return Ok(Value::Null);
    };
    if needle.len() == 1
        && let Some(Value::String(wanted)) = needle.get(0)
    {
        for start in (0..=last_start).rev() {
            let Some(found) = haystack.get(start) else {
                continue;
            };
            let matches = matches!(
                found.untagged(),
                Value::String(candidate) if candidate.as_str() == wanted.as_str()
            );
            if matches {
                let at = i64::try_from(start).unwrap_or(i64::MAX);
                return Ok(position(at));
            }
        }
        return Ok(Value::Null);
    }
    for start in (0..=last_start).rev() {
        if matches_at(haystack, needle, start, resources)? {
            let at = i64::try_from(start).unwrap_or(i64::MAX);
            return Ok(position(at));
        }
    }
    Ok(Value::Null)
}

/// The FIRST codepoint position of `needle` in `haystack`, or `null` — the string half of `indices | .[0]`, stopping
/// at the first match instead of materializing the position array. Empty needle answers `null`, matching `indices("")`
/// then `.[0]`.
pub fn first_codepoint_position(haystack: &str, needle: &str) -> Value {
    if needle.is_empty() {
        return Value::Null;
    }
    let Some(relative) = haystack.find(needle) else {
        return Value::Null;
    };
    let codepoint = i64::try_from(haystack[..relative].chars().count()).unwrap_or(i64::MAX);
    position(codepoint)
}

/// The LAST codepoint position of `needle` in `haystack`, or `null` — the string half of `indices | .[-1:][0]`. The
/// forward cursor loop keeps the last match (an end-first `rfind` would still owe the codepoint tally over the prefix,
/// so there is nothing to stop early); the win is the position array never existing. Empty needle answers `null`.
pub fn last_codepoint_position(haystack: &str, needle: &str) -> Value {
    if needle.is_empty() {
        return Value::Null;
    }
    let mut from = 0_usize;
    let mut counted = 0_usize;
    let mut codepoint = 0_i64;
    let mut last: Option<i64> = None;
    while let Some(relative) = haystack[from..].find(needle) {
        let offset = from + relative;
        codepoint += i64::try_from(haystack[counted..offset].chars().count()).unwrap_or(i64::MAX);
        counted = offset;
        last = Some(codepoint);
        // Advance ONE codepoint, not one match: the search overlaps.
        from = offset
            + haystack[offset..]
                .chars()
                .next()
                .expect("a str match starts at a char boundary")
                .len_utf8();
    }
    last.map_or(Value::Null, position)
}

/// Whether every element of `needle` equals the `haystack` element above `start`.
fn matches_at(
    haystack: &Array,
    needle: &Array,
    start: usize,
    resources: &ResourceContext<'_>,
) -> Result<bool, EngineRunError> {
    for (offset, wanted) in needle.iter().enumerate() {
        let Some(found) = haystack.get(start + offset) else {
            return Ok(false);
        };
        if !semantic_eq(found, wanted).map_err(|_| raise(equality_message(), resources))? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The pieces of `text` cut on `separator`.
///
/// An empty separator cuts into codepoints and an empty input is the empty array — both of which are the answers
/// rather than `str::split`'s, which would give `[""]` for the second. Otherwise the pieces are an ordinary split, so a
/// separator at either end contributes an empty piece and `"abc"/"abc"` is `["",""]`.
///
/// # Errors
///
/// Returns an allocation failure when growing the piece array or a piece fails.
pub fn split(text: &str, separator: &str, _resources: &ResourceContext<'_>) -> Result<Value, ValueAllocationError> {
    let mut pieces = Array::try_new()?;
    if text.is_empty() {
        return Ok(Value::Array(pieces));
    }
    if separator.is_empty() {
        let mut piece = String::new();
        for character in text.chars() {
            piece.clear();
            piece.push(character);
            pieces.try_push(Value::try_string(&piece)?)?;
        }
    } else {
        for piece in text.split(separator) {
            pieces.try_push(Value::try_string(piece)?)?;
        }
    }
    Ok(Value::Array(pieces))
}

/// One position as a value.
fn position(at: i64) -> Value {
    Value::Number(Number::integer(Integer::from_i64(at)))
}

#[cfg(test)]
mod tests {
    use super::{
        codepoint_indices, first_codepoint_position, first_subsequence_position, last_codepoint_position,
        last_subsequence_position, split, subsequence_indices,
    };
    use alloc::string::String;
    use jqf_data::{Array, Number, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    use crate::semantics::render::to_json;

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger: a position array and every piece of a split are charged at their own construction,
    /// so no law answers without an account.
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    /// The compact JSON of an answered value.
    fn dump(value: &Value) -> String {
        to_json(value).expect("the answer renders")
    }

    /// An array of the given number spellings.
    fn numbers(spellings: &[&str]) -> Array {
        let _resources = ledger();
        let mut array = Array::try_new().expect("fixture array");
        for spelling in spellings {
            let number = Number::try_json_literal(spelling).expect("literal");
            array.try_push(Value::Number(number)).expect("fixture element");
        }
        array
    }

    /// The FIRST/LAST position scans agree with `indices | .[0]` and `indices | .[-1:][0]` on every shape the position
    /// array serves, and stop early where the array would not.
    ///
    /// The discriminating row is the multi-element STRING needle: the one-element fast path must not fire for it, or
    /// `["a","x","a","b"]` answers `[0,2]` instead of `[2]` — the latent bug these scans were born fixing.
    #[test]
    fn position_scans_agree_with_the_indices_array() {
        let resources = ledger();
        let haystack = numbers(&["0", "1", "2", "3", "1", "2"]);
        let needle = numbers(&["1", "2"]);
        assert_eq!(
            to_json(&first_subsequence_position(&haystack, &needle, &resources).expect("scan")).expect("render"),
            "1"
        );
        assert_eq!(
            to_json(&last_subsequence_position(&haystack, &needle, &resources).expect("scan")).expect("render"),
            "4"
        );
        // One-element string needles keep the byte-compare fast path, both ends.
        let strings = |spellings: &[&str]| {
            let mut array = Array::try_new().expect("fixture array");
            for spelling in spellings {
                let text = String::from(*spelling);
                let shared = jqf_data::Shared::try_from_str(&text).expect("text");
                array.try_push(Value::String(shared)).expect("fixture element");
            }
            array
        };
        let tags = strings(&["a", "x", "a", "b", "a"]);
        let wanted = strings(&["b"]);
        assert_eq!(
            to_json(&first_subsequence_position(&tags, &wanted, &resources).expect("scan")).expect("render"),
            "3"
        );
        assert_eq!(
            to_json(&last_subsequence_position(&tags, &wanted, &resources).expect("scan")).expect("render"),
            "3"
        );
        let missing = strings(&["z"]);
        assert_eq!(
            to_json(&first_subsequence_position(&tags, &missing, &resources).expect("scan")).expect("render"),
            "null"
        );
        assert_eq!(
            to_json(&last_subsequence_position(&tags, &missing, &resources).expect("scan")).expect("render"),
            "null"
        );
        // The multi-element string needle takes the general walk.
        let pair = strings(&["a", "b"]);
        assert_eq!(
            to_json(&first_subsequence_position(&tags, &pair, &resources).expect("scan")).expect("render"),
            "2"
        );
        assert_eq!(
            to_json(&last_subsequence_position(&tags, &pair, &resources).expect("scan")).expect("render"),
            "2"
        );
        // Empty needles answer null exactly like `indices([]) | .[0]`.
        let none = Array::try_new().expect("empty needle");
        assert_eq!(
            to_json(&first_subsequence_position(&tags, &none, &resources).expect("scan")).expect("render"),
            "null"
        );
        // String scans: first, last, empty needle, and codepoint positions.
        assert_eq!(to_json(&first_codepoint_position("αβγβ", "β")).expect("render"), "1");
        assert_eq!(to_json(&last_codepoint_position("αβγβ", "β")).expect("render"), "3");
        assert_eq!(to_json(&first_codepoint_position("αβγβ", "ζ")).expect("render"), "null");
        assert_eq!(to_json(&last_codepoint_position("αβγβ", "")).expect("render"), "null");
        // Overlap is preserved at both ends: `"aaaa"` indexes `"aa"` at 0..2.
        assert_eq!(to_json(&first_codepoint_position("aaaa", "aa")).expect("render"), "0");
        assert_eq!(to_json(&last_codepoint_position("aaaa", "aa")).expect("render"), "2");
    }

    /// String positions are CODEPOINTS, overlap, and stop at the empty needle.
    ///
    /// The multibyte row is the one a byte-indexed search gets wrong — answering `[0,2]` — and the overlap is what
    /// forbids advancing by the match: `"ααα" | indices("αα")` is `[0,1]`. The empty needle answers no positions
    /// at all, which is both the answer and the only one that terminates.
    #[test]
    fn string_positions_are_codepoints_and_overlap() {
        let resources = ledger();
        let positions = codepoint_indices("ααα", "αα", &resources).expect("positions");
        assert_eq!(dump(&positions), "[0,1]");
        let empty = codepoint_indices("abc", "", &resources).expect("positions");
        assert_eq!(dump(&empty), "[]");
    }

    /// Array positions overlap too, and compare by VALUE across spellings.
    ///
    /// The needle is matched with the total order's equality, so `[1]` is found in `[1.0]`: a search that compared
    /// representations would miss it, and the value law does not. The empty needle answers no positions, as the string
    /// law does.
    #[test]
    fn array_positions_overlap_and_compare_by_value() {
        let resources = ledger();
        let haystack = numbers(&["1", "2", "1", "2"]);
        let found = subsequence_indices(&haystack, &numbers(&["1", "2"]), &resources).expect("array positions");
        assert_eq!(dump(&found), "[0,2]");

        let spelled = numbers(&["1.0"]);
        let across = subsequence_indices(&numbers(&["1"]), &spelled, &resources).expect("array positions");
        assert_eq!(dump(&across), "[0]");

        let none = subsequence_indices(&haystack, &numbers(&[]), &resources).expect("no positions");
        assert_eq!(dump(&none), "[]");
    }

    /// A split keeps the empty-operand answers, not `str::split`'s.
    ///
    /// The empty INPUT is the row `str::split` would answer `[""]` for, and the empty SEPARATOR cuts into codepoints
    /// rather than answering nothing. A separator at the end still contributes its empty piece.
    #[test]
    fn a_split_keeps_the_empty_operand_answers() {
        let resources = ledger();
        let pieces = split("a,b,", ",", &resources).expect("pieces");
        assert_eq!(dump(&pieces), "[\"a\",\"b\",\"\"]");
        let empty_input = split("", ",", &resources).expect("pieces");
        assert_eq!(dump(&empty_input), "[]");
        let codepoints = split("aβ", "", &resources).expect("pieces");
        assert_eq!(dump(&codepoints), "[\"a\",\"β\"]");
    }
}
