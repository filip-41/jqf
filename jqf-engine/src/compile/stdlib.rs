//! Transcribed stdlib and extension prelude source.
//!
//! The texts the compile pipeline token-gates and parses, plus the name lists
//! that must stay in step with those texts.

/// The standard library, as SOURCE.
///
/// Every definition here is transcribed source, verified by the corpus.
/// Writing them as source rather than as hand-built arena graphs is the point:
/// the semantics come from the real definition instead of a reimplementation,
/// which is how the `limit`/`first` edge-case table came out right by
/// construction.
///
/// Two rules govern what may live here, and both are about failing early rather
/// than late:
///
/// 1. **Non-recursive only.** `def` is inlining-only, so a recursive definition
///    (`until`, `while`, `repeat`, `recurse`) would be rejected at its first
///    call site — a worse failure than not offering it at all.
/// 2. **Only vocabulary the engine already has.** A definition written against
///    an unregistered builtin compiles fine as PRELUDE (nothing lowers until it
///    is called) and then fails at the call site; the `type`-based selectors
///    joined this list the moment `type/0` was registered, which is exactly the
///    intended cadence.
///
/// `last(g)` is spelled `[g] | .[-1:] | .[]` rather than a fold: the original
/// definition was not recoverable, and the observable law is that
/// `last(empty)` emits NOTHING. A `reduce g as $x (null; $x)` would emit
/// `null` instead, which was an earlier attempt and was caught.
///
/// The trailing `.` is the query the prelude parse needs to be a complete
/// program; it is discarded — only the `def` chain is read.
pub(crate) const STDLIB_PRELUDE: &str = "\
def isempty(g): first((g|false), true);\n\
def all(generator; condition): isempty(generator|condition and empty);\n\
def any(generator; condition): isempty(generator|condition or empty)|not;\n\
def all(condition): all(.[]; condition);\n\
def any(condition): any(.[]; condition);\n\
def all: all(.[]; .);\n\
def any: any(.[]; .);\n\
def first: .[0];\n\
def last: .[-1];\n\
def last(g): [g] | .[-1:] | .[];\n\
def values: select(. != null);\n\
def nulls: select(. == null);\n\
.";

/// Every name [`STDLIB_PRELUDE`] defines, for the token gate above.
///
/// This list must stay in step with the prelude. A name here that the prelude
/// does not define only costs a wasted parse; a prelude definition MISSING from
/// here is a correctness bug — its calls would report `not defined` — so
/// `compile/tests.rs` pins the two together.
pub(crate) const STDLIB_NAMES: &[&str] = &["isempty", "all", "any", "first", "last", "values", "nulls"];

/// Every name [`EXTENSION_PRELUDE`] defines, for the same token gate.
pub(crate) const EXTENSION_NAMES: &[&str] = &[
    "windows",
    "moving_sum",
    "moving_avg",
    "moving_min",
    "moving_max",
    "ewma",
    "deltas",
    "lag",
    "running",
    "counter",
];

/// The EXTENSION prelude: the window/rolling builtin family, as SOURCE
/// definitions beside the transcribed standard surface.
///
/// Every name here is an extension — none of them exists in the reference
/// surface (the `builtins` diff is the gate), so the stdlib const above stays
/// pure by rule and this one carries the extension surface. The prelude rules
/// bind identically: non-recursive `foreach`/`reduce` compositions only, and
/// every name they call is registered vocabulary.
///
/// The family is generator-parameterized (`limit($n; g)` shape): the stream is
/// an explicit generator argument, never `inputs`, so it composes with arrays,
/// `inputs`, and `--follow` alike. Argument order is `($param; g)` — the
/// window/alpha first, matching `limit/2`.
///
/// Spelling notes:
/// - `deltas`/`lag` share the `{p, first}` state shape; the first input emits
///   nothing, so the update computes `$x - .p` only when `.first` is false
///   (the original sketch subtracted on the first iteration, which raises
///   `number and null cannot be subtracted`).
/// - `running(f; g)` is the call-by-name law: the filter argument is inlined
///   in the CALLER's scope, so `f` sees the state as `.` and can never
///   reference the foreach element `$x` (`running(. + $x; g)` is rejected with
///   `$x is not defined`). The scan is state-only by construction.
/// - `moving_min`/`moving_max` recompute over the window array: O(n) per step
///   is the v1 ceiling; the monotonic-deque trick is the recognizer-era
///   upgrade (ponytail: ceiling documented, upgrade path named).
/// - Exact-decimal accumulation makes `moving_sum`/`moving_avg` drift-free
///   where a float accumulator drifts.
///
/// The trailing `.` is the query the prelude parse needs to be a complete
/// program; it is discarded — only the `def` chain is read.
pub(crate) const EXTENSION_PRELUDE: &str = "\
def windows($n; g): foreach g as $x ([]; (. + [$x])[-$n:]);\n\
def moving_sum($n; g): foreach g as $x ({q: [], s: 0}; .q += [$x] | .s += $x | if (.q|length) > $n then .s -= .q[0] | .q |= .[1:] else . end; .s);\n\
def moving_avg($n; g): foreach g as $x ({q: [], s: 0}; .q += [$x] | .s += $x | if (.q|length) > $n then .s -= .q[0] | .q |= .[1:] else . end; .s / (.q|length));\n\
def moving_min($n; g): foreach g as $x ([]; (. + [$x])[-$n:]; min);\n\
def moving_max($n; g): foreach g as $x ([]; (. + [$x])[-$n:]; max);\n\
def ewma($a; g): foreach g as $x (null; if . == null then $x else $a * $x + (1 - $a) * . end);\n\
def deltas(g): foreach g as $x ({p: null, first: true}; {p: $x} + (if .first then {first: false, skip: true} else {first: false, d: ($x - .p)} end); if .skip then empty else .d end);\n\
def lag(g): foreach g as $x ({p: null, first: true}; {p: $x, first: false} + (if .first then {skip: true} else {v: .p} end); if .skip then empty else .v end);\n\
def running(f; g): foreach g as $x (null; f);\n\
def counter(g): foreach g as $x (0; . + 1);\n\
.";
