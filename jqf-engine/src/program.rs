//! Engine-owned compiled program arena.
//!
//! This module owns the executable generator IR's storage vocabulary: the dense
//! [`ProgramNodeId`] index, the [`ProgramNode`] arena node, its [`StageStep`]
//! components, and the [`Program`] container that holds the node arena, its
//! `root`, and the single derived analysis fact this module produces (the
//! codec-pushdown split from [`crate::analysis`]). It also charges the arena's
//! backing and key bytes to the request ledger.
//!
//! It does not parse source, lower an AST, derive the split or fuse the arena
//! (that is [`crate::analysis`]'s job — this module only stores the fused
//! result), lower into a codec requirement, or execute the graph.
//!
//! **Path-normal form (the arena-at-rest invariant).** Lowering emits
//! [`ProgramNode::FlatMap`] for pipe/group-postfix composition,
//! [`ProgramNode::Choice`] for comma, and
//! [`ProgramNode::CollectArray`]/[`ProgramNode::ConstructObject`] for the
//! constructors; a [`ProgramNode::Stage`] carries a [`StageStart`] (`Current` for
//! a path floor, `Literal` for a scalar-literal producer). [`crate::analysis`]
//! fuses every `FlatMap(Stage{s}, Stage{Current})` into one `Stage{s}` — a
//! `Literal`-start BODY blocks its own fusion (it ignores the upstream value,
//! changing cardinality). A stored [`Program`]'s arena is therefore in
//! **path-normal form**: every fusable `FlatMap` is fused, and no fusable
//! `Stage∘Stage` `FlatMap` survives. What remains is a graph of `Stage`, `Choice`,
//! `CollectArray`, `ConstructObject`, and the `FlatMap` nodes a `Choice`,
//! constructor, or literal body blocked — a bare-`Stage` root for the pure
//! path/iteration/literal subset (the landed fast path), a `Choice`/constructor
//! root for a top-level comma or `[…]`/`{…}`, and a `FlatMap` root only when one
//! side blocks fusion.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use jqf_data::Value;

use crate::analysis::{
    AntiJoinScan, CorrelatedScan, ElementBoundary, PartialSort, ProjectionClass, PushdownSplit, anti_joins, classify,
    correlated_scans, partial_sorts,
};
use jqf_builtins::registry::{BuiltinDispatch, BuiltinOverloadId, Evaluator, SemanticRevision, dispatch};

/// Dense identifier of one node within a [`Program`]'s arena.
///
/// The identifier is a `u32` index into the backing `nodes` vector, never a
/// source name or a pointer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProgramNodeId(u32);

impl ProgramNodeId {
    /// The identifier of a freshly built arena's root node. Production code
    /// reads the root out of the [`Program`] that owns the arena; this names it
    /// for the tests that build one by hand.
    #[cfg(test)]
    pub const ROOT: Self = Self(0);

    /// The dense identifier for the node at `index`, or `None` when the index
    /// exceeds the arena's `u32` addressing bound.
    pub fn from_index(index: usize) -> Option<Self> {
        u32::try_from(index).ok().map(Self)
    }

    /// The identifier as a `usize` arena index.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// One static forward step within a [`ProgramNode::Stage`]: a static access
/// paired with its per-component optional (`?`) flag.
///
/// The optional flag is interpretation state, never access. It changes only how
/// a per-step type mismatch is read (the exact-step suppression law in
/// [`crate::exec`]); requirement lowering and the codec-pushdown split ignore it
/// entirely, so `.a? | .b` and `.a | .b` lower to the identical requirement.
#[derive(Clone, Debug)]
pub struct StageStep {
    access: StepAccess,
    optional: bool,
    /// Set when the step was HOISTED out of an `Alternative` operand by the
    /// spine-node fusion: the miss the fallback handles left the
    /// alternative's own suppression region, so the strict dial treats the
    /// step's cell sites as marked. Lenient never reads it (the codec pushdown
    /// ignores it, so a hoisted prefix is byte-identical either way).
    suppressed: bool,
}

impl StageStep {
    /// Pairs a static `access` with its authored optional (`?`) flag.
    pub const fn new(access: StepAccess, optional: bool) -> Self {
        Self {
            access,
            optional,
            suppressed: false,
        }
    }

    /// Marks this step suppression-derived: a fusion hoist moved it
    /// out of an `Alternative` operand, so its cell sites stay marked.
    pub fn mark_suppressed(&mut self) {
        self.suppressed = true;
    }

    /// Whether this step's MISMATCH-CELL sites are intent-suppressed: the
    /// authored `?` marks the site itself, and a fusion hoist marks
    /// steps it moved out of an `Alternative` operand. A step's `?` semantics
    /// (converting a NATIVE mismatch to a skip) stay separate — this answers
    /// only the dial's cell suppression (the old
    /// `is_suppressed` accessor's only reader was this).
    pub const fn cell_suppressed(&self) -> bool {
        self.optional || self.suppressed
    }

    /// The static access this step performs.
    pub const fn access(&self) -> &StepAccess {
        &self.access
    }

    /// Mutable access to the step's accessor, for the module-merge rebase.
    pub fn access_mut(&mut self) -> &mut StepAccess {
        &mut self.access
    }

    /// Whether the authoring component carried a per-component optional `?`.
    pub const fn is_optional(&self) -> bool {
        self.optional
    }
}

/// The dense index of one variable binding slot in the executor's env vector.
///
/// Lowering assigns ONE UNIQUE SLOT PER BINDER OCCURRENCE in the program (the
/// binding-slot law): there is no reuse across sibling
/// scopes and no save/restore machinery, because streaming emissions run
/// downstream code while binder frames are still live. The executor never sees a
/// variable NAME — the lowerer resolves every `$x` reference against its lexical
/// scope stack into one of these indices.
pub type VarSlot = u32;

/// The dense index of one recursive-definition FILTER-parameter slot.
///
/// Distinct from [`VarSlot`]: a filter parameter is a runtime closure
/// (argument graph plus the env captured at the call), not a bound value.
/// Unique per parameter occurrence of a recursive callable, resolved at
/// lower time the way variable slots are.
pub type FilterSlot = u32;

/// One engine-cursor slot in a machine's cursor vector: the machine-level
/// identity a `~x` binding occurrence owns, written by an
/// [`ProgramNode::EngineBind`] and read by [`ProgramNode::EnginePull`] steps.
/// Unique per binder OCCURRENCE, exactly like [`VarSlot`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EngineSlot(pub u32);

impl EngineSlot {
    /// The zero-based slot index.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// The dense index of one `label $out` OCCURRENCE.
///
/// Assigned ONE PER LABEL OCCURRENCE in PROGRAM ORDER, under the same law as
/// [`VarSlot`] — no reuse across sibling scopes, no save/restore — and for the
/// same reason: a streaming emission can run downstream code while an enclosing
/// label frame is still live.
///
/// **Program order is load-bearing, not cosmetic.** The reference renders a
/// caught break as `{"__jq": n}` where `n` is the label's occurrence index
/// (nested `$a`/`$b` give 0/1, siblings give 0/1). Because the marker is
/// user-observable through `catch`, allocating in program order is what makes
/// jqf's rendered marker byte-match the reference's.
pub type LabelSlot = u32;

/// One authored slice bound (`.[a:b]`, `.[:b]`, `.[a:]`).
///
/// The bound is stored EXACTLY as authored — never normalized at lower time.
/// Normalizing to an `i64` is unsound: the len-relative rounding law
/// ([`crate::exec`]'s slice resolver) keys the wrap off the AUTHORED sign, and
/// no integer can represent "rounded to 0 but authored negative" (`.[:-0.5]`
/// takes the whole array while `.[:-0.0]` takes none). A non-numeric authored
/// bound is likewise legal to LOWER — `.["a":2]` yields `null` over a `null`
/// input, the integer-bound raise over an array, and the index-class mismatch
/// over an object — so its outcome is runtime-input-dependent, not a compile
/// rejection.
#[derive(Clone, Debug)]
pub enum SliceBound {
    /// An omitted bound (`.[:b]`'s start, `.[a:]`'s end): the container's own
    /// end. It renders as `null` in the index-class mismatch operand, exactly
    /// like an authored `null` bound.
    Open,
    /// An authored scalar bound, stored verbatim (`1`, `-0.5`, `"a"`, `true`,
    /// `null`). An authored `null` behaves as [`SliceBound::Open`] at runtime.
    Literal(Value),
    /// A bound by BOUND VARIABLE (`.[$a:$b]`), resolved against the env slot at
    /// runtime under the same law and type dispatch as a literal bound.
    Var(VarSlot),
}

/// The authored start and end of one [`StepAccess::Slice`] step, boxed so a
/// slice step costs one pointer rather than two `Value`s in EVERY program's
/// [`StageStep`] (the step size is pinned by a unit test).
#[derive(Clone, Debug)]
pub struct SliceBounds {
    /// The authored start bound.
    pub start: SliceBound,
    /// The authored end bound.
    pub end: SliceBound,
}

/// The signed `i64`-or-Open normalization of one authored bound pair — the
/// BOUNDARY LAW.
///
/// The authored form stays the executor's authority (see [`SliceBound`]); this
/// is a SECOND, lossy reading built only for the codec pushdown, and every
/// spelling no signed integer can carry DECLINES rather than approximating:
///
/// - `start := floor(v)`, `end := ceil(v)` — the same rounding directions the
///   runtime resolver applies, hoisted to lower time because rounding commutes
///   with the len-relative wrap for every bound the law admits;
/// - a STRICTLY NEGATIVE end whose ceil is `0` becomes OPEN. That is the one
///   case an integer provably cannot carry: `.[:-0.5]` wraps to `len - 0.5` and
///   ceils to `len` (the whole array), while `.[:-0.0]` stays at `0` (empty), so
///   the two would normalize to the same integer. Open is exactly `len`;
/// - a magnitude beyond `i64` SATURATES, which is sound because every bound past
///   the container's own length clamps to an edge anyway;
/// - an authored `null` bound is Open (its runtime behaviour), and every
///   non-numeric literal or [`SliceBound::Var`] bound DECLINES — their outcome is
///   runtime-input-dependent (a raise, a `null`, or the index-class mismatch),
///   which is not a range.
///
/// A STRICTLY-NEGATIVE resolved bound normalizes to its SIGNED value — the
/// `SliceRange` the consumers carry then owns the len-relative reading, and the
/// RESOLVER resolves it against the observed container length exactly as it
/// resolves a signed [`StepAccess::Index`] (the strict JSON codec's range scan
/// with a two-pass count-then-cut arm). Consumers whose resolution
/// is document-side rather than codec-side (the count and element-iteration
/// demand rows, which resolve in `jqf-data`) keep declining a strictly-negative
/// bound on their own recognizer side: their early-stop walk gives up the whole
/// scan a len-relative resolution needs.
impl SliceBounds {
    pub fn try_normalize(&self) -> Option<(Option<i64>, Option<i64>)> {
        let admit = |bound: NormalizedBound| match bound {
            NormalizedBound::Open => Some(None),
            NormalizedBound::At(value) => Some(Some(value)),
            NormalizedBound::Decline => None,
        };
        let start = admit(normalize_bound(&self.start, true))?;
        let end = admit(normalize_bound(&self.end, false))?;
        Some((start, end))
    }
}

/// One authored bound's lower-time reading.
enum NormalizedBound {
    /// The container's own edge (an omitted bound, an authored `null`, or the
    /// strictly-negative end that ceils to zero).
    Open,
    /// An exact signed position.
    At(i64),
    /// A spelling no signed integer can carry: the pushdown DECLINES.
    Decline,
}

/// Reads one authored bound under the boundary law (see [`SliceBounds`]).
fn normalize_bound(bound: &SliceBound, is_start: bool) -> NormalizedBound {
    let value = match bound {
        SliceBound::Open => return NormalizedBound::Open,
        SliceBound::Var(_) => return NormalizedBound::Decline,
        SliceBound::Literal(value) => value,
    };
    let number = match value {
        Value::Null => return NormalizedBound::Open,
        Value::Number(number) => number,
        _ => return NormalizedBound::Decline,
    };
    let raw = jqf_builtins::semantics::order::to_f64(number);
    if raw.is_nan() {
        return NormalizedBound::Decline;
    }
    let rounded = if is_start {
        jqf_builtins::semantics::arith::floor_binary64(raw)
    } else {
        jqf_builtins::semantics::arith::ceil_binary64(raw)
    };
    // The one spelling no integer carries: an end authored strictly negative
    // that rounds to zero means the container's own end, not position zero.
    if !is_start && raw < 0.0 && rounded == 0.0 {
        return NormalizedBound::Open;
    }
    NormalizedBound::At(saturating_i64(rounded))
}

/// Saturating projection of an integral binary64 onto `i64`, which is sound
/// because every bound past the container's own length clamps to an edge —
/// the body is exactly the float→int `as` cast, which has saturated (with
/// NaN→0) since Rust 1.45.
#[allow(
    clippy::cast_possible_truncation,
    reason = "the float->int cast saturates by definition, which is exactly the law this helper names"
)]
fn saturating_i64(value: f64) -> i64 {
    value as i64
}

/// The static access one [`StageStep`] performs.
///
/// Mirrors the recognized path component exactly: keys are decoded semantic
/// strings and indices are signed to preserve negative-index semantics.
/// [`StepAccess::Each`] is the `.[]` iteration component — array/object value
/// iteration with zero-to-many output cardinality per input.
#[derive(Clone, Debug)]
pub enum StepAccess {
    /// A decoded semantic object key.
    Key(String),
    /// A signed array position preserving negative-index semantics.
    Index(i64),
    /// Array/object value iteration (`.[]`): fans one container input out into
    /// one output per child, arrays in position order and objects in insertion
    /// order. It is the vertical's first zero-to-many cardinality component and
    /// the first step the executor navigates rather than the codec pushdown.
    Each,
    /// A dynamic index by BOUND VARIABLE (`.[$i]`, `.o[$x]`): ONE step variant
    /// that type-dispatches at RUNTIME on the bound value, because the lowerer
    /// cannot know whether `$x` holds a key or an index. A string reads an object
    /// key, a number reads an array position (truncating toward zero, wrapping
    /// negatives), any other bound type is the index-class mismatch — all
    /// suppressed by this step's own `?`. General expression indices stay
    /// rejected at lower time.
    DynVar(VarSlot),
    /// Recursive descent (`..`): a SECOND zero-to-many cardinality component,
    /// and the first RECURSIVE one. It emits its input FIRST and then every
    /// descendant in document order (pre-order, depth-first) — a scalar emits
    /// itself alone, so the step has no error path at all. The executor fans it
    /// out with a frame whose children resume AT THIS STEP (recursing) while the
    /// self-emission continues at the next one.
    Descend,
    /// An array/string slice (`.[a:b]`, `.[:b]`, `.[a:]`) over its AUTHORED
    /// bounds. Every part of the semantics is runtime state: the container's
    /// length decides the wrap, the input's type decides between a subrange, a
    /// `null` pass-through, the integer-bound raise, and the index-class
    /// mismatch, and the bound values themselves may live in env slots.
    Slice(Box<SliceBounds>),
    /// A `.@name` node/fact accessor (the direct and quoted forms). The selector
    /// is a STATIC string at lower time: the accessor reads the input node's
    /// attached fact whose ROLE is the selector text, or — for the `tag`
    /// selector — the node's intrinsic tag. An input that carries no such fact
    /// answers `null` (the tree's standing spelling for an absent fact), so the
    /// step has no error path of its own. The dynamic `@(expr)` form is
    /// [`Self::DynNodeAccessor`].
    NodeAccessor(String),
    /// A `.&name` markup-attribute accessor (the direct and quoted forms). The
    /// selector is a STATIC string at lower time: the accessor reads the input
    /// node's attached attribute fact — ROLE `attribute`, KIND the selector
    /// text — answering `null` when the node carries none. The dynamic
    /// `&(expr)` form is [`Self::DynAttribute`].
    Attribute(String),
    /// A dynamic `.@(name_expr)` node/fact accessor. The selector is a bound
    /// slot whose runtime value must be a string; a non-string is a type
    /// mismatch. The string is the fact ROLE (or `tag`), with the
    /// `comment_head` alias normalized to `comment` the same way a static
    /// selector is. Absent facts still answer `null`. This is not
    /// [`Self::DynVar`]: a number is never an accessor name.
    DynNodeAccessor(VarSlot),
    /// A dynamic `.&(name_expr)` markup-attribute accessor. The selector is a
    /// bound slot whose runtime value must be a string; a non-string is a type
    /// mismatch. The string is the attribute KIND. Absent attributes still
    /// answer `null`. This is not [`Self::DynVar`]: a number is never an
    /// attribute name.
    DynAttribute(VarSlot),
}

/// What value a [`ProgramNode::Stage`] starts its steps from.
///
/// [`StageStart::Current`] is the ordinary path floor: the stage walks its steps
/// over the input value. [`StageStart::Literal`] is a scalar-literal producer
/// (`null`, `true`, `false`, a number, a plain string): the stage IGNORES its
/// input and starts from the owned literal, then applies any postfix steps to it
/// (`3[]?`, `(1).a?`). This is the `start` discriminant the pipe vertical
/// reserved, landing with its producing (literals) vertical.
#[derive(Clone, Debug)]
pub enum StageStart {
    /// The stage walks its steps over the input value (the ordinary path floor).
    Current,
    /// The stage ignores its input and starts from this owned scalar literal.
    Literal(Value),
    /// The stage ignores its input and starts from the value bound in this env
    /// slot (`$x`, `$r.a[1]`, `$row[]`). Like [`StageStart::Literal`] it blocks
    /// cross-stage fusion and takes no pushdown, but unlike it the value is
    /// env-read at EVAL time rather than arena-cloned at seed, so it charges no
    /// arena bytes (the slot's bytes are the executor's per-slot residency).
    Variable(VarSlot),
}

/// One member of a [`ProgramNode::ConstructObject`]: a key producer paired with a
/// value producer.
///
/// Both are ordinary generator graphs evaluated over the constructor's ORIGINAL
/// input. A static-identifier or string key lowers to a singleton
/// [`StageStart::Literal`] string producer (AGENTS.md: static keys are singleton
/// semantic strings), a parenthesized `(expr)` key to `expr`'s graph, and a
/// shorthand `{a}` to key `"a"` with value `.a`. Every key output must be a core
/// string; the value producer's outputs are captured as the member's values.
/// The frame-free shape of a [`ProgramNode::Binary`], filled after fuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BinaryShape {
    /// Not the identity × literal/slot pair; drive through frames.
    Framed,
    /// Left is identity, right is a step-less literal.
    IdentityLiteral,
    /// Left is identity, right is a step-less variable slot.
    IdentitySlot(VarSlot),
}

#[derive(Clone, Debug)]
pub struct ObjectMemberNode {
    /// The graph producing this member's key(s) — a singleton string for a static
    /// key, or a `(expr)` generator for a dynamic one.
    pub key: ProgramNodeId,
    /// The graph producing this member's value(s).
    pub value: ProgramNodeId,
    /// When the key graph is a step-less string literal, the key text.
    /// Runtime skips evaluating the key graph. `None` is the conservative
    /// dynamic-key path. Filled after fuse.
    pub static_key: Option<String>,
}

/// The two short-circuiting logical operators `and`/`or`.
///
/// They are a control-flow family separate from [`BinaryKind`]: their drive law
/// is LEFT-outer (the left operand is the outer loop, each output short-circuiting
/// or expanding over the right), the inverse of [`ProgramNode::Binary`]'s
/// hard-pinned RIGHT-outer Cartesian order. jqf's Binary drive law leaves no room
/// for a second drive order under one tag, so `and`/`or` are a separate
/// `Logical` family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogicalOp {
    /// `and` — short-circuits on the first falsy left output (emits `false`).
    And,
    /// `or` — short-circuits on the first truthy left output (emits `true`).
    Or,
}

/// Which counted-stream law a [`ProgramNode::Counted`] applies to its source.
///
/// The three rows are the reference's three counted builtins, and they differ only in what
/// the countdown does with the item it just paid for: `limit` publishes it and
/// cuts the source when the count is spent, `skip` withholds it while the count
/// is unspent and publishes every item after, and `nth` publishes exactly the
/// first item `skip` would have and cuts there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountedKind {
    /// `limit($n; f)` — publish the first `ceil($n)` items, then cut the source.
    Limit,
    /// `skip($n; f)` — withhold the first `floor($n)` items, publish the rest.
    Skip,
    /// `nth($n; f)` — publish the item at index `floor($n)` and cut the source.
    Nth,
}

/// Which projection an [`ProgramNode::EnginePull`] performs — the only two the
/// engine-binding protocol admits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnginePullKind {
    /// `~x.next`: advance the cursor one step and emit that value; empty once
    /// the cursor is exhausted (idempotently).
    Next,
    /// `~x.rest`: emit every remaining value as a stream — exactly repeated
    /// `.next` pulls until exhaustion, by definition.
    Rest,
}

/// The value-level binary operator family, re-exported from the vertical that
/// owns its law ([`jqf_builtins::semantics::binary`]) because it is a FIELD of this
/// arena's `Binary` node. The definition lives beside the dispatch it drives;
/// the arena keeps the `program::BinaryKind` spelling its nodes are written in.
pub use jqf_builtins::semantics::binary::BinaryKind;

/// One node in the engine program arena.
///
/// Lowering emits [`ProgramNode::Stage`] for identity/static paths, iteration and
/// scalar literals, [`ProgramNode::FlatMap`] for pipe and group-postfix
/// composition, [`ProgramNode::Choice`] for comma, and
/// [`ProgramNode::CollectArray`]/[`ProgramNode::ConstructObject`] for the `[…]`
/// and `{…}` constructors; [`crate::analysis`] fuses every `FlatMap(Stage, Stage)`
/// (with a [`StageStart::Current`] body) back into one `Stage`, so a stored
/// [`Program`] is in path-normal form (no fusable `Stage∘Stage` `FlatMap`
/// survives). [`ProgramNode::Call`] is a resolved `Evaluator` builtin the
/// executor dispatches by id (`Lowering` builtins expand before the arena). The
/// control-flow families [`ProgramNode::Conditional`] (`if`/`elif`/`else`),
/// [`ProgramNode::Alternative`] (`//`), and [`ProgramNode::Logical`] (`and`/`or`)
/// each survive fusion like `Choice` and contribute zero pushdown prefix on the
/// spine. [`ProgramNode::Empty`] is the zero-cardinality `empty` filter, and
/// [`ProgramNode::Bind`]/[`ProgramNode::Reduce`]/[`ProgramNode::Foreach`] are the
/// variable-binding family (`as`, `reduce`, `foreach`); all four likewise survive
/// fusion and contribute zero pushdown prefix.
/// One compiled RECURSIVE definition body: the shared body graph plus the
/// value-parameter binder slots and filter-parameter closure slots it reads.
/// Call sites reference it by index during lowering; the call-def resolution
/// pass rewrites the arena into self-contained [`ProgramNode::CallDef`] nodes
/// before analysis.
#[derive(Debug)]
pub struct CallableDef {
    /// The shared body graph.
    pub body: ProgramNodeId,
    /// The value-parameter binder slots the body reads.
    pub param_slots: Vec<VarSlot>,
    /// The filter-parameter closure slots the body reads.
    pub filter_slots: Vec<FilterSlot>,
}

/// The arena of lowered executable nodes.
///
/// `Clone` exists for the arena-level REWRITE (the slurp-aggregate
/// fold): the rewrite copies the map body's subgraph into a new program
/// arena. It is not a license to clone nodes in the hot path — the arena is
/// owned and charged once; a clone re-charges the allocator for the copied
/// literals.
#[derive(Clone, Debug)]
pub enum ProgramNode {
    /// One filter floor: a start value plus an ordered list of static forward
    /// steps. A [`StageStart::Current`] stage with empty `steps` is identity; a
    /// [`StageStart::Literal`] stage is a scalar-literal producer.
    Stage {
        /// What value the steps start from (input, or an owned scalar literal).
        start: StageStart,
        /// The ordered static forward steps this stage walks.
        steps: Vec<StageStep>,
    },
    /// Pipe / group-postfix: evaluate `body`'s graph over every value
    /// `upstream`'s graph produces. Lowering emits it for `a | b` and for a
    /// postfix chain on a non-`Stage` group base; [`crate::analysis`] fuses it
    /// away when both sides are `Stage`s. A surviving `FlatMap` is therefore
    /// always `Choice`-coupled (a `Choice` on one side blocked fusion), and its
    /// executor arm drives it directly.
    FlatMap {
        /// The graph producing the values `body` runs over.
        upstream: ProgramNodeId,
        /// The graph run once per `upstream` value.
        body: ProgramNodeId,
    },
    /// Comma `a, b`: executable choice. Evaluate the complete `left` filter over
    /// the input, then the complete `right` filter over the SAME input, in that
    /// order (the AGENTS.md law). `a, b, c` lowers left-associatively as
    /// `Choice(Choice(a, b), c)`. Choice is the first non-linearizable
    /// composition — it survives fusion, so a stored [`Program`] may hold it.
    Choice {
        /// The graph evaluated first over the input.
        left: ProgramNodeId,
        /// The graph evaluated second over the same input.
        right: ProgramNodeId,
    },
    /// Array construction `[body?]`: collect every value the optional `body`
    /// generator produces over the input into one owned `Value::Array`, then emit
    /// that single array. An absent body is the empty array `[]`. Like `Choice`,
    /// it survives fusion and is a semantic construction barrier — located member
    /// outputs materialize to owned values at capture.
    CollectArray {
        /// The optional generator whose outputs are collected; `None` is `[]`.
        body: Option<ProgramNodeId>,
    },
    /// The COUNT-ONLY collect: `[body] | length`, recognized at pipe lowering
    /// and executed WITHOUT materializing the array. The collect body runs exactly as it would under
    /// `CollectArray` — same drive, same error propagation, same empty-body
    /// law — and the frame counts its outputs instead of pushing them into an
    /// owned array; the single published value is that count as an exact
    /// integer. Semantically identical to `[body] | length` by construction:
    /// `length` of the constructed array IS its element count, the collect
    /// publishes nothing before completion, and a body error aborts before
    /// anything publishes in both spellings. The count fits `i64` for any
    /// array the process can hold (each element costs at least one byte of
    /// array storage, so 2^63 elements exceed any memory ceiling). The
    /// materialization the barrier used to pay per emission — the
    /// `[..]` gap — disappears entirely when the array's only consumer is its
    /// length. The absent-body arm is the empty collect's contract, unreachable
    /// from the current lowering (`[] | length` folds to a literal stage).
    CountCollect {
        /// The optional generator whose outputs are counted; `None` counts zero.
        body: Option<ProgramNodeId>,
    },
    /// Object construction `{members}`: the deterministic depth-first Cartesian
    /// product of the members' key and value producers (later members vary
    /// fastest, key-producer outer / value-producer inner within a member), each
    /// completed combination finished through `jqf_data::ObjectBuilder` into one
    /// owned `Value::Object`. Survives fusion; a semantic construction barrier.
    ConstructObject {
        /// The ordered members, each a key producer paired with a value producer.
        members: Vec<ObjectMemberNode>,
    },
    /// A resolved builtin call the executor dispatches by stable identity
    /// (`length`, `keys`, `select`). The compiler resolves `(name, arity)`
    /// against the registry BEFORE lowering `args`, storing the compact stable
    /// [`BuiltinOverloadId`] plus its separate [`SemanticRevision`] — never the
    /// source name. `Lowering`-kind builtins (`map`) never reach the arena: they
    /// expand at lower time, so every stored `Call` is an `Evaluator`. Like
    /// `Choice`, a `Call` survives fusion; on the upstream spine it contributes
    /// zero pushdown prefix (a call reads the whole document authority).
    Call {
        /// The resolved overload's stable id, dispatched by the executor.
        overload: BuiltinOverloadId,
        /// The overload's pinned semantic revision, stored beside the id.
        revision: SemanticRevision,
        /// The lowered argument filter graphs (one per `Filter` parameter), each
        /// driven by the evaluator over the call's original input; empty for the
        /// arity-0 evaluators.
        args: Vec<ProgramNodeId>,
        /// The evaluator payload, resolved at lower time. Runtime is a load,
        /// not a name/arity lookup.
        payload: Evaluator,
    },
    /// A call to a RECURSIVE user definition, compiled once as a callable body
    /// and referenced by every call site (recursion makes pure inlining
    /// inexpressible). The body and its value-parameter binder slots are shared
    /// arena facts; the ARGS are the call site's own lowered VALUE-parameter
    /// graphs, evaluated over the call's original input (`arg as $x` law).
    /// Filter-parameter graphs live in `filter_args` and are captured as
    /// runtime closures rather than evaluated at the call: each use in the
    /// body evaluates the captured graph against the captured env with the
    /// current `.` as input. Each invocation runs the body against a copied
    /// binder overlay, so nested recursion re-binds the same slots without
    /// clobbering the outer frame.
    CallDef {
        /// The compiled definition body graph, shared by every call site.
        body: ProgramNodeId,
        /// The value-parameter binder slots, in declaration order.
        param_slots: Vec<VarSlot>,
        /// The filter-parameter closure slots, in declaration order.
        filter_slots: Vec<FilterSlot>,
        /// The call-site VALUE-parameter argument graphs, one per `$` parameter.
        args: Vec<ProgramNodeId>,
        /// The call-site FILTER-parameter argument graphs, one per non-`$`
        /// parameter. Captured as closures at the call, not evaluated here.
        filter_args: Vec<ProgramNodeId>,
        /// Whether this self-call is in a tail position within its enclosing
        /// callable body (set during analysis after `fuse_with_callables`).
        tail: bool,
    },
    /// A use of a recursive definition's FILTER parameter: evaluate the
    /// closure bound to `slot` (the argument graph captured at the call,
    /// plus that call's env) over the current input. Zero to many outputs,
    /// like any other generator. Distinct from [`Self::CallDef`]: this is
    /// the parameter USE, not a call of the definition.
    CallFilter {
        /// The filter-parameter slot this use reads.
        slot: FilterSlot,
    },
    /// A binary operator `left op right` (`+ - * / %`, `== != < <= > >=`): the
    /// deterministic RIGHT-outer Cartesian product of the two operand graphs, each
    /// evaluated over the SAME input, with `op` applied per pair to owned operands
    /// (materialized at the op barrier where the operator needs owned results).
    /// Like `Choice`, it survives fusion and contributes zero pushdown prefix on
    /// the spine (both operands read the whole input authority).
    Binary {
        /// The operator applied at each Cartesian pair.
        op: BinaryKind,
        /// The left operand graph (the inner, fastest-varying Cartesian loop).
        left: ProgramNodeId,
        /// The right operand graph (the outer, slowest-varying Cartesian loop).
        right: ProgramNodeId,
        /// Filled after fuse. Whether this pair is the frame-free identity
        /// × literal/slot shape. `Framed` is the conservative default.
        shape: BinaryShape,
    },
    /// String interpolation `"a\(x)b\(y)"`: the Cartesian product of every
    /// part graph, LEFTMOST fastest-varying (the same order a left-associative
    /// `+` chain produces), joined into ONE string buffer per combination.
    /// Each hole is already `tostring`ed at lower time. Survives fusion like
    /// `Binary`; contributes zero pushdown prefix (every part reads the whole
    /// input authority).
    Concat {
        /// Template parts in authored left-to-right order. A static template
        /// never reaches this node — it is one literal stage.
        parts: Vec<ProgramNodeId>,
    },
    /// A conditional `if condition then consequent else alternative end`: the
    /// `condition` is a generator; EACH of its outputs selects a branch evaluated
    /// over the re-borrowed ORIGINAL input, truthy (not `false`/`null`) choosing
    /// `consequent` and falsy choosing `alternative`. `elif` chains desugar to
    /// nested `Conditional`s (the next branch is the enclosing `alternative`), and
    /// a missing `else` synthesizes an identity `alternative`. Like `Choice`, it
    /// survives fusion and contributes zero pushdown prefix on the spine (the
    /// condition and both arms read the whole input authority).
    Conditional {
        /// The predicate generator; each output selects an arm over the original
        /// input.
        condition: ProgramNodeId,
        /// The arm evaluated for a truthy condition output.
        consequent: ProgramNodeId,
        /// The arm evaluated for a falsy condition output (identity when the
        /// authored `if` had no `else`).
        alternative: ProgramNodeId,
    },
    /// An alternative `left // right`: the `left` outputs are truthiness-FILTERED
    /// eagerly — each truthy (non-`false`/`null`) output is re-emitted upward as it
    /// arrives (no buffering), each falsy one is swallowed; if the `left` produces
    /// ZERO truthy outputs and completes, `right` is evaluated once over the
    /// re-borrowed ORIGINAL input and its outputs pass through UNFILTERED. An error
    /// in `left` propagates (an error after a truthy output leaves that prefix
    /// standing and never seeds `right`; an error after only-falsy outputs beats
    /// the pending `right`). Like `Choice`, it survives fusion and contributes zero
    /// pushdown prefix on the spine.
    Alternative {
        /// The filtered generator: truthy outputs pass through, falsy are dropped.
        left: ProgramNodeId,
        /// The unfiltered fallback, evaluated once only when `left` yields no
        /// truthy output.
        right: ProgramNodeId,
    },
    /// A short-circuiting logical `left and right` / `left or right`: the `left` is
    /// the outer loop. Per left output, if it short-circuits (`and` + falsy, or
    /// `or` + truthy) the operator emits the boolean of the LEFT's truthiness
    /// without touching `right`; otherwise it evaluates `right` over the re-borrowed
    /// ORIGINAL input and emits one boolean per right output (the RIGHT's
    /// truthiness). Every output is an owned `Value::Bool`. This LEFT-outer drive is
    /// the inverse of `Binary`'s RIGHT-outer Cartesian order, which is why `and`/
    /// `or` are a separate family (see [`LogicalOp`]). Like `Choice`, it survives
    /// fusion and contributes zero pushdown prefix on the spine.
    Logical {
        /// Which short-circuiting operator this node applies.
        operator: LogicalOp,
        /// The outer-loop operand whose truthiness drives short-circuiting.
        left: ProgramNodeId,
        /// The inner operand, evaluated per non-short-circuiting left output.
        right: ProgramNodeId,
    },
    /// A `try body` / `try body catch handler` error barrier: `body`'s outputs pass
    /// THROUGH transparently (a `try` is not a consumer); the FIRST catch-eligible
    /// error `body` raises stops the fan-out and, when `handler` is present, seeds
    /// it with the error VALUE (its outputs REPLACE the remainder) — a catchless
    /// `try` swallows the error (no output). A raised error walks to the nearest
    /// enclosing `Try` barrier; a machine error (`Codec`) tears through. Like
    /// `Choice`, it survives fusion and contributes zero pushdown prefix on the
    /// spine. Term-try (`(expr)?`, `.a??`) lowers to a catchless `Try`.
    Try {
        /// The protected body graph.
        body: ProgramNodeId,
        /// The optional catch handler, seeded with the caught error value; `None`
        /// is a catchless `try` (the error is swallowed).
        handler: Option<ProgramNodeId>,
    },
    /// A FUSION BARRIER around a body shared by every arm of a `?//` chain.
    ///
    /// The chain's law — a raise anywhere in an arm restarts the NEXT
    /// alternative from the beginning of the body — used to copy the body once
    /// per arm, squaring the arena for chains nested in their own body. The
    /// body is now lowered ONCE and every arm's frame ends at
    /// this node; fusion materializes the inner body once (memoized by id) and
    /// every arm references that single materialization, so the arena stays
    /// linear. Execution is a pure indirection: evaluating this node evaluates
    /// its inner body with the current dot, and raises pass through unchanged
    /// to the enclosing `Try` barrier.
    ChainBody {
        /// The shared body graph, evaluated per arm with that arm's bindings.
        body: ProgramNodeId,
    },
    /// The `empty` filter: ZERO outputs, no navigation, no error. It is the
    /// zero-cardinality floor the `reduce`/`foreach` null-state laws are
    /// pinned against, and it contributes zero pushdown prefix on the spine.
    Empty,
    /// A variable binding `SOURCE as $x | BODY`: for EACH value `source`
    /// produces, in order, write it into env slot `slot` and evaluate `body` over
    /// the binder's ORIGINAL input (a binding does NOT change dot). `slot` is
    /// unique to this binder OCCURRENCE, so no save/restore is needed when a
    /// streaming emission runs downstream code while the binder frame is live.
    /// Like `Choice` it survives fusion and contributes zero pushdown prefix.
    Bind {
        /// The generator whose outputs are bound, one body run per output.
        source: ProgramNodeId,
        /// The unique env slot this binder occurrence writes.
        slot: VarSlot,
        /// The body evaluated once per bound value, over the original input.
        body: ProgramNodeId,
        /// TRUE for a PATTERN-FRAME EXTRACTION binder — the `$matched | .[i]`
        /// steps the reference's array/object matchers compile. Path mode
        /// evaluates such a source with the register OPEN, because the
        /// reference's constant-key matcher indexes skip the freeze; every
        /// ordinary bind (the as-var, reduce/foreach sources, computed-key
        /// helpers) stays FROZEN. Only path mode reads this field; the graph
        /// machine's bind law is the same either way.
        frame: bool,
    },
    /// An ENGINE binding `~CONSTRUCTOR as ~x | BODY`: evaluate the
    /// `~generator` constructor graph ONCE, hold the resulting cursor machine in
    /// engine slot `slot`, evaluate `body` once over the binder's ORIGINAL input
    /// (dot unchanged — a pull sees the parent's dot), and release the cursor
    /// when the body completes (scope-bounded RAII lifetime). The cursor is a
    /// separately-owned nested `exec::GraphMachine`, so a `limit`/`first`
    /// cut or a raise in the ENCLOSING program cannot unwind or truncate it; a
    /// re-evaluation of this node replaces the slot's cursor, dropping the old one.
    /// Like `Bind` it survives fusion and contributes zero pushdown prefix.
    EngineBind {
        /// The `~generator` constructor graph ([`ProgramNode::EngineGenerator`]).
        source: ProgramNodeId,
        /// The unique engine-cursor slot this binding occurrence owns.
        slot: EngineSlot,
        /// The body evaluated once with `~x` in scope.
        body: ProgramNodeId,
    },
    /// A pull on the cursor in engine slot `slot`: `~x.next` emits ZERO-OR-ONE
    /// value per evaluation (empty once the cursor is exhausted, and exhaustion
    /// is idempotent), `~x.rest` emits ALL remaining values as a stream. These
    /// are the ONLY projections the engine-binding protocol admits; the bare
    /// binding is rejected at lower time, so this node is the whole value-return
    /// surface of `~x`. The pull is stateful — a `.next` advances the cursor —
    /// so it declines the morsel lane (the morsel gate classifies every
    /// engine-surface node kind as ineligible rather than trusting a registry
    /// record).
    EnginePull {
        /// The cursor slot this pull reads.
        slot: EngineSlot,
        /// Which projection this pull performs.
        kind: EnginePullKind,
    },
    /// The cursor BODY of `~generator(INIT; UPDATE; EXTRACT)`: a
    /// stateful one-pull-per-emission drive owned by a NESTED `GraphMachine`
    /// seeded at this node (never by the enclosing machine — the enclosing
    /// machine only ever evaluates the [`ProgramNode::EngineBind`] that seeds
    /// it). Per pull: `init` ran once at seed, then `update` runs with dot = the
    /// current state (zero outputs exhausts the cursor forever, two or more
    /// raise the typed cardinality error), then `extract` runs with dot = the
    /// new state (one output is the emitted value, zero skips to the next
    /// cycle, two or more raise). An error inside `update`/`extract` propagates
    /// from the `.next` call site with the state left at the last COMPLETED
    /// update.
    EngineGenerator {
        /// The initial-state filter, evaluated once over the seed input.
        init: ProgramNodeId,
        /// The state-update filter, evaluated with dot = the current state.
        update: ProgramNodeId,
        /// The extract filter, evaluated with dot = the new state.
        extract: ProgramNodeId,
    },
    /// The cursor BODY of `~rng($seed)`: a NESTED `GraphMachine`
    /// seeded at this node evaluates the seed graph once (one exact integer)
    /// and then draws one float uniform in `[0, 1)` per pull through the same
    /// xoshiro256** law `rand(seed)` uses, so `~rng(S).next` == `rand(S)` and
    /// later pulls continue the stream. The cursor never completes — a `.rest`
    /// pull takes with `limit`. Like [`ProgramNode::EngineGenerator`], the
    /// ENCLOSING machine never evaluates it; it is referenced only as an
    /// `EngineBind` source.
    EngineRng {
        /// The seed graph: exactly one exact-integer output.
        seed: ProgramNodeId,
    },
    /// A fold `reduce SOURCE as $x (INIT; UPDATE)`: `init` runs over the OUTER
    /// dot; per source output the slot is bound and `update` runs with dot = the
    /// current state, the LAST update output becoming the next state and ZERO
    /// update outputs nulling it; when the source completes, exactly ONE final
    /// state is emitted (an empty source emits the init). A multi-output `init`
    /// runs the whole fold once per init value (the dot-clobber artifact is a
    /// catalogued intentional difference).
    Reduce {
        /// The generator whose outputs the fold consumes.
        source: ProgramNodeId,
        /// The unique env slot this loop occurrence writes.
        slot: VarSlot,
        /// The initial-state generator, evaluated over the outer input.
        init: ProgramNodeId,
        /// The state-update generator, evaluated with dot = the current state.
        update: ProgramNodeId,
        /// The recognized keyed-collect key graph, filled after fusion.
        /// `None` until marked, and `None` when the update is not that shape.
        keyed_collect: Option<ProgramNodeId>,
    },
    /// A streaming fold `foreach SOURCE as $x (INIT; UPDATE[; EXTRACT])`: the
    /// same drive as `Reduce`, except EVERY update output is emitted as it lands
    /// (linearly — the last output is the next state, it does NOT fork) and no
    /// final state is emitted. With an `extract`, each update output is instead
    /// detoured through it with dot = the NEW state and `$x` still in scope, and
    /// every extract output is emitted.
    Foreach {
        /// The generator whose outputs the loop consumes.
        source: ProgramNodeId,
        /// The unique env slot this loop occurrence writes.
        slot: VarSlot,
        /// The initial-state generator, evaluated over the outer input.
        init: ProgramNodeId,
        /// The state-update generator, evaluated with dot = the current state.
        update: ProgramNodeId,
        /// The optional 3-arg extract, evaluated with dot = the new state.
        extract: Option<ProgramNodeId>,
    },
    /// The counted-stream drive `limit`/`skip`/`nth` share: run `source` and pay
    /// one unit of the already-bound count `$n` per item, publishing or
    /// withholding it per [`CountedKind`] and CUTTING the source the moment the
    /// count decides no further item can matter.
    ///
    /// It is the lowering of the `foreach f as $item ($n; . - 1; …)` countdown
    /// those three builtins are defined by, and it reproduces that shape's laws
    /// rather than replacing them: the count is bound OUTSIDE this node (so a
    /// multi-output count still runs the whole drive once per value), the SIGN
    /// gates that turn a negative count into `error("… doesn't support negative
    /// …")` stay in the enclosing `Conditional` (so the refusal keeps its
    /// position and its wording), and the countdown itself is `. - 1` under
    /// the reference's own arithmetic — which is why a non-numeric count raises the
    /// ordinary subtraction type error LAZILY, on the first item, and never at
    /// all over an empty source.
    ///
    /// The cut is this node's whole reason to exist. The `foreach` spelling
    /// needs a `label`/`break` pair, a `Choice`, a `Conditional`, and two
    /// `Binary` comparisons per item to express it; this node truncates its own
    /// frame prefix instead, which is the same unwind the `label` performs and
    /// costs no marker value, no walk, and no per-item arithmetic on the common
    /// exact-integer count.
    Counted {
        /// The generator the countdown consumes.
        source: ProgramNodeId,
        /// The env slot the enclosing binder wrote the count `$n` into.
        count: VarSlot,
        /// Which counted law this node applies.
        kind: CountedKind,
        /// The barrier slot PATH MODE cuts the source with. The executor cuts by
        /// truncating frames and never reads it; path mode has no frame stack of
        /// its own, so it re-uses the ordinary `break` marker and needs a unique
        /// occurrence slot to carry it.
        stop: LabelSlot,
    },
    /// A `label $out | BODY` non-local exit barrier: `body`'s outputs pass
    /// THROUGH transparently, and a matching `break` unwinds to this node and
    /// completes it with no further output — the outputs already emitted STAND.
    ///
    /// The barrier is a `Try`-family sibling by construction: the reference
    /// implements `break` as an ERROR carrying the marker `{"__jq": slot}`, so
    /// an intervening `try` between the `break` and its
    /// `label` CATCHES it (`label $o | try (break $o) catch .` renders
    /// `{"__jq":0}`) while a `try` outside the label does not (the label catches
    /// first). Like `Choice` it survives fusion and contributes zero pushdown
    /// prefix.
    Label {
        /// The occurrence slot this barrier catches; see [`LabelSlot`].
        slot: LabelSlot,
        /// The graph whose outputs pass through this barrier.
        body: ProgramNodeId,
    },
    /// A `break $out`: raises the marker value `{"__jq": slot}`, which the
    /// enclosing [`ProgramNode::Label`] of the same slot catches and swallows.
    ///
    /// Zero outputs of its own. The marker is a real, user-reachable value —
    /// `error({"__jq":0})` IS a break in the reference, and a marker with no matching label
    /// surfaces as an ordinary error — so this node raises rather than signalling
    /// out of band.
    Break {
        /// The occurrence slot of the `label` this exits.
        slot: LabelSlot,
    },
    /// The assignment drive shared by all eight assignment operators: fold the
    /// input document over the paths `paths` names, rewriting one path per step,
    /// and emit exactly ONE value.
    ///
    /// The path generator reads the ORIGINAL input; every rewrite lands in the
    /// evolving accumulator, and an [`ModifyMode::Update`] step reads the value
    /// it updates OUT of that accumulator (so an outer edit is visible to an
    /// inner one). An `Update` step whose update produces NO output records the
    /// path for deletion instead, and the recorded paths are applied through ONE
    /// `delpaths` after the fold — which is what makes `.[] |= empty` empty the
    /// container rather than delete every other element.
    ///
    /// The operator-specific half lives entirely in the LOWERING (see
    /// `compile::lower`): `= op= //=` bind their right-hand side OUTSIDE this
    /// node, so a bound RHS emitting nothing kills the whole expression before a
    /// single path is visited, while `|= empty` reaches the deletion law above.
    Modify {
        /// The path generator, evaluated in path mode over the original input.
        paths: ProgramNodeId,
        /// The graph producing the new value at each path (at most one output —
        /// the lowering caps it with the reference's own `label`/`break` first-output shape).
        update: ProgramNodeId,
        /// Whether the update reads the value it replaces.
        mode: ModifyMode,
    },
    /// The FACT-WRITE assignment (`PATH.@role = RHS` or `PATH.&name = RHS`
    /// under `--edit`): the value path `paths` resolves to LOCATED nodes, each
    /// node's fact `role` payload is replaced (or updated) by `update`, and the
    /// run records one [`crate::exec::FactDelta`] per node BESIDE the unchanged
    /// document — a fact write is a span operation, never a value mutation.
    ///
    /// Reachable only in edit-mode compiles (the CLI passes `--edit` into the
    /// compile). The executor walks the original located document for the span
    /// authority — a preceding value write emits an owned result, so the machine
    /// keeps that document as origin. The edit lane merges the deltas with any
    /// value-diff patches against the same retained source.
    FactAssign {
        /// The value path generator, lowered from the key/index chain before
        /// the accessor step — static Key/Index steps and runtime-resolved
        /// `DynVar` slots (the compile rejects any other path shape).
        paths: ProgramNodeId,
        /// The fact role selector (`"comment"` and its position siblings, or
        /// `"attribute"` for a markup `.&name` write); the executor matches it
        /// against the codec's namespaced roles (`yaml.comment@1` serves it,
        /// `xml.attribute@1` serves the attribute role).
        role: String,
        /// The fact KIND: the attribute selector (`"href"`) for a `.&href`
        /// write, empty for the comment roles. The attribute read/write match
        /// role `"attribute"` AND this kind; the comment roles match on role
        /// alone. Ignored when [`Self::FactAssign::selector`] names an
        /// attribute slot.
        kind: String,
        /// The DYNAMIC selector slot of the write target's LAST step
        /// (`PATH.@(expr) = RHS` / `PATH.&(expr) = RHS`): the bound value is
        /// resolved at run time — for a node accessor a string naming the fact
        /// role (validated against the closed writable vocabulary), for an
        /// attribute accessor (`true`) a string naming the attribute kind.
        /// The lowering binds the selector expression OUTSIDE this node (the
        /// operand-binder law), so the slot always holds the resolved name by
        /// the time the executor reads it. `None` for every static write.
        selector: Option<(VarSlot, bool)>,
        /// The graph producing the new payload at each node (at most one
        /// output, capped exactly as [`ProgramNode::Modify`] caps its update).
        update: ProgramNodeId,
        /// Whether the update reads the payload it replaces.
        mode: ModifyMode,
    },
}

impl ProgramNode {
    /// A framed binary. [`crate::exec::mark_binary_shapes`] may rewrite `shape`.
    pub fn binary(op: BinaryKind, left: ProgramNodeId, right: ProgramNodeId) -> Self {
        Self::Binary {
            op,
            left,
            right,
            shape: BinaryShape::Framed,
        }
    }
}

/// What a [`ProgramNode::Modify`] step does at one path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModifyMode {
    /// `=`: the update ignores the old value (it is the bound right-hand side),
    /// and no step can ever record a deletion — the RHS is bound, so an empty one
    /// never reaches the fold at all.
    Set,
    /// `|=` and every `op=`: the update runs with dot = the value the path
    /// addresses IN THE ACCUMULATOR, and a step with no output records the path
    /// for the deferred deletion.
    Update,
}

/// A compiled program: the node arena, its root, and the derived pushdown split.
///
/// Opaque and private to the crate; callers observe it only through the public
/// [`crate::CompiledProgram`] wrapper. Identity is the root [`ProgramNode::Stage`]
/// with empty `steps`; `root` is always a valid node, so there is no separate
/// root sentinel.
///
/// Invariant: the arena is always in **path-normal form** — [`crate::analysis`]
/// fuses every fusable `FlatMap(Stage, Stage)` before assembly, so no fusable
/// `Stage∘Stage` `FlatMap` survives (a [`StageStart::Literal`] body blocks its
/// own fusion). `root` may address a [`ProgramNode::Stage`] (the pure
/// path/iteration/literal subset), a [`ProgramNode::Choice`] (a top-level comma),
/// a [`ProgramNode::CollectArray`]/[`ProgramNode::ConstructObject`] (a top-level
/// constructor), or a [`ProgramNode::FlatMap`] whose fusion a `Choice`, a
/// constructor, or a literal body blocked.
#[derive(Debug)]
pub struct Program {
    nodes: Vec<ProgramNode>,
    root: ProgramNodeId,
    split: PushdownSplit,
    slots: u32,
    scans: Vec<CorrelatedScan>,
    topk: Vec<PartialSort>,
    anti: Vec<AntiJoinScan>,
}

impl Program {
    /// Assembles a program from a lowered arena, its derived split, and the
    /// binder-occurrence slot count the executor sizes its env vector from.
    ///
    /// The correlated-scan table is derived HERE rather than on demand (the
    /// one fact that still derives on demand, [`Self::projection_class`], is
    /// consulted exactly once per compiled program by the engine's
    /// count-demand derivation at construction — see its doc) because its
    /// reader is the executor's per-node dispatch, not a route selector: a
    /// per-element route seeds one machine per element, and re-deriving a table
    /// per seed would charge every streamed element for a fact that cannot
    /// change between them.
    pub fn new(nodes: Vec<ProgramNode>, root: ProgramNodeId, split: PushdownSplit, slots: u32) -> Self {
        let scans = correlated_scans(&nodes);
        let topk = partial_sorts(&nodes);
        let anti = anti_joins(&nodes);
        Self {
            nodes,
            root,
            split,
            slots,
            scans,
            topk,
            anti,
        }
    }

    /// The correlated `select` scans this program's executor may drive from a
    /// sorted keyed index (see [`crate::analysis::CorrelatedScan`]).
    ///
    /// Empty for every program the closed table declines, which is almost all of
    /// them; the executor's dispatch reads that emptiness before anything else.
    pub fn scans(&self) -> &[CorrelatedScan] {
        &self.scans
    }

    /// The partial-sort rows this program's executor may substitute for a full
    /// `sort`/`sort_by` (see [`crate::analysis::PartialSort`]).
    ///
    /// Empty for every program the closed table declines, which is almost all of
    /// them; the executor's dispatch reads that emptiness before anything else.
    pub fn topk_sorts(&self) -> &[PartialSort] {
        &self.topk
    }

    /// The anti-join `select` calls this program's executor may answer from a
    /// sorted keyed index queried for the complement.
    ///
    /// Empty for every program the closed table declines, which is almost all of
    /// them; the executor's dispatch reads that emptiness before anything else.
    pub fn anti_joins(&self) -> &[AntiJoinScan] {
        &self.anti
    }

    /// The derived codec-pushdown split.
    pub const fn split(&self) -> PushdownSplit {
        self.split
    }

    /// The number of variable binding slots lowering assigned — one per binder
    /// OCCURRENCE. Zero for a binder-free program, whose run never grows the
    /// executor's env vector at all.
    pub const fn slots(&self) -> u32 {
        self.slots
    }

    /// The complete node arena, borrowed for the executor's graph walk.
    pub fn nodes(&self) -> &[ProgramNode] {
        &self.nodes
    }

    /// The derived projection-demand class of one streamed element.
    ///
    /// The class is a compile-time constant of the program, but it is still
    /// derived here on demand rather than stored as a field:
    /// [`crate::analysis::ProjectionClass`] BORROWS this program's arena (its
    /// field names are arena interner strings), so it cannot live in a
    /// self-referential struct. The engine consults it exactly ONCE per
    /// compiled program — the count-demand derivation at construction — and
    /// caches the OWNED demand that consumed it
    /// (`jqf_engine::compile::CompiledProgram::count_demand`), so the walk
    /// never runs inside a per-record loop.
    pub fn projection_class(&self) -> ProjectionClass<'_> {
        // A SOLE named boundary licenses exact per-boundary classification;
        // nested iteration keeps the conservative join over every `.[]`.
        let boundary = self
            .split
            .element_boundary()
            .filter(|boundary| boundary.is_sole())
            .map(|boundary| (boundary.stage(), boundary.stage_prefix_len()));
        classify(&self.nodes, self.root, self.slots, boundary)
    }

    /// The RANGE-locate row this program matches, when it is the bare slice
    /// publish `PATH[a:b]` (the Gate A row).
    pub fn range_locate(&self) -> bool {
        crate::analysis::is_range_locate(self.nodes(), self.root())
    }

    /// The program's NAMED element boundary — the single `.[]` a projected route
    /// would stream — or `None` when the shape names none.
    ///
    /// Unlike the element-iteration row recognizer this is not one:
    /// it reaches inside `Reduce`/`Foreach` sources and through `map`'s lowered
    /// body, shapes the deleted element-stream route never took. Naming a
    /// boundary changes no routing.
    pub const fn element_boundary(&self) -> Option<ElementBoundary> {
        self.split.element_boundary()
    }

    /// The steps of the [`Stage`] at `id`, or an empty slice for any other node.
    ///
    /// The named element boundary addresses stages by id (its container path can
    /// span a `FlatMap` upstream stage and the boundary stage), so the lowering
    /// needs step access that is not tied to the pushdown ENTRY stage.
    ///
    /// [`Stage`]: ProgramNode::Stage
    pub fn stage_steps(&self, id: ProgramNodeId) -> &[StageStep] {
        match &self.nodes[id.index()] {
            ProgramNode::Stage { steps, .. } => steps,
            _ => &[],
        }
    }

    /// The arena's root node id — where execution enters the graph.
    pub const fn root(&self) -> ProgramNodeId {
        self.root
    }

    /// The static forward steps of the pushdown ENTRY stage — the [`Stage`] on
    /// the graph's upstream spine whose leading steps the codec pushdown consumes
    /// ([`crate::analysis::PushdownSplit::prefix_len`] of them).
    ///
    /// It is a real [`Stage`] whenever the prefix is non-empty (a scoped path,
    /// possibly nested inside a `FlatMap` upstream, e.g. `.a[] | (.b, .c)`); the
    /// codec's per-component `?` flag lookup indexes it. When the spine leads to
    /// a `Choice` instead (a whole-document comma), there is no pushed-down
    /// prefix and this is empty — the flag lookup never runs in that case (a
    /// whole-document route yields a resolved root, never a pushed-down mismatch).
    ///
    /// [`Stage`]: ProgramNode::Stage
    pub fn entry_stage_steps(&self) -> &[StageStep] {
        self.stage_steps(self.split.entry())
    }

    /// Whether this program is the MORSEL STATIC PATH class: the shape a record
    /// morsel worker may run.
    ///
    /// Exactly the shape the record-route review pinned as the parallel lane:
    /// the root is a bare
    /// [`StageStart::Current`] [`Stage`] — no `Choice`, `FlatMap`, constructor,
    /// `Call`, binder, or literal producer — whose every step is a STATIC
    /// [`StepAccess::Key`] or [`StepAccess::Index`]. Identity (an empty step
    /// list) qualifies. `.[]`, `..`, a slice, and a dynamic variable step do
    /// not, so the class publishes at most one value per input and iterates
    /// nothing.
    ///
    /// It is an ELIGIBILITY fact, not a correctness one: byte identity between
    /// the serial and the parallel record drives rests on the clean-morsel law
    /// (a morsel that emits any diagnostic yields to serial), which is
    /// program-independent. This predicate exists so the route promises only
    /// the class it verified.
    ///
    /// [`Stage`]: ProgramNode::Stage
    pub fn is_morsel_static_path(&self) -> bool {
        let ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } = &self.nodes[self.root.index()]
        else {
            return false;
        };
        steps
            .iter()
            .all(|step| matches!(step.access(), StepAccess::Key(_) | StepAccess::Index(_)))
    }

    /// Whether this program is SAFE to run once per record on a morsel worker:
    /// no overload in its graph has an observable effect.
    ///
    /// This is the WIDENED record-route eligibility:
    /// the clean-morsel law — a morsel either publishes exactly what serial
    /// would for the same records or reports that it did not — is
    /// program-independent, so the eligibility gate is the program's
    /// per-record OBSERVABILITY, not its shape. A pure program's output depends
    /// only on the one record in front of it, so a worker's bytes for its byte
    /// range are serial's bytes by construction; the input family (`input/0`,
    /// `inputs/0`, `input_filename/0`, `input_line_number/0`) reads the shared
    /// cursor, `stderr/0`/`debug` write to a host channel the worker has none
    /// of, `halt`/`halt_error` terminate the process, and the nondeterministic
    /// builtins (`now`, `uuid_v4`/`v7`, `env`) observe a different world per
    /// worker — all of them are [`jqf_builtins::registry::Effects::Impure`], so the
    /// fence is one closed classification, the same one the registry's
    /// `effects()` lookup serves to every routing gate. A `None` (an id outside
    /// the registry, unreachable by construction) compares unequal to
    /// [`jqf_builtins::registry::Effects::Pure`] and so declines — the
    /// fail-closed direction.
    ///
    /// The walk covers EVERY arena node, `CallDef` bodies included: a recursive
    /// definition's body is compiled into the same arena, and a call inside a
    /// definition is a `Call` node like any other. The engine surface (`~x`
    /// bindings and pulls) is stateful and declines too, by node kind rather
    /// than by a registry record.
    ///
    /// The match is EXHAUSTIVE on purpose, with no wildcard arm: a new node
    /// kind must be classified here before it compiles, so a future stateful
    /// family can never default to parallel-eligible (the fail-open shape).
    /// Every node not named above is structurally pure — its observable
    /// behavior lives entirely in the arena nodes it references (calls and
    /// the engine surface), which this same walk visits.
    pub fn is_morsel_eligible(&self) -> bool {
        self.nodes.iter().all(|node| match node {
            ProgramNode::Call { overload, .. } => {
                jqf_builtins::registry::effects(overload.get()) == Some(jqf_builtins::registry::Effects::Pure)
            }
            // The engine surface is stateful: a pull advances its cursor and a
            // bind seeds it, and the gate cannot structurally prove a cursor
            // body's per-run determinism (it is arbitrary user code over runtime
            // state) — the fail-closed direction.
            ProgramNode::EngineBind { .. }
            | ProgramNode::EnginePull { .. }
            | ProgramNode::EngineGenerator { .. }
            | ProgramNode::EngineRng { .. } => false,
            ProgramNode::Stage { .. }
            | ProgramNode::FlatMap { .. }
            | ProgramNode::Choice { .. }
            | ProgramNode::CollectArray { .. }
            | ProgramNode::CountCollect { .. }
            | ProgramNode::ConstructObject { .. }
            | ProgramNode::CallDef { .. }
            | ProgramNode::CallFilter { .. }
            | ProgramNode::Binary { .. }
            | ProgramNode::Concat { .. }
            | ProgramNode::Conditional { .. }
            | ProgramNode::Alternative { .. }
            | ProgramNode::Logical { .. }
            | ProgramNode::Try { .. }
            | ProgramNode::ChainBody { .. }
            | ProgramNode::Empty
            | ProgramNode::Bind { .. }
            | ProgramNode::Reduce { .. }
            | ProgramNode::Foreach { .. }
            | ProgramNode::Counted { .. }
            | ProgramNode::Label { .. }
            | ProgramNode::Break { .. }
            | ProgramNode::Modify { .. }
            | ProgramNode::FactAssign { .. } => true,
        })
    }

    /// Whether this program is the BARE IDENTITY filter: a root `Stage` that
    /// starts at `Current` and has zero steps.
    ///
    /// This is deliberately narrower than [`Self::is_morsel_static_path`],
    /// which admits any static Key/Index chain. The source-preserving
    /// round-trip lane publishes the input bytes UNCHANGED, so only a program
    /// that provably emits its input once, value-for-value, for every input may
    /// take it; `.a` re-derives a value, `.[]` fans out, and every other shape
    /// is a different value or cardinality.
    pub fn is_identity(&self) -> bool {
        let ProgramNode::Stage {
            start: StageStart::Current,
            steps,
        } = &self.nodes[self.root.index()]
        else {
            return false;
        };
        steps.is_empty()
    }

    /// Whether the program contains any assignment node.
    ///
    /// The edit lane's document-subject law keys on this: a program with no
    /// assignment leaves the document unchanged (the output subject is the
    /// retained input bytes), while a program that assigns publishes the
    /// assignment's modified document.
    pub fn modifies(&self) -> bool {
        self.nodes.iter().any(|node| match node {
            ProgramNode::Modify { .. } => true,
            // `del`, `delpaths`, and `setpath` are the path-WRITE evaluators:
            // they lower to builtin calls rather than `Modify` nodes, but their
            // single output IS the modified document, exactly like an
            // assignment's. `map_values` is the same law from the other side:
            // it is a NATIVE evaluator call whose output is a rebuilt document,
            // never the retained input bytes, so the edit lane's subject law
            // must treat it as modifying too.
            ProgramNode::Call { overload, .. } => matches!(
                overload.get(),
                jqf_builtins::registry::builtins::id::SETPATH
                    | jqf_builtins::registry::builtins::id::DELPATHS
                    | jqf_builtins::registry::builtins::id::DEL
                    | jqf_builtins::registry::builtins::id::MAP_VALUES
            ),
            _ => false,
        })
    }
}

impl ProgramNode {
    /// A resolved Evaluator call. The payload is resolved here so runtime
    /// dispatch is a load, not a name/arity lookup.
    pub(crate) fn call(overload: BuiltinOverloadId, revision: SemanticRevision, args: Vec<ProgramNodeId>) -> Self {
        let payload = match dispatch(overload) {
            Some(BuiltinDispatch::Evaluator(payload)) => payload,
            Some(BuiltinDispatch::Lowering(_)) | None => {
                // A Lowering builtin expands at lower time (`lower_call`
                // guards every source-driven site) and an unregistered id
                // cannot be minted, so reaching this arm is a registry or
                // compiler contract violation — never user input. The stored
                // node must stay total, but it must never EXECUTE as the
                // `error/0` payload this fallback names: debug builds refuse
                // at the mint site instead of raising silently at run time.
                debug_assert!(
                    false,
                    "ProgramNode::call minted for a non-Evaluator overload; \
                     Lowering builtins expand at lower time and every id is \
                     registered"
                );
                Evaluator::ErrorZero
            }
        };
        Self::Call {
            overload,
            revision,
            args,
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SliceBound, SliceBounds, StageStep, StepAccess};
    use alloc::boxed::Box;
    use alloc::string::String;
    use jqf_data::Value;

    fn literal(spelling: &str) -> SliceBound {
        SliceBound::Literal(Value::Number(
            jqf_data::Number::try_json_literal(spelling).expect("literal"),
        ))
    }

    fn normalized(start: SliceBound, end: SliceBound) -> Option<(Option<i64>, Option<i64>)> {
        SliceBounds { start, end }.try_normalize()
    }

    #[test]
    fn the_boundary_law_floors_the_start_and_ceils_the_end() {
        // The rounding directions are the runtime resolver's own, hoisted to
        // lower time: `.[1.7:3.2]` is the half-open `[1, 4)`, the reference's
        // three elements at positions 1, 2, 3.
        assert_eq!(normalized(literal("1.7"), literal("3.2")), Some((Some(1), Some(4))));
        assert_eq!(
            normalized(literal("2"), literal("2")),
            Some((Some(2), Some(2))),
            "a degenerate range normalizes; the emptiness is the resolver's clamp"
        );
        assert_eq!(normalized(SliceBound::Open, SliceBound::Open), Some((None, None)));
        // An authored `null` bound is the container's own edge at runtime.
        assert_eq!(
            normalized(SliceBound::Literal(Value::Null), literal("2")),
            Some((None, Some(2)))
        );
    }

    #[test]
    fn a_strictly_negative_end_that_ceils_to_zero_is_open_not_zero() {
        // The one spelling no signed integer can carry, and the whole reason the
        // authored form stays the executor's authority: `.[:-0.5]` takes the
        // WHOLE array (it wraps to `len - 0.5` and ceils to `len`) while
        // `.[:-0.0]` takes NONE (`-0.0` is not strictly negative).
        assert_eq!(
            normalized(SliceBound::Open, literal("-0.5")),
            Some((None, None)),
            "-0.5 as an end is the container's own end"
        );
        assert_eq!(
            normalized(SliceBound::Open, literal("-0.0")),
            Some((None, Some(0))),
            "-0.0 as an end is position zero"
        );
    }

    #[test]
    fn the_declines_are_the_bounds_no_range_footprint_can_carry() {
        // A strictly-negative resolved bound normalizes to its SIGNED value:
        // the ArrayRange owns the len-relative reading and the
        // codec's two-pass arm resolves it against the observed length. A
        // non-numeric literal and a bound by variable decline permanently:
        // their outcome is runtime-input dependent, which is not a range at all.
        assert_eq!(normalized(literal("-2"), SliceBound::Open), Some((Some(-2), None)));
        assert_eq!(normalized(SliceBound::Open, literal("-1")), Some((None, Some(-1))));
        assert_eq!(
            normalized(SliceBound::Literal(Value::Bool(true)), SliceBound::Open),
            None
        );
        assert_eq!(normalized(SliceBound::Var(0), SliceBound::Open), None);
        // A magnitude past `i64` SATURATES rather than declining: every bound
        // past the container's own length clamps to an edge anyway.
        assert_eq!(
            normalized(literal("0"), literal("1e30")),
            Some((Some(0), Some(i64::MAX)))
        );
    }

    #[test]
    fn a_slice_step_does_not_widen_the_step_of_every_other_program() {
        // Two `Value`-carrying bounds inline would roughly DOUBLE
        // `size_of::<StageStep>` for EVERY program's step vector, so the bound
        // pair is boxed. The pin is that a `Slice` step is no wider than the
        // widest pre-existing access (a decoded `Key`), which this asserts
        // structurally rather than against a hard-coded byte count.
        let key = core::mem::size_of_val(&StepAccess::Key(String::new()));
        let slice = core::mem::size_of_val(&StepAccess::Slice(Box::new(SliceBounds {
            start: SliceBound::Literal(Value::Null),
            end: SliceBound::Open,
        })));
        assert_eq!(slice, key);
        assert_eq!(
            core::mem::size_of::<StageStep>(),
            core::mem::size_of::<StepAccess>() + core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn descend_and_each_carry_no_payload_at_all() {
        let each = core::mem::size_of_val(&StepAccess::Each);
        assert_eq!(core::mem::size_of_val(&StepAccess::Descend), each);
    }
}
