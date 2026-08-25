//! The jqf FUZZY family.
//!
//! Three pure string laws over the Levenshtein edit distance, computed on CHARACTERS — never bytes — after both
//! sides are normalized to Unicode NFC and case-folded:
//!
//! - `edit_distance(other)` — the primitive: the Levenshtein distance.
//! - `similarity(other)` — `1 - distance / max(len_a, len_b)`, in `[0.0, 1.0]`
//!   (1.0 for a pair of empty strings).
//! - `fuzzy_match(other; threshold)` — whether `similarity >= threshold`.
//!
//! The two correctness traps are MANDATORY: byte-level distance over UTF-8 is nonsense for non-ASCII (`"café"` vs
//! `"cafe"` would score 2 instead of 1), and without NFC the same string in two encodings would score as different. The
//! pipeline is `case-fold → NFC` (caseless's full case folding, then `unicode-normalization`'s canonical
//! recomposition), so `"café"` and `"cafe\u{301}"` are distance 0 and `"STRASSE"` matches `"straße"` at distance 0.
//!
//! The perf lever is real: `fuzzy_match` is NOT `similarity >= threshold`.
//! `similarity >= t` ⟺ `distance <= floor((1-t) * max)` for an integer distance, so the threshold is pushed into a
//! BANDED Levenshtein of width `2k+1` that abandons the moment a row's minimum exceeds `k` — O(k·n) instead of
//! O(n·m), which is what makes `fuzzy_match` usable as a filter over a large document. `edit_distance`/`similarity`
//! use the full DP.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use caseless::Caseless;
use jqf_data::{Float, Integer, Number, Value};
use jqf_resource::ResourceContext;
use unicode_normalization::UnicodeNormalization;

use super::id;
use crate::error::EngineRunError;
use crate::error::message;
use crate::registry::record::{
    BuiltinExample, BuiltinExecution, BuiltinFamilyId, BuiltinFamilyRecord, BuiltinOverloadId, BuiltinOverloadRecord,
    DemandTransfer, Effects, ParameterKind, SemanticRevision,
};
use crate::semantics::order;
use crate::semantics::path::raise;

/// The fuzzy-law discriminants, one per evaluator shape.
#[derive(Clone, Copy, Debug)]
pub enum FuzzyLaw {
    /// `edit_distance/1` — the Levenshtein distance.
    EditDistance,
    /// `similarity/1` — `1 - distance / max(len_a, len_b)`.
    Similarity,
    /// `fuzzy_match/2` — whether `similarity >= threshold`, via the bounded banded search.
    FuzzyMatch,
}

/// The normalized comparison form: full case folding, then NFC recomposition.
fn prepare(text: &str) -> String {
    text.chars().default_case_fold().collect::<String>().nfc().collect()
}

/// The full Levenshtein DP over two normalized character slices, O(n·m) with two rolling rows.
#[allow(
    clippy::many_single_char_names,
    reason = "the DP's dimensions and indices read as `n`/`m`/`i`/`j` in every textbook \
              formulation; renaming them would obscure the algorithm the code documents"
)]
fn edit_distance(a: &[char], b: &[char]) -> usize {
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let substitution = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            let deletion = prev[j] + 1;
            let insertion = cur[j - 1] + 1;
            cur[j] = substitution.min(deletion).min(insertion);
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Whether the Levenshtein distance is at most `k`, by the BANDED DP.
///
/// Cells with `|i - j| > k` can never lie on a path of cost ≤ k (the length bound `dist(i, j) ≥ |i - j|`), so only
/// the `2k+1`-wide band around the diagonal is computed, cells outside it are sentinel `usize::MAX`, and the drive
/// abandons the moment a row's minimum exceeds `k` — every lattice path to the answer crosses that row, and a
/// crossing cell already costs more than `k`, so no later row can bring the answer back down. O(k·n).
#[allow(
    clippy::many_single_char_names,
    reason = "same `n`/`m`/`i`/`j`/`k` textbook spelling as `edit_distance`"
)]
fn distance_at_most(a: &[char], b: &[char], bound: usize) -> bool {
    let n = a.len();
    let m = b.len();
    // The distance never exceeds the longer side's length, so a bound past it is the full DP — and the length
    // difference is a lower bound, so a pair whose lengths already differ by more than `k` cannot match.
    let k = bound.min(n.max(m));
    if k == 0 {
        return a == b;
    }
    if n.abs_diff(m) > k {
        return false;
    }
    let sentinel = usize::MAX;
    let mut prev: Vec<usize> = (0..=m).map(|j| if j <= k { j } else { sentinel }).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        // Column 0 is in the band only while its own diagonal is.
        cur[0] = if i <= k { i } else { sentinel };
        let lo = i.saturating_sub(k).max(1);
        let hi = (i + k).min(m);
        // Cells outside the row's band keep the sentinel — the previous row's values there are > k by the length
        // bound, so treating them as `usize::MAX` is exactly right.
        cur[1..lo].fill(sentinel);
        // The row minimum spans the WHOLE band, including column 0 when it is in-band (an empty `j` band — the `m ==
        // 0` rows — must not turn the minimum into the sentinel).
        let mut row_min = cur[0];
        for j in lo..=hi {
            let mut best = sentinel;
            if prev[j - 1] != sentinel {
                best = best.min(prev[j - 1].saturating_add(usize::from(a[i - 1] != b[j - 1])));
            }
            if prev[j] != sentinel {
                best = best.min(prev[j].saturating_add(1));
            }
            if cur[j - 1] != sentinel {
                best = best.min(cur[j - 1].saturating_add(1));
            }
            cur[j] = best;
            row_min = row_min.min(best);
        }
        cur[(hi + 1)..].fill(sentinel);
        if row_min > k {
            return false;
        }
        core::mem::swap(&mut prev, &mut cur);
    }
    prev[m] <= k
}

/// The `similarity >= threshold` test as a BOUNDED search.
///
/// `similarity = 1 - d/max` where `d` is the integer distance, so `similarity >= t ⟺ d <= (1 - t) * max`, and since
/// `d` is an integer, `d <= floor((1 - t) * max)`. The threshold is therefore a distance bound pushed straight into
/// [`distance_at_most`] — never a float comparison over the full DP. A `NaN` threshold matches nothing (every float
/// comparison against `NaN` is false), a threshold above 1 matches nothing, and a threshold at or below 0 matches
/// everything.
fn fuzzy_match(a: &[char], b: &[char], threshold: f64) -> bool {
    if threshold.is_nan() || threshold > 1.0 {
        return false;
    }
    if threshold <= 0.0 {
        return true;
    }
    let max = a.len().max(b.len());
    if max == 0 {
        return true;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "the bound is capped at the longer length before the cast, so it cannot exceed \
                  usize on any 64-bit host, the threshold is finite and in (0, 1], and a decoded \
                  document's strings cannot approach 2^52 characters"
    )]
    let bound = ((1.0 - threshold) * max as f64).floor() as usize;
    if distance_at_most(a, b, bound) {
        return true;
    }
    // Float rounding can floor the bound one short of a distance whose own similarity still meets the threshold (`(1.0
    // - 0.8) * 10` floors to 1, while similarity computes bit-equal 0.8 for d=2, max=10). When the band fails, the
    // distance is either exactly bound+1 or larger — the first call already excluded everything at or under it — so
    // admit bound+1 through THE SAME [`similarity`] arithmetic the similarity builtin answers with. Larger distances
    // have strictly lower similarity, so they fail with it.
    let Some(candidate) = bound.checked_add(1) else {
        return false;
    };
    if !distance_at_most(a, b, candidate) {
        return false;
    }
    similarity(a.len(), b.len(), candidate) >= threshold
}

/// The similarity law, `1 - distance / max(len_a, len_b)`, with `1.0` for a pair of empty strings.
#[allow(
    clippy::cast_precision_loss,
    reason = "a decoded document's strings cannot approach 2^52 characters, so usize→f64 \
              is lossless in practice"
)]
fn similarity(a_len: usize, b_len: usize, distance: usize) -> f64 {
    let max = a_len.max(b_len);
    if max == 0 {
        1.0
    } else {
        1.0 - distance as f64 / max as f64
    }
}

fn integer_value(value: i64) -> Value {
    Value::Number(Number::integer(Integer::from_i64(value)))
}

/// One fuzzy-law evaluation for exactly one tuple: the piped `subject` (its whole value) and the argument tuple `args`.
/// The caller owns argument EVALUATION — it runs each parameter's filter over the call's input and calls this law
/// once per combination — so this function never reasons about cardinality.
///
/// # Errors
///
/// Returns a catchable refusal for a non-string input or a non-string `other` argument, or a non-number threshold
/// (`fuzzy_match/2`).
pub fn fuzzy_law(
    law: FuzzyLaw,
    subject: &Value,
    args: &[Value],
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    let Value::String(subject_text) = subject.untagged() else {
        return Err(raise(
            match law {
                FuzzyLaw::EditDistance => "edit_distance requires a string input",
                FuzzyLaw::Similarity => "similarity requires a string input",
                FuzzyLaw::FuzzyMatch => "fuzzy_match requires a string input",
            },
            resources,
        ));
    };
    let other = match args.first() {
        Some(Value::String(other)) => other.as_str(),
        _ => {
            return Err(raise(
                match law {
                    FuzzyLaw::EditDistance => "edit_distance requires a string argument",
                    FuzzyLaw::Similarity => "similarity requires a string argument",
                    FuzzyLaw::FuzzyMatch => "fuzzy_match requires a string argument",
                },
                resources,
            ));
        }
    };
    match law {
        FuzzyLaw::EditDistance => {
            let a = prepare(subject_text.as_str());
            let b = prepare(other);
            let a_chars = a.chars().collect::<Vec<_>>();
            let b_chars = b.chars().collect::<Vec<_>>();
            let distance = edit_distance(&a_chars, &b_chars);
            // A string longer than i64::MAX characters cannot exist in a decoded document, so the widening is lossless.
            #[allow(
                clippy::cast_possible_wrap,
                reason = "the length bound above makes the cast lossless"
            )]
            let distance = distance as i64;
            Ok(integer_value(distance))
        }
        FuzzyLaw::Similarity => {
            let a = prepare(subject_text.as_str());
            let b = prepare(other);
            let a_chars = a.chars().collect::<Vec<_>>();
            let b_chars = b.chars().collect::<Vec<_>>();
            let distance = edit_distance(&a_chars, &b_chars);
            let similarity = similarity(a_chars.len(), b_chars.len(), distance);
            Ok(Value::Number(Number::float(Float::new(similarity))))
        }
        FuzzyLaw::FuzzyMatch => {
            let threshold = if let Some(Value::Number(number)) = args.get(1) {
                order::to_f64(number)
            } else {
                let text = message::number_required(args.get(1).unwrap_or(&Value::Null))?;
                return Err(raise(&text, resources));
            };
            let a = prepare(subject_text.as_str());
            let b = prepare(other);
            let a_chars = a.chars().collect::<Vec<_>>();
            let b_chars = b.chars().collect::<Vec<_>>();
            Ok(Value::Bool(fuzzy_match(&a_chars, &b_chars, threshold)))
        }
    }
}

// ------------------------------------------------------------------------
// Registry records.

const ONE_FILTER: &[ParameterKind] = &[ParameterKind::Filter];
const TWO_FILTERS: &[ParameterKind] = &[ParameterKind::Filter, ParameterKind::Filter];

const fn family(id: u16, name: &'static str, summary: &'static str, detail: &'static str) -> BuiltinFamilyRecord {
    BuiltinFamilyRecord {
        id: BuiltinFamilyId::new(id),
        canonical_name: name,
        category: "jqf-enrich",
        summary,
        detail,
    }
}

const fn example(program: &'static str, input: &'static str, expected: &'static str) -> BuiltinExample {
    BuiltinExample {
        program,
        input,
        expected,
    }
}

const fn overload(
    id: u16,
    family_id: u16,
    name: &'static str,
    arity: u8,
    parameters: &'static [ParameterKind],
    examples: &'static [BuiltinExample],
) -> BuiltinOverloadRecord {
    BuiltinOverloadRecord {
        id: BuiltinOverloadId::new(id),
        family: BuiltinFamilyId::new(family_id),
        canonical_name: name,
        arity,
        parameters,
        execution: BuiltinExecution::Evaluator,
        demand_transfer: DemandTransfer::Subtree,
        semantic_revision: SemanticRevision::new(1),
        effects: Effects::Pure,
        examples,
    }
}

const EDIT_DISTANCE_FAMILY: BuiltinFamilyRecord = family(
    id::EDIT_DISTANCE_FAMILY_ID,
    "edit_distance",
    "The Levenshtein distance between two strings, per Unicode character.",
    "",
);
const SIMILARITY_FAMILY: BuiltinFamilyRecord = family(
    id::SIMILARITY_FAMILY_ID,
    "similarity",
    "1 - distance / max(len_a, len_b), in [0.0, 1.0].",
    "",
);
const FUZZY_MATCH_FAMILY: BuiltinFamilyRecord = family(
    id::FUZZY_MATCH_FAMILY_ID,
    "fuzzy_match",
    "Whether similarity(other) is at least a threshold, via a bounded search.",
    "",
);

pub const FAMILIES: &[BuiltinFamilyRecord] = &[EDIT_DISTANCE_FAMILY, SIMILARITY_FAMILY, FUZZY_MATCH_FAMILY];

const EDIT_DISTANCE_OVERLOAD: BuiltinOverloadRecord = overload(
    id::EDIT_DISTANCE,
    id::EDIT_DISTANCE_FAMILY_ID,
    "edit_distance",
    1,
    ONE_FILTER,
    &[
        example("edit_distance(\"kitten\")", "\"sitting\"", "3\n"),
        // Per CHARACTER, not per byte: "café" is 4 characters to "cafe"'s 4, so the difference is one character, not
        // two bytes.
        example("edit_distance(\"cafe\")", "\"café\"", "1\n"),
        // NFC-equivalent spellings are the same string: distance 0.
        example("edit_distance(\"cafe\\u0301\")", "\"café\"", "0\n"),
        // Case folding: STRASSE and straße are the same folded string.
        example("edit_distance(\"STRASSE\")", "\"straße\"", "0\n"),
    ],
);
const SIMILARITY_OVERLOAD: BuiltinOverloadRecord = overload(
    id::SIMILARITY,
    id::SIMILARITY_FAMILY_ID,
    "similarity",
    1,
    ONE_FILTER,
    &[
        example("similarity(\"kitten\")", "\"sitting\"", "0.5714285714285714\n"),
        // Identical strings (here NFC-equivalent) score 1.0.
        example("similarity(\"cafe\\u0301\")", "\"café\"", "1\n"),
    ],
);
const FUZZY_MATCH_OVERLOAD: BuiltinOverloadRecord = overload(
    id::FUZZY_MATCH,
    id::FUZZY_MATCH_FAMILY_ID,
    "fuzzy_match",
    2,
    TWO_FILTERS,
    &[
        example("fuzzy_match(\"sitting\"; 0.5)", "\"kitten\"", "true\n"),
        example("fuzzy_match(\"sitting\"; 0.9)", "\"kitten\"", "false\n"),
        // The threshold is pushed into a bounded search, never a float comparison over the full DP.
        example("fuzzy_match(\"kitten\"; 1)", "\"kitten\"", "true\n"),
        // "café" vs "cafe" scores 0.75, so 0.7 admits it and 0.9 rejects.
        example("fuzzy_match(\"cafe\"; 0.7)", "\"café\"", "true\n"),
    ],
);

pub const OVERLOADS: &[BuiltinOverloadRecord] = &[EDIT_DISTANCE_OVERLOAD, SIMILARITY_OVERLOAD, FUZZY_MATCH_OVERLOAD];

/// The fuzzy execution payloads, aligned one-to-one with [`OVERLOADS`].
///
/// Every entry carries its overload id so the const coverage walk in `registry::dispatch` can prove pairwise alignment.
/// The laws ride the extension family's argument-product drive; `registry::dispatch` wraps them into
/// `ExtensionLaw::Fuzzy` at table build time.
pub const PAYLOADS: &[(u16, FuzzyLaw)] = &[
    (id::EDIT_DISTANCE, FuzzyLaw::EditDistance),
    (id::SIMILARITY, FuzzyLaw::Similarity),
    (id::FUZZY_MATCH, FuzzyLaw::FuzzyMatch),
];

#[cfg(test)]
mod tests {
    use super::*;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &ContinueControl,
            WorkMeter::try_new_v1(1).expect("work"),
        )
        .expect("resources")
    }

    fn string(text: &str, _resources: &ResourceContext<'static>) -> Value {
        Value::try_string(text).expect("string")
    }

    fn dist(a: &str, b: &str) -> usize {
        let a = prepare(a).chars().collect::<Vec<_>>();
        let b = prepare(b).chars().collect::<Vec<_>>();
        edit_distance(&a, &b)
    }

    #[test]
    fn distance_is_per_character_not_per_byte() {
        assert_eq!(dist("café", "cafe"), 1);
        assert_eq!(dist("café", "cafe\u{301}"), 0);
        assert_eq!(dist("こんにちは", "こんばんは"), 2);
    }

    #[test]
    fn case_folding_is_full() {
        assert_eq!(dist("STRASSE", "straße"), 0);
        assert_eq!(dist("KITTEN", "kitten"), 0);
    }

    #[test]
    fn banded_matches_full_dp() {
        // The bounded search and the full DP must agree over a spread of bounds — the band's sentinel law is exactly
        // the equivalence `fuzzy_match` promises.
        let pairs = [
            ("kitten", "sitting"),
            ("saturday", "sunday"),
            ("café", "cafe"),
            ("こんにちは", "こんばんは"),
            ("", "abc"),
            ("abc", ""),
            ("", ""),
            ("aaaa", "aaab"),
            ("abcdef", "fedcba"),
        ];
        for (a, b) in pairs {
            let a_chars = prepare(a).chars().collect::<Vec<_>>();
            let b_chars = prepare(b).chars().collect::<Vec<_>>();
            let full = edit_distance(&a_chars, &b_chars);
            for bound in 0..=full + 2 {
                assert_eq!(
                    distance_at_most(&a_chars, &b_chars, bound),
                    full <= bound,
                    "{a:?} vs {b:?} bound {bound}"
                );
            }
        }
    }

    #[test]
    fn threshold_law_matches_similarity() {
        for (a, b, threshold) in [
            ("kitten", "sitting", 0.5),
            ("kitten", "sitting", 0.9),
            ("kitten", "kitten", 1.0),
            ("kitten", "kitten", 0.0),
            ("kitten", "kitten", -1.0),
            ("kitten", "kitten", 1.5),
            ("café", "cafe", 0.9),
            ("café", "cafe", 0.5),
            ("", "", 0.5),
        ] {
            let a = prepare(a);
            let b = prepare(b);
            let a_chars = a.chars().collect::<Vec<_>>();
            let b_chars = b.chars().collect::<Vec<_>>();
            let d = edit_distance(&a_chars, &b_chars);
            let similarity = similarity(a_chars.len(), b_chars.len(), d);
            assert_eq!(
                fuzzy_match(&a_chars, &b_chars, threshold),
                similarity >= threshold,
                "{a:?} vs {b:?} threshold {threshold}"
            );
        }
    }

    /// The float bound can round one short of the similarity law's own answer: `(1.0 - 0.8) * 10` floors to 1, but
    /// similarity computes a bit-equal 0.8 for d=2, max=10 — so the band's refusal must defer to the same arithmetic
    /// similarity runs.
    #[test]
    fn bound_rounding_defers_to_similarity() {
        let a = "abcdefghij";
        let b = "abcdefxyij"; // two substitutions, both length 10
        let a_chars = prepare(a).chars().collect::<Vec<_>>();
        let b_chars = prepare(b).chars().collect::<Vec<_>>();
        let d = edit_distance(&a_chars, &b_chars);
        assert_eq!(d, 2);
        let threshold = 0.8;
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(similarity(a_chars.len(), b_chars.len(), d), threshold);
        }
        assert!((1.0 - threshold) * 10.0 < 2.0, "the bound must floor short");
        assert!(fuzzy_match(&a_chars, &b_chars, threshold));
        // One substitution more drops below the threshold for good.
        let c = prepare("abcdefxyzj").chars().collect::<Vec<_>>(); // three substitutions
        assert_eq!(edit_distance(&a_chars, &c), 3);
        assert!(!fuzzy_match(&a_chars, &c, threshold));
    }

    #[test]
    fn law_answers_via_the_registry_path() {
        let resources = resources();
        let out = fuzzy_law(
            FuzzyLaw::EditDistance,
            &string("sitting", &resources),
            &[string("kitten", &resources)],
            &resources,
        )
        .expect("edit_distance");
        let Value::Number(number) = out.untagged() else {
            panic!("expected a number");
        };
        assert_eq!(number.to_i64(), Some(3));
        let out = fuzzy_law(
            FuzzyLaw::Similarity,
            &string("sitting", &resources),
            &[string("kitten", &resources)],
            &resources,
        )
        .expect("similarity");
        let Value::Number(number) = out.untagged() else {
            panic!("expected a number");
        };
        assert!((order::to_f64(number) - 4.0 / 7.0).abs() < 1e-12);
        let out = fuzzy_law(
            FuzzyLaw::FuzzyMatch,
            &string("sitting", &resources),
            &[
                string("kitten", &resources),
                Value::Number(Number::float(Float::new(0.5))),
            ],
            &resources,
        )
        .expect("fuzzy_match");
        assert!(matches!(out.untagged(), Value::Bool(true)));
    }
}
