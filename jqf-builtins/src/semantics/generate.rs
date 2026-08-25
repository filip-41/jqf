//! The generator vocabulary: the closed table of value SOURCES, and the pure state each one steps.
//!
//! One job: own the six generator rows `range`, `while`, `until`, `repeat`, `recurse` and `combinations` reduce to,
//! together with the pure stepping laws two of them need ([`RangeCursor`] and [`Odometer`]). The executor owns the
//! FRAME that resumes a generator; this module owns what a resumption computes, so the drive shape and the arithmetic
//! cannot drift apart.
//!
//! **[`GENERATORS`] is a closed table, in the [`super::keyed::KEY_MODES`] discipline**: explicit rows, no default arm,
//! and a `const` assertion that a row's index equals its lookup. Each row carries the generator's canonical NAME (which
//! the registry records read from here rather than respelling), its TERMINATION class (which the executor reads to
//! decide which resumptions owe the cooperative control check) and the one-line PROOF of that class — which is the
//! column that makes the table worth having, because four of the seven laws do not terminate at all and a reader has to
//! be able to tell which four.
//! One row carries a [`SecondLaw`], with its own termination class and its own proof: `range` is two laws under one
//! name, and the section below is why.
//!
//! **`Range` accumulates in `f64`, deliberately, against jqf's exact-decimal number model.** This is the most
//! surprising line in the file and the one most likely to be "corrected":
//!
//! ```text
//! [range(0;1;0.1)] | length   ->  reference: 11,  exact-decimal accumulation: 10
//! [range(1;2;1e-1)] | .[2]    ->  reference: 1.2000000000000002, exact: 1.2
//! ```
//!
//! Ten exact steps of `0.1` land exactly on `1` and stop; ten binary64 steps overshoot to `0.9999999999999999` and take
//! an eleventh. So the choice changes a COUNT, not merely some bytes. [`emit`] converts each `f64` to its shortest
//! round-trip decimal at the boundary, after which the value re-enters exact-decimal arithmetic downstream — the
//! divergence is confined to the accumulator.
//!
//! **And `range` has a SECOND law.** Only `range/1` and `range/2` are type-checked; `range/3` is a definition-level law
//! — `if $by == 0 then empty elif $by > 0 then $from|while(. < $upto; . + $by) else $from|while(. > $upto; . + $by)
//! end` — so at that arity a bound never meets a type check at all. It meets the TOTAL ORDER and binary `+`.
//! [`ValueCursor`] is that law, [`RangeLaw`] is the choice between the two, and the choice is by SHAPE: three numbers
//! keep the `f64` accumulator above, anything else steps values.
//!
//! Negative space: nothing here drives a subgraph, opens a frame, or decides cardinality.
//! `while`/`until`/`repeat`/`recurse` carry no state struct here at all, because their state IS a `Value` plus the
//! executor's frame stack.

use alloc::vec::Vec;
use core::cmp::Ordering;

use jqf_data::{Array, Integer, Number, Value};
use jqf_resource::ResourceContext;

use crate::error::EngineRunError;
use crate::error::{apply_binary, message};
use crate::semantics::binary::BinaryKind;
use crate::semantics::depth::{Guarded, TooDeep, message as depth_message};
use crate::semantics::order::{to_f64, total_cmp};
use crate::semantics::path::raise;

/// The one message class for a `range` bound that is not a number.
///
/// # It belongs to arities 1 and 2 ALONE
///
/// `range/3` is a definition-level law: [`open`] chooses the law by arity, so at that arity a non-number bound steps
/// under [`ValueCursor`]'s total order and never reaches this message.
///
/// The check fires once BOTH bounds are in hand, not as each one lands, which is observable when the bound to the right
/// raises:
///
/// ```text
/// [range([]; error("x"))]   -> x     (both bounds evaluate, then the check)
/// [range([]; empty)]        -> []    (no pair, so the check never runs)
/// [range([]; 3)]            -> Range bounds must be numeric
/// ```
pub const NON_NUMERIC_BOUND: &str = "Range bounds must be numeric";

/// The six generator rows. Every generator overload reduces to exactly one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Generator {
    /// `range/1`, `range/2`, `range/3`.
    Range,
    /// `while/2`.
    While,
    /// `until/2`.
    Until,
    /// `repeat/1`.
    Repeat,
    /// `recurse/0`, `recurse/1`, `recurse/2`.
    Recurse,
    /// `combinations/0`, `combinations/1`.
    Combinations,
}

/// Whether a generator runs out on its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Termination {
    /// The state itself runs out: the cursor passes its bound, the odometer visits its last tuple, the walk reaches
    /// leaves.
    Structural,
    /// NOTHING in the generator stops it. Only a consumer (`limit`, `first`) or the ledger does, so every resumption of
    /// one of these owes the cooperative control check.
    Cooperative,
}

/// A generator row's SECOND stepping law, when the builtin has two.
///
/// Exactly one row carries one, and it is not a stylistic split: `range/1` and `range/2` are type-checked and `range/3`
/// is a definition-level law, so one builtin obeys two different laws with two different termination classes. A row
/// with a single `termination` column could not say that, and a reader who only saw the `Structural` column would
/// conclude — wrongly — that no `range` can run forever.
#[derive(Clone, Copy, Debug)]
pub struct SecondLaw {
    /// The SHAPE that selects this law over the row's primary one.
    pub selector: &'static str,
    /// Whether THIS law stops on its own. It is read the same way the primary column is: a `Cooperative` law owes the
    /// control check on every resumption.
    pub termination: Termination,
    /// Why `termination` is what it is, in one line — the same obligation the primary law carries, asserted by the
    /// same `const` block.
    pub proof: &'static str,
}

/// One row of the closed generator table.
#[derive(Clone, Copy, Debug)]
pub struct GeneratorRow {
    /// The row's own discriminant, so a lookup can assert it landed on itself.
    pub generator: Generator,
    /// The canonical builtin name the row serves. The registry records read it from here rather than respelling it, so
    /// the catalog and the semantics cannot name the same generator differently.
    pub name: &'static str,
    /// Whether the generator stops on its own — read by the executor to decide which resumptions owe the cooperative
    /// control check.
    pub termination: Termination,
    /// Why `termination` is what it is, in one line. Every row must carry one; the const assertion below is what makes
    /// that a build error rather than a review note.
    pub proof: &'static str,
    /// The row's SECOND law, for the one builtin that has two.
    pub second_law: Option<SecondLaw>,
}

/// The closed generator table.
///
/// The `termination` column is the reason this table exists rather than a bare enum: `while`, `until` and `repeat` are
/// NON-TERMINATING BY DESIGN, bounded only by their consumer (`limit`, `first`) and by the ledger's cooperative control
/// check, and a reader has to be able to see that without reading the executor. `range`'s `second_law` is the fourth
/// non-terminating entry and the least expected one, which is exactly why it is a declared column rather than a remark.
pub const GENERATORS: [GeneratorRow; 6] = [
    GeneratorRow {
        generator: Generator::Range,
        name: "range",
        termination: Termination::Structural,
        proof: "state: three f64 (current, upto, by). `by == 0` emits nothing; otherwise \
                `current` moves monotonically toward `upto`, and an infinite `by` gets there \
                in one step",
        second_law: Some(SecondLaw {
            selector: "`range/3` whose three bounds are not all numbers — that arity is \
                       definition-level, so its bounds meet the TOTAL ORDER and binary `+` \
                       rather than a type check",
            termination: Termination::Cooperative,
            proof: "state: three VALUES (current, upto, by). It stops when the order comparison \
                    leaves the range (`[range({};3;1)]` is `[]`) or when `+` raises \
                    (`[range(0;3;{})]` cannot add), but for the shapes the def spins on — \
                    `[range(0;{};1)]`, `[range(\"a\";\"z\";\"b\")]`, `[range(3;0;null)]` — \
                    NOTHING in the cursor stops it, so every resumption owes the cooperative \
                    control check exactly as `while` does. Those shapes are NOT oracle-pinned, \
                    for the reason the `while` row gives",
        }),
    },
    GeneratorRow {
        generator: Generator::While,
        name: "while",
        termination: Termination::Cooperative,
        proof: "state: the current value. A condition that never goes falsy never stops, which \
                is the specification and not an oversight",
        second_law: None,
    },
    GeneratorRow {
        generator: Generator::Until,
        name: "until",
        termination: Termination::Cooperative,
        proof: "state: the current value. Same as `while`, except that it emits exactly ONE \
                value — the first that satisfies the condition",
        second_law: None,
    },
    GeneratorRow {
        generator: Generator::Repeat,
        name: "repeat",
        termination: Termination::Cooperative,
        proof: "state: the ORIGINAL input, which never advances. `repeat(f)` re-applies `f` to \
                the input it started from, forever",
        second_law: None,
    },
    GeneratorRow {
        generator: Generator::Recurse,
        name: "recurse",
        termination: Termination::Structural,
        proof: "state: an explicit frame stack, never the Rust stack. Each level takes the \
                ledger's nesting guard, so an unbounded descent raises rather than running \
                away",
        second_law: None,
    },
    GeneratorRow {
        generator: Generator::Combinations,
        name: "combinations",
        termination: Termination::Structural,
        proof: "state: an odometer over the dimension vector, which visits each tuple once",
        second_law: None,
    },
];

/// The row for one generator.
pub const fn row(generator: Generator) -> &'static GeneratorRow {
    &GENERATORS[index(generator)]
}

/// One generator's row index, which is also its declaration order — the fieldless enum's implicit discriminant.
const fn index(generator: Generator) -> usize {
    generator as usize
}

/// Compile-time closure: every row sits at its own lookup index, so a row can never be read for a generator other than
/// its own.
const _: () = {
    assert!(matches!(GENERATORS[0].generator, Generator::Range));
    assert!(matches!(GENERATORS[1].generator, Generator::While));
    assert!(matches!(GENERATORS[2].generator, Generator::Until));
    assert!(matches!(GENERATORS[3].generator, Generator::Repeat));
    assert!(matches!(GENERATORS[4].generator, Generator::Recurse));
    assert!(matches!(GENERATORS[5].generator, Generator::Combinations));
    let mut i = 0;
    while i < GENERATORS.len() {
        assert!(
            !GENERATORS[i].proof.is_empty(),
            "a generator row carries no termination proof"
        );
        assert!(
            !GENERATORS[i].name.is_empty(),
            "a generator row carries no canonical name"
        );
        // A second law owes exactly what the first one owes: what selects it, and why it stops (or does not).
        if let Some(second) = GENERATORS[i].second_law {
            assert!(
                !second.selector.is_empty(),
                "a second generator law carries no selector"
            );
            assert!(
                !second.proof.is_empty(),
                "a second generator law carries no termination proof"
            );
        }
        i += 1;
    }
    // `range` is the ONLY builtin with two laws; a new one is a table-shape decision, so it fails the build rather than
    // sliding in.
    assert!(GENERATORS[0].second_law.is_some());
    assert!(GENERATORS[1].second_law.is_none());
    assert!(GENERATORS[2].second_law.is_none());
    assert!(GENERATORS[3].second_law.is_none());
    assert!(GENERATORS[4].second_law.is_none());
    assert!(GENERATORS[5].second_law.is_none());
};

/// `range`'s three-`f64` accumulator.
///
/// The bound is ACCUMULATED (`current += by`), never recomputed as `from + i*by`: a recomputing implementation answers
/// `1.2` where the reference answers `1.2000000000000002`, and the difference is a byte-parity fact this cursor exists
/// to preserve.
#[derive(Clone, Copy, Debug)]
pub struct RangeCursor {
    current: f64,
    upto: f64,
    by: f64,
    /// `true` when the cursor was opened by ARITY 3 (`range/3`), whose law is the positive-stated `def` comparison
    /// rather than the opcode's negation.
    /// Arities 1 and 2 are the checked opcode and stay negated; arity 3 is a definition-level `while` law, so a NaN
    /// `upto` STOPS the ascending walk at once and a NaN `current` stops the descending one — the two laws part
    /// exactly there.
    def_law: bool,
}

impl RangeCursor {
    /// Opens a cursor over `[from, upto)` stepping by `by` under the opcode law (arities 1 and 2).
    pub const fn new(from: f64, upto: f64, by: f64) -> Self {
        Self {
            current: from,
            upto,
            by,
            def_law: false,
        }
    }

    /// Opens a cursor over `[from, upto)` stepping by `by` under the `def range/3` comparison law (arity 3).
    pub const fn def_law(from: f64, upto: f64, by: f64) -> Self {
        Self {
            current: from,
            upto,
            by,
            def_law: true,
        }
    }

    /// The next value, or `None` once the cursor is spent.
    #[expect(
        clippy::neg_cmp_op_on_partial_ord,
        reason = "the negated comparison IS the law: the range loop is `while (!(i >= e))` / \
                  `while (!(i <= e))` under IEEE, where a NaN makes every comparison false, so \
                  the negation is the ONLY spelling that never stops on a NaN bound; clippy's \
                  `partial_cmp` rewrite would answer `Equal != Greater` (keep walking) where `!(5 >= 5)` \
                  answers stop"
    )]
    pub fn next(&mut self) -> Option<f64> {
        // The C loop form, adopted byte-for-byte: the ascending walk continues while NOT(current >= upto) and the
        // descending walk while NOT(current <= upto), under IEEE comparisons. For finite bounds the two spellings ARE
        // the `<` / `>` a positive statement would use, but a NaN makes every IEEE comparison false, so a NaN `upto`
        // never stops the walk and a NaN `current` never exits: `range(0; nan)` is the unbounded ascending stream,
        // `range(0; nan; -1)` the unbounded descending one, and `range(nan; 5)` the unbounded stream of NaN (printed
        // `null`) — all pinned by the compat corpus. This replaced an earlier positive-stated law (`current < upto`)
        // that folded a NaN bound into the empty cursor; that choice predated the `nan` literal and quietly diverged
        // once NaN could reach the cursor at all.
        // A zero (or NaN) step still emits NOTHING rather than hanging — `[range(0;3;0)] | length` is 0, not an
        // infinite stream of zeroes.
        //
        // Arity 3 is the OTHER law: `range/3` is a `while` def whose comparison is POSITIVE-STATED under the total
        // order, so a NaN `upto` makes the ascending test false and STOPS the walk at once, while the descending test
        // `$a > $b` is true for a NaN `upto`
        // (NaN compares less than everything) and only a NaN `current` stops it. That is why `[limit(5; range(0; nan;
        // 1))] | length` is 0 while `first(range(0; nan; -1))` is 0 — the two laws part exactly at the NaN cases, and
        // finite bounds are byte-identical under both.
        let advancing = if self.def_law {
            self.def_advancing()
        } else if self.by > 0.0 {
            !(self.current >= self.upto)
        } else if self.by < 0.0 {
            !(self.current <= self.upto)
        } else {
            false
        };
        if !advancing {
            return None;
        }
        let out = self.current;
        self.current += self.by;
        Some(out)
    }

    /// The arity-3 (`def range/3`) comparison: `while($a < $b; …)` ascending and `while($a > $b; …)` descending,
    /// spelled with the NaN arms explicit. The def is `if $by == 0 then empty elif $by > 0 then while($a < $b; …)
    /// else while($a > $b; …) end` — the else arm is the DESCENDING walk, so a NaN `$by` (equal to neither side of
    /// the sign test) walks down too — and the total order ranks NaN BELOW every number, so `$a > $b` with a NaN `$b`
    /// stays true (the walk streams PAST a NaN bound) while a NaN `$a` fails it (a NaN current stops).
    /// Ascending is the mirror: a NaN `$b` fails the test and stops the walk at once.
    fn def_advancing(&self) -> bool {
        if self.by > 0.0 {
            if self.current.is_nan() {
                true
            } else if self.upto.is_nan() {
                false
            } else {
                self.current < self.upto
            }
        } else if self.by < 0.0 || self.by.is_nan() {
            if self.current.is_nan() {
                false
            } else if self.upto.is_nan() {
                true
            } else {
                self.current > self.upto
            }
        } else {
            false
        }
    }
}

/// Which way the `while` walks, chosen by `$by` against ZERO under the TOTAL ORDER rather than by sign.
///
/// The distinction is the whole surprise of the second law: `null`, `false` and `true` rank BELOW every number, so they
/// are all "negative" steps and walk DOWNWARD, while a string, array or object ranks above and walks UPWARD. That is
/// why `[range(0;3;null)]` is empty (down from 0 never passes 3) and `[range(0;3;{})]` emits `0` and then fails to add.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    /// `$by > 0`: emit while `current < upto`.
    Up,
    /// `$by < 0`: emit while `current > upto`.
    Down,
}

/// `range/3`'s value cursor: the `while` def, stepped one resumption at a time.
///
/// It is the [`GENERATORS`] Range row's [`SecondLaw`]. Three facts are
///
/// * The direction is the TOTAL ORDER against zero, not a numeric sign
///   ([`Step`]).
/// * The step is DEFERRED past the emission, because `while` emits before it
///   updates: `range(0;3;{})` prints `0` and only then raises `number (0) and object ({}) cannot be added`. Adding
///   eagerly would invert those two.
/// * NOTHING here bounds the walk. `[range(0;{};1)]`, `[range("a";"z";"b")]`
///   and `[range(3;0;null)]` run forever, so the cursor is [`Termination::Cooperative`] and its resumptions owe the
///   control check.
#[derive(Debug)]
pub struct ValueCursor {
    /// The value the next emission publishes, before its own step.
    current: Value,
    /// The exclusive bound the order comparison tests against.
    upto: Value,
    /// The addend, whose rank chose [`Self::step`] and which every step adds.
    by: Value,
    /// Which way the comparison must fall for the cursor to keep going, or `None` for the def's `else empty end` — a
    /// step ranking EQUAL to zero, which emits nothing at all rather than looping on a value that never moves.
    step: Option<Step>,
    /// Whether an emission has already happened, which is what defers the first addition past the first output.
    stepped: bool,
}

impl ValueCursor {
    /// The next value, or `None` once the comparison leaves the range.
    ///
    /// # Errors
    ///
    /// Propagates whatever binary `+` raises for the pair in hand (the `X and Y cannot be added` family), the
    /// comparison's own depth cap, or an allocation failure.
    pub fn next(&mut self, resources: &ResourceContext<'_>) -> Result<Option<Value>, EngineRunError> {
        let Some(step) = self.step else {
            return Ok(None);
        };
        if self.stepped {
            self.current = apply_binary(BinaryKind::Add, &self.current, &self.by, resources)?;
        }
        self.stepped = true;
        let ordering = compare(&self.current, &self.upto, resources)?;
        let advancing = match step {
            Step::Up => ordering.is_lt(),
            Step::Down => ordering.is_gt(),
        };
        if !advancing {
            return Ok(None);
        }
        Ok(Some(self.current.clone()))
    }
}

/// Two owned values under jqf's one total order, with the comparison's own depth cap already spelled the way `<` spells
/// it.
fn compare(left: &Value, right: &Value, resources: &ResourceContext<'_>) -> Result<Ordering, EngineRunError> {
    total_cmp(left, right).map_err(|TooDeep| raise(depth_message(Guarded::Comparison), resources))
}

/// The three `range` bounds AS AUTHORED, before either law reads them.
///
/// They are held as VALUES rather than converted where each one lands, because which law applies is not known until the
/// LAST one is in hand: three numbers take the `f64` accumulator, anything else takes [`ValueCursor`]. Holding them is
/// also what gives arities 1 and 2 their check TIMING — the opcode checks once both bounds exist, so `[range([];
/// error("x"))]` raises `x`.
#[derive(Debug)]
pub struct RangeBounds {
    /// `(from, upto, by)`. `None` is "this arity does not author that bound", which [`open`] fills with the default
    /// rather than a sentinel.
    slots: [Option<Value>; 3],
}

impl RangeBounds {
    /// An empty triple, before any bound has been consumed.
    pub const fn new() -> Self {
        Self {
            slots: [None, None, None],
        }
    }

    /// Fixes one bound.
    pub fn set(&mut self, slot: usize, value: Value) -> Result<(), EngineRunError> {
        let target = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| EngineRunError::internal_contract("a range bound past its triple"))?;
        *target = Some(value);
        Ok(())
    }

    /// A copy, for the Cartesian frame that keeps fixing bounds to its right.
    ///
    /// # Errors
    ///
    /// Returns an allocation failure when a held bound cannot be cloned.
    pub fn try_clone(&self) -> Self {
        let mut slots = [None, None, None];
        for (target, source) in slots.iter_mut().zip(self.slots.iter()) {
            target.clone_from(source);
        }
        Self { slots }
    }
}

/// Which of `range`'s two laws a fixed bound triple takes.
pub enum RangeLaw {
    /// Three NUMBERS: the `f64` accumulator, which the parity law requires and which every arity takes for a numeric
    /// triple.
    ///
    /// `first` is the authored `from` number, carried so the FIRST emitted value keeps the literal's spelling the `f64`
    /// accumulator would render away — the reference's range emits the argument itself for the first position, then
    /// computed doubles. `None` when the arity does not author a `from` (arity 1's `0` default has no spelling to
    /// keep).
    Numeric {
        /// The stepping cursor.
        cursor: RangeCursor,
        /// The authored `from` number, emitted instead of the cursor's first rendering; `None` when the arity defaults
        /// it.
        first: Option<Number>,
    },
    /// `range/3` with a bound that is not a number: the def-semantics.
    Value(ValueCursor),
}

/// Chooses `range`'s law for one fixed bound triple and opens its cursor.
///
/// The choice is by ARITY first and SHAPE second: arities 1 and 2 are the checked opcode, and arity 3 is a
/// definition-level `while` law, which cannot type-check. A numeric arity-3 triple still takes the `f64` cursor — the
/// two laws agree on numbers only up to accumulation, and the `f64` one is the byte-pinned answer.
///
/// # Errors
///
/// Returns `Range bounds must be numeric` for a non-number bound at arity 1 or 2, or an allocation failure.
pub fn open(bounds: RangeBounds, arity: usize, resources: &ResourceContext<'_>) -> Result<RangeLaw, EngineRunError> {
    let [from, upto, by] = bounds.slots;
    // The first emitted value keeps the authored `from` NUMBER, spelling and all — see [`RangeLaw::Numeric`]'s
    // `first` field. The gate is `to_f64(from)` being FINITE: a computed overflow (`1e308*2` is the exact `2E+308` in
    // jqf's arithmetic but the reference's infinite double) must keep the cursor's clamped rendering, while a literal
    // that underflows or rounds still carries the spelling the reference preserves. The tag is not part of the twin
    // (the accumulator path never carried one), only the spelling is.
    let first = match from.as_ref().map(Value::untagged) {
        Some(Value::Number(number)) if to_f64(number).is_finite() => Some(number.clone()),
        _ => None,
    };
    if arity < 3 {
        return Ok(RangeLaw::Numeric {
            cursor: RangeCursor::new(
                optional_bound(from.as_ref(), 0.0, resources)?,
                optional_bound(upto.as_ref(), 0.0, resources)?,
                optional_bound(by.as_ref(), 1.0, resources)?,
            ),
            first,
        });
    }
    let (Some(from), Some(upto), Some(by)) = (from, upto, by) else {
        return Err(EngineRunError::internal_contract("range/3 opened without three bounds"));
    };
    if let (Value::Number(from), Value::Number(upto), Value::Number(by)) =
        (from.untagged(), upto.untagged(), by.untagged())
    {
        // The `f64` cursor stays the byte-pinned accumulator for a numeric triple, but under the DEF's comparison law
        // (`def_law`), not the opcode's negation: `range/3` is a definition-level `while` law, so a NaN `upto` stops
        // the ascending walk at once (`[limit(5; range(0; nan; 1))] | length` is 0) and a NaN `current` stops the
        // descending one. Arities 1 and 2 stay on the opcode law above.
        return Ok(RangeLaw::Numeric {
            cursor: RangeCursor::def_law(to_f64(from), to_f64(upto), to_f64(by)),
            first,
        });
    }
    // `if $by > 0 … elif $by < 0 … else empty end`, under the TOTAL ORDER and spelled as the def's own three arms.
    let zero = Value::Number(Number::integer(Integer::from_i64(0)));
    let step = match compare(&by, &zero, resources)? {
        Ordering::Greater => Some(Step::Up),
        Ordering::Less => Some(Step::Down),
        Ordering::Equal => None,
    };
    Ok(RangeLaw::Value(ValueCursor {
        current: from,
        upto,
        by,
        step,
        stepped: false,
    }))
}

/// One optional bound as an `f64`, or the arity's own default for an argument this arity does not author.
fn optional_bound(value: Option<&Value>, default: f64, resources: &ResourceContext<'_>) -> Result<f64, EngineRunError> {
    match value {
        Some(value) => bound(value, resources),
        None => Ok(default),
    }
}

/// The dimension count `[range(upto)]` spans — `combinations(n)`'s count, defined as `[range(n)] | length` and
/// inheriting every one of its edges: a negative or zero `n` is zero dimensions, a fractional one rounds UP
/// (`[range(1.5)]` is `[0,1]`), and [`crate::semantics::arith::ceil_binary64`] answers zero for a nan `upto` rather
/// than falling through.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the ceiling is positive here and the cast saturates rather than wrapping"
)]
pub fn range_width(upto: f64) -> usize {
    crate::semantics::arith::ceil_binary64(upto) as usize
}

/// One `range` bound as an `f64`, or the single non-numeric bound message.
///
/// # Errors
///
/// Returns `Range bounds must be numeric` for any non-number bound.
pub fn bound(value: &Value, resources: &ResourceContext<'_>) -> Result<f64, EngineRunError> {
    match value.untagged() {
        Value::Number(number) => Ok(to_f64(number)),
        _ => Err(raise(NON_NUMERIC_BOUND, resources)),
    }
}

/// One accumulated `f64` as the jqf number it publishes.
///
/// The integral fast path is not only speed: it keeps `range`'s ordinary output an INTEGER rather than a decimal that
/// happens to have no fraction, which is what makes `[range(3)]` render `[0,1,2]`.
///
/// # Errors
///
/// Returns an allocation failure when the decimal spelling cannot be built.
pub fn emit(value: f64) -> Result<Value, EngineRunError> {
    // A non-finite value is a COMPUTED range position — a NaN `from` walks forever (`range(nan; 5)` streams `null`)
    // and a `-infinite` `from` walks downward — and it is a VALUE like the literals: the float Number carries its
    // exact bits and the render path spells them the reference way (`null` for NaN, the clamped widest binary64 for an
    // infinity). Routing it through the exact-decimal spelling below would fail, because `"NaN"` / `"inf"` are not JSON
    // number literals.
    if !value.is_finite() {
        return Ok(Value::Number(Number::float(jqf_data::Float::new(value))));
    }
    // `as` saturates and answers 0 for nan, so the round-trip below is the whole integrality test: it holds exactly
    // when `value` is an integer this `i64` represents. Negative zero declines it, so `-0` keeps its sign.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the cast saturates and the round-trip below rejects every inexact result"
    )]
    let integral = value as i64;
    #[expect(
        clippy::cast_precision_loss,
        clippy::float_cmp,
        reason = "the round-trip IS the exactness test; a lossy cast simply declines"
    )]
    let exact = integral as f64 == value && !(value == 0.0 && value.is_sign_negative());
    if exact {
        return Ok(Value::Number(Number::integer(Integer::from_i64(integral))));
    }
    // The shortest round-trip significand digits and exponent convert to the exact decimal directly — no intermediate
    // string, everything downstream of this point is exact-decimal again.
    Number::try_from_shortest_f64(value)
        .map(Value::Number)
        .map_err(|_| EngineRunError::allocation_failure())
}

/// How many dimensions a `combinations` subject carries.
#[derive(Clone, Copy, Debug)]
pub enum Dimensions {
    /// An array subject: this many dimensions, each read as `.[i]`.
    Array(usize),
    /// A subject whose `length` is zero, which is the ONE empty combination.
    None,
}

/// Reads a `combinations` subject's dimension count, with the error order.
///
/// The definition is `if length == 0 then [] else .[0][] as $x | …`, so a keyless subject fails `length` first
/// (`true` has no length) and every other non-array subject with a non-zero length fails the `.[0]` INDEX instead —
/// which is why `{"a":[1]} | combinations` says `Cannot index object with number (0)` and not anything about arrays.
///
/// # Errors
///
/// Returns the no-length error for a boolean, or the index error for any other non-empty non-array subject.
pub fn dimensions(subject: &Value, resources: &ResourceContext<'_>) -> Result<Dimensions, EngineRunError> {
    let untagged = subject.untagged();
    match untagged {
        Value::Array(array) => Ok(Dimensions::Array(array.len())),
        Value::Null => Ok(Dimensions::None),
        Value::Bool(_) => {
            let operand = message::dump_trunc_owned(untagged)?;
            let text = message::no_length_message(untagged.kind(), &operand)?;
            Err(raise(&text, resources))
        }
        Value::String(text) if text.as_str().is_empty() => Ok(Dimensions::None),
        Value::Object(object) if object.is_empty() => Ok(Dimensions::None),
        Value::Number(number) if is_zero(number) => Ok(Dimensions::None),
        other => {
            let accessor = message::render_array_key(0)?;
            let text = message::index_message(other.kind(), &accessor)?;
            Err(raise(&text, resources))
        }
    }
}

/// Whether a number is the zero `length`, which is the only number `combinations` treats as an empty dimension vector.
fn is_zero(number: &Number) -> bool {
    to_f64(number) == 0.0
}

/// `combinations`' odometer over a dimension vector.
///
/// It descends LAZILY, one dimension at a time, because the recursive definition does: `[[1,2],[]] | combinations` is
/// `[]` with no error, while `[[1],1] | combinations` raises — the second dimension is only ever examined once the
/// first has a child to pair it with. An odometer that validated the whole vector up front would raise on both.
#[derive(Debug)]
pub struct Odometer {
    /// The chosen child index at each dimension already descended into.
    cursor: Vec<usize>,
    /// How many dimensions the subject has.
    width: usize,
    /// Set once the walk has run out of tuples.
    spent: bool,
}

impl Odometer {
    /// Opens an odometer over `width` dimensions.
    pub const fn new(width: usize) -> Self {
        Self {
            cursor: Vec::new(),
            width,
            spent: false,
        }
    }

    /// The next combination, or `None` once every tuple has been emitted.
    ///
    /// Zero dimensions emit exactly ONE empty combination — `[] | combinations` is `[[]]`, not `[]` — which falls
    /// out of the descent loop finishing immediately rather than being special-cased.
    ///
    /// # Errors
    ///
    /// Returns the iterate error for a dimension that is not iterable, or an allocation failure.
    pub fn next(&mut self, subject: &Array, resources: &ResourceContext<'_>) -> Result<Option<Value>, EngineRunError> {
        loop {
            if self.spent {
                return Ok(None);
            }
            let mut empty_dimension = false;
            while self.cursor.len() < self.width {
                let length = dimension_len(subject, self.cursor.len(), resources)?;
                if length == 0 {
                    empty_dimension = true;
                    break;
                }
                self.cursor
                    .try_reserve(1)
                    .map_err(|_| EngineRunError::allocation_failure())?;
                self.cursor.push(0);
            }
            if empty_dimension {
                self.step(subject, resources)?;
                continue;
            }
            let tuple = self.tuple(subject, resources)?;
            self.step(subject, resources)?;
            return Ok(Some(tuple));
        }
    }

    /// Advances the rightmost dimension that still has a sibling, dropping the dimensions to its right so the next call
    /// re-descends them.
    fn step(&mut self, subject: &Array, resources: &ResourceContext<'_>) -> Result<(), EngineRunError> {
        while let Some(chosen) = self.cursor.pop() {
            let length = dimension_len(subject, self.cursor.len(), resources)?;
            if chosen + 1 < length {
                self.cursor.push(chosen + 1);
                return Ok(());
            }
        }
        self.spent = true;
        Ok(())
    }

    /// The combination the cursor currently names.
    fn tuple(&self, subject: &Array, _resources: &ResourceContext<'_>) -> Result<Value, EngineRunError> {
        let mut out = Array::try_with_capacity(self.cursor.len()).map_err(|_| EngineRunError::allocation_failure())?;
        for (position, chosen) in self.cursor.iter().enumerate() {
            let dimension = subject
                .get(position)
                .ok_or_else(|| EngineRunError::internal_contract("odometer past its subject"))?;
            let child = dimension_child(dimension, *chosen)
                .ok_or_else(|| EngineRunError::internal_contract("odometer past its dimension"))?;
            let child = child.clone();
            out.try_push(child).map_err(|_| EngineRunError::allocation_failure())?;
        }
        Ok(Value::Array(out))
    }
}

/// How many children dimension `position` has, raising the `.[i][]` iterate error for a dimension that cannot be
/// iterated.
fn dimension_len(subject: &Array, position: usize, resources: &ResourceContext<'_>) -> Result<usize, EngineRunError> {
    let dimension = subject
        .get(position)
        .ok_or_else(|| EngineRunError::internal_contract("odometer past its subject"))?;
    match dimension.untagged() {
        Value::Array(array) => Ok(array.len()),
        Value::Object(object) => Ok(object.len()),
        other => {
            let operand = message::dump_trunc_owned(other)?;
            let text = message::iterate_message(other.kind(), &operand)?;
            Err(raise(&text, resources))
        }
    }
}

/// One child of a dimension, in `.[]` order (array position, object insertion).
fn dimension_child(dimension: &Value, index: usize) -> Option<&Value> {
    match dimension.untagged() {
        Value::Array(array) => array.get(index),
        Value::Object(object) => object.get_index(index).map(jqf_data::ObjectEntry::value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Generator, RangeBounds, RangeCursor, RangeLaw, Termination, emit, open, range_width, row};
    use alloc::vec::Vec;
    use jqf_data::{Array, Number, ObjectBuilder, Value};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    /// One unlimited request ledger: the value cursor's `+` allocates its own result, which is charged at construction.
    fn ledger() -> ResourceContext<'static> {
        let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);
        let account = RequestAccount::try_new(limits).expect("test account");
        let work = WorkMeter::try_new_v1(1).expect("test work meter");
        ResourceContext::new(account, &CONTROL, work).expect("test ledger")
    }

    fn number(spelling: &str) -> Value {
        Value::Number(Number::try_json_literal(spelling).expect("literal"))
    }

    fn string(text: &str, _resources: &ResourceContext<'_>) -> Value {
        Value::try_string(text).expect("string")
    }

    fn empty_object(_resources: &ResourceContext<'_>) -> Value {
        Value::Object(ObjectBuilder::new().try_finish().expect("object"))
    }

    fn empty_array(_resources: &ResourceContext<'_>) -> Value {
        Value::Array(Array::try_new().expect("array"))
    }

    /// Opens `range/3` over three bounds and walks it, stopping at `ceiling` so a non-terminating shape can be examined
    /// without hanging the suite.
    fn walk(
        from: Value,
        upto: Value,
        by: Value,
        ceiling: usize,
        resources: &ResourceContext<'_>,
    ) -> Result<Vec<Value>, alloc::string::String> {
        let mut bounds = RangeBounds::new();
        bounds.set(0, from).expect("from");
        bounds.set(1, upto).expect("upto");
        bounds.set(2, by).expect("by");
        let law = open(bounds, 3, resources).map_err(|_| alloc::string::String::from("open"))?;
        let RangeLaw::Value(mut cursor) = law else {
            return Err(alloc::string::String::from("numeric law"));
        };
        let mut out = Vec::new();
        while out.len() < ceiling {
            match cursor.next(resources) {
                Ok(Some(value)) => out.push(value),
                Ok(None) => return Ok(out),
                Err(error) => {
                    let text = error
                        .typed_message()
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| alloc::string::String::from("raised"));
                    return Err(text);
                }
            }
        }
        Ok(out)
    }

    #[test]
    fn a_numeric_triple_keeps_the_f64_law_at_every_arity() {
        let resources = ledger();
        let mut bounds = RangeBounds::new();
        bounds.set(0, number("0")).expect("from");
        bounds.set(1, number("1")).expect("upto");
        bounds.set(2, number("0.1")).expect("by");
        // The f64 accumulator's ELEVEN, not exact-decimal's ten.
        let RangeLaw::Numeric { mut cursor, .. } = open(bounds, 3, &resources).expect("open") else {
            panic!("a numeric triple must not reach the value law");
        };
        let mut count = 0;
        while cursor.next().is_some() {
            count += 1;
        }
        assert_eq!(count, 11);
    }

    #[test]
    fn the_direction_is_the_total_order_not_a_numeric_sign() {
        let resources = ledger();
        // `null`, `false` and `true` all rank BELOW every number, so each is a DOWNWARD step and `0 > 3` fails
        // immediately.
        for step in [Value::Null, Value::Bool(false), Value::Bool(true)] {
            let walked = walk(number("0"), number("3"), step, 8, &resources).expect("walks");
            assert!(walked.is_empty());
        }
        // An object ranks ABOVE, so it is an UPWARD step: `0 < 3` holds, the value is emitted, and only the step that
        // follows it fails.
        let failure =
            walk(number("0"), number("3"), empty_object(&resources), 8, &resources).expect_err("the addition raises");
        assert_eq!(failure, "number (0) and object ({}) cannot be added");
    }

    #[test]
    fn the_step_is_deferred_past_the_emission() {
        let resources = ledger();
        // The `while` emits BEFORE it updates, so `range(null;3;"x")` publishes `null` and only then adds: `null + "x"`
        // is `"x"`, which no longer ranks below 3, and the cursor stops with one output.
        let walked = walk(Value::Null, number("3"), string("x", &resources), 8, &resources).expect("walks");
        assert_eq!(walked.len(), 1);
        assert!(matches!(walked[0], Value::Null));
    }

    #[test]
    fn a_zero_ranked_step_is_the_defs_empty_arm() {
        let resources = ledger();
        // `[range({};{};{})]` is `[]`: an object step ranks above zero, so it is UPWARD, and `{} < {}` is false. A step
        // that ranks EQUAL to zero never loops at all.
        let object = empty_object(&resources);
        let walked = walk(object.clone(), object.clone(), object, 8, &resources).expect("walks");
        assert!(walked.is_empty());
    }

    #[test]
    fn a_string_step_concatenates_and_never_terminates() {
        let resources = ledger();
        // `[limit(3; range("a";"z";"b"))]` is `["a","ab","abb"]`, and nothing in the cursor stops it — which is what
        // the row's second law declares.
        let walked = walk(
            string("a", &resources),
            string("z", &resources),
            string("b", &resources),
            3,
            &resources,
        )
        .expect("walks");
        assert_eq!(walked.len(), 3);
        let spelled: Vec<&str> = walked
            .iter()
            .map(|value| match value {
                Value::String(text) => text.as_str(),
                other => panic!("expected a string, got {other:?}"),
            })
            .collect();
        assert_eq!(spelled, alloc::vec!["a", "ab", "abb"]);
    }

    #[test]
    fn an_array_bound_ranks_and_adds_like_any_other_value() {
        let resources = ledger();
        // `[range([];[1];1)]` emits `[]` and then fails to add a number to it.
        let one = Array::try_from_vec(alloc::vec![number("1")]).expect("array");
        let failure = walk(empty_array(&resources), Value::Array(one), number("1"), 4, &resources)
            .expect_err("the addition raises");
        assert_eq!(failure, "array ([]) and number (1) cannot be added");
        // `[range([];[];[])]` is empty: `[] < []` is false, so no step runs.
        let walked = walk(
            empty_array(&resources),
            empty_array(&resources),
            empty_array(&resources),
            4,
            &resources,
        )
        .expect("walks");
        assert!(walked.is_empty());
    }

    #[test]
    fn the_range_row_declares_a_second_cooperative_law() {
        // The executor reads this to decide which resumptions owe the control check, and the value cursor is as
        // unbounded as `while` is.
        let second = row(Generator::Range)
            .second_law
            .expect("the range row carries a second law");
        assert_eq!(second.termination, Termination::Cooperative);
        assert_eq!(row(Generator::Range).termination, Termination::Structural);
    }

    #[test]
    fn every_row_answers_for_its_own_generator() {
        for generator in [
            Generator::Range,
            Generator::While,
            Generator::Until,
            Generator::Repeat,
            Generator::Recurse,
            Generator::Combinations,
        ] {
            assert_eq!(row(generator).generator, generator);
        }
    }

    #[test]
    fn the_step_is_accumulated_not_recomputed() {
        // The byte-parity fact: `from + i*step` answers 1.2 here, the reference answers 1.2000000000000002, and so must
        // this cursor.
        let mut cursor = RangeCursor::new(1.0, 2.0, 0.1);
        let values: Vec<f64> = core::iter::from_fn(|| cursor.next()).collect();
        assert_eq!(values.len(), 10);
        #[expect(clippy::float_cmp, reason = "the exact bit pattern is the fact under test")]
        {
            assert_eq!(values[2], 1.200_000_000_000_000_2);
            assert_ne!(values[2], 1.2);
        }
    }

    #[test]
    fn the_f64_accumulator_changes_the_count() {
        // `[range(0;1;0.1)] | length` is 11. Exact-decimal accumulation would answer 10; this is the row's whole reason
        // for existing.
        let mut cursor = RangeCursor::new(0.0, 1.0, 0.1);
        let mut count = 0;
        while cursor.next().is_some() {
            count += 1;
        }
        assert_eq!(count, 11);
    }

    #[test]
    fn a_zero_step_emits_nothing_and_an_infinite_step_emits_once() {
        let mut zero = RangeCursor::new(0.0, 3.0, 0.0);
        assert!(zero.next().is_none());
        let mut infinite = RangeCursor::new(0.0, 5.0, f64::INFINITY);
        assert_eq!(infinite.next(), Some(0.0));
        assert!(infinite.next().is_none());
    }

    #[test]
    fn a_descending_range_walks_down() {
        let mut cursor = RangeCursor::new(0.0, -5.0, -1.0);
        let values: Vec<f64> = core::iter::from_fn(|| cursor.next()).collect();
        assert_eq!(values, alloc::vec![0.0, -1.0, -2.0, -3.0, -4.0]);
        // The wrong-signed step emits nothing at all.
        let mut wrong = RangeCursor::new(0.0, 10.0, -1.0);
        assert!(wrong.next().is_none());
    }

    #[test]
    fn a_nan_bound_never_stops_the_walk_and_a_nan_step_emits_nothing() {
        // The C loop is `while (!(current >= upto))` ascending and `while (!(current <= upto))` descending, so a NaN
        // bound — every IEEE comparison false — never stops the walk. The first values pin the stream shape; the
        // walk itself is unbounded, which IS the `range(0; nan)` stream (the compat corpus pins the `first`/`nth`
        // cuts).
        let mut nan_upto = RangeCursor::new(0.0, f64::NAN, 1.0);
        let values: Vec<f64> = core::iter::from_fn(|| nan_upto.next()).take(5).collect();
        assert_eq!(values, alloc::vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        let mut nan_from = RangeCursor::new(f64::NAN, 5.0, 1.0);
        let values: Vec<f64> = core::iter::from_fn(|| nan_from.next()).take(3).collect();
        assert_eq!(values.len(), 3);
        assert!(values.iter().all(|value| value.is_nan()));
        let mut descending = RangeCursor::new(0.0, f64::NAN, -1.0);
        let values: Vec<f64> = core::iter::from_fn(|| descending.next()).take(4).collect();
        assert_eq!(values, alloc::vec![0.0, -1.0, -2.0, -3.0]);
        // A NaN step still emits NOTHING: neither sign branch holds, so `[range(0;5;nan)] | length` is 0, not an
        // infinite stream.
        let mut nan_step = RangeCursor::new(0.0, 5.0, f64::NAN);
        assert!(nan_step.next().is_none());
    }

    #[test]
    fn arity3_def_law_parts_from_the_opcode_exactly_at_nan() {
        // `range/3` is the `while` DEF, not the opcode, so its comparisons are positive-stated under the total order: a
        // NaN `upto` STOPS the ascending walk at once (`[limit(5; range(0; nan; 1))] | length` is 0) while the
        // descending test `$a > $b` stays TRUE for a NaN `$b` (NaN ranks below every number), so the descending walk
        // streams PAST a NaN bound and stops only on a NaN `current` (the inf-triple emits exactly one clamped value).
        // Arities 1-2 keep the C law above.
        let mut ascending = RangeCursor::def_law(0.0, f64::NAN, 1.0);
        assert!(ascending.next().is_none(), "NaN upto must stop arity-3 ascent");
        let mut descending = RangeCursor::def_law(0.0, f64::NAN, -1.0);
        let values: Vec<f64> = core::iter::from_fn(|| descending.next()).take(4).collect();
        assert_eq!(values, alloc::vec![0.0, -1.0, -2.0, -3.0]);
        // The inf-triple: `inf > -inf` holds, then `inf + -inf` is NaN, which fails the descending test — exactly one
        // value, the `[limit(5; range(1e308*2; -1e308*2; -1e308*2))] | map(tostring)`.
        let mut triple = RangeCursor::def_law(f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        assert_eq!(triple.next(), Some(f64::INFINITY));
        assert!(triple.next().is_none());
        // A NaN CURRENT stops the descending walk, the mirror of a NaN upto.
        let mut nan_current = RangeCursor::def_law(f64::NAN, f64::NEG_INFINITY, -1.0);
        assert!(nan_current.next().is_none());
        // Finite bounds are byte-identical to the C law under the def law.
        let mut finite = RangeCursor::def_law(0.0, 5.0, 1.0);
        let values: Vec<f64> = core::iter::from_fn(|| finite.next()).collect();
        assert_eq!(values, alloc::vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        let mut wrong = RangeCursor::def_law(0.0, 10.0, -1.0);
        assert!(wrong.next().is_none());
    }

    #[test]
    fn a_nan_step_takes_the_defs_descending_arm() {
        // The def is `if $by == 0 then empty elif $by > 0 … else <descending>`, so a NaN step — equal to neither
        // side of the sign test — walks DOWN: it emits the first value and stops once the NaN current fails `current
        // > upto`.
        let mut down = RangeCursor::def_law(3.0, 0.0, f64::NAN);
        assert_eq!(down.next(), Some(3.0));
        assert!(down.next().is_none());
        let mut down = RangeCursor::def_law(1.0, -1.0, f64::NAN);
        assert_eq!(down.next(), Some(1.0));
        assert!(down.next().is_none());
        // An ascending authoring with a NaN step is empty: `0 > 3` fails at once, exactly `range(0;3;nan)`.
        let mut empty = RangeCursor::def_law(0.0, 3.0, f64::NAN);
        assert!(empty.next().is_none());
        // A NaN `upto` stays a STOP for an ascending step (`range(0;nan;1)`), the def's mirror of the C law's unbounded
        // walk.
        let mut nan_upto = RangeCursor::def_law(0.0, f64::NAN, 1.0);
        assert!(nan_upto.next().is_none());
        // A zero step is the ONLY empty arm: `[range(0;3;0)] | length` is 0.
        let mut zero = RangeCursor::def_law(0.0, 3.0, 0.0);
        assert!(zero.next().is_none());
        // The inf-triple (`1e308*2` arithmetic): `inf > -inf` holds, then `inf + -inf` is NaN and fails the descending
        // test — one value.
        // Not a reference probe: the reference hangs on this exact shape.
        let mut triple = RangeCursor::def_law(f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        assert_eq!(triple.next(), Some(f64::INFINITY));
        assert!(triple.next().is_none());
    }

    #[test]
    fn emit_publishes_non_finite_positions_as_float_values() {
        // A computed range position can be non-finite (a NaN `from` walks forever, a `-infinite` `from` walks down). It
        // is a VALUE like the literals: the float Number carries its exact bits and the render path spells them the
        // reference way. Routing it through the exact-decimal spelling below would fail — `"NaN"` / `"inf"` are not
        // JSON number literals.
        let Value::Number(nan) = emit(f64::NAN).expect("nan emits") else {
            panic!("nan must publish a number");
        };
        assert!(nan.as_float().expect("float").get().is_nan());
        let Value::Number(inf) = emit(f64::INFINITY).expect("inf emits") else {
            panic!("infinity must publish a number");
        };
        assert!(inf.as_float().expect("float").get().is_infinite());
        let Value::Number(neg) = emit(f64::NEG_INFINITY).expect("-inf emits") else {
            panic!("-infinity must publish a number");
        };
        assert!(neg.as_float().expect("float").get().is_infinite());
        // Finite emission is untouched by the new arm.
        assert!(matches!(emit(3.0).expect("finite emits"), Value::Number(_)));
    }

    #[test]
    fn a_dimension_count_rounds_up_and_floors_at_zero() {
        assert_eq!(range_width(0.0), 0);
        assert_eq!(range_width(-1.0), 0);
        assert_eq!(range_width(1.0), 1);
        assert_eq!(range_width(1.5), 2);
        assert_eq!(range_width(2.5), 3);
    }

    #[test]
    fn an_integral_step_publishes_an_integer() {
        assert!(matches!(
            emit(3.0).expect("integral emits"),
            jqf_data::Value::Number(number) if number.category() == jqf_data::NumberCategory::Integer
        ));
        assert!(matches!(
            emit(0.5).expect("fractional emits"),
            jqf_data::Value::Number(number) if number.category() != jqf_data::NumberCategory::Integer
        ));
    }
}
