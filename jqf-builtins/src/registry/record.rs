//! The builtin catalog record shape and its closed vocabulary.
//!
//! One job: define the const-constructible family and overload records the registry stores, plus the small closed enums
//! and compact id newtypes those records are built from. These records are pure data — the pinned home of a builtin's
//! prose docs ([`BuiltinFamilyRecord::summary`]/`detail`), its executable [`BuiltinExample`]s, and the identity fields
//! the crate-private dispatch and the demand-projection classifier read (id, family, execution, demand transfer,
//! effects). The prose docs feed the future declaration compiler, generated reference docs, CLI help, and SDK
//! introspection; the executable examples feed the `builtin_examples` harness today.
//!
//! Negative space: it holds no inventory (that is [`super`]'s concatenated slices), runs no validation (that is
//! [`super::validate`]), resolves no name, and executes nothing (that is [`super::dispatch`]). It carries exactly the
//! foundation field subset pinned in the design doc's D4; the deferred resolved-record fields (errors, state
//! requirements, host requirements, optimization metadata, `JqRelation`) land with the mass-import/manifest vertical,
//! not here.

/// Compact stable identifier of one builtin overload.
///
/// Executable plans store this id, never the source name or a user-visible `@1` spelling. It is stable across
/// documentation changes; a meaning that cannot stay compatible receives a new id (see [`SemanticRevision`] for
/// compatible semantic changes).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinOverloadId(u16);

impl BuiltinOverloadId {
    /// Wraps a raw overload id.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// The raw overload id.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Compact stable identifier of one builtin family.
///
/// A family groups overloads that share a canonical name (for example `log/0` and `log/1,2`). It is derived from the
/// canonical name, not from whichever inventory contributed an overload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinFamilyId(u16);

impl BuiltinFamilyId {
    /// Wraps a raw family id.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// The raw family id.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Monotonic revision of one overload's pinned semantics.
///
/// A compatible semantic change (algorithm, data revision, rounding law) increments this while the
/// [`BuiltinOverloadId`] stays fixed; an incompatible change takes a new overload id instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticRevision(u16);

impl SemanticRevision {
    /// Wraps a raw semantic revision.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// The raw semantic revision.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// The two jqf function parameter kinds.
///
/// Mirrors the historical `FunctionParamKind::{Value, Filter}` split: a value parameter receives one evaluated argument
/// value, a filter parameter receives an unevaluated argument graph the callee drives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParameterKind {
    /// One evaluated argument value.
    Value,
    /// An unevaluated argument filter the callee applies.
    Filter,
}

/// How a resolved call runs, independent of how its implementation is sourced.
///
/// This is the classification only; the executing payload (definition graph, evaluator handle, lowering, operator)
/// lives in the crate-private [`super::dispatch`] table, keyed by the same id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuiltinExecution {
    /// A standard-library definition composed from other overloads.
    Definition,
    /// A native evaluator the executor dispatches by id.
    Evaluator,
    /// A call the compiler lowers into other program nodes.
    Lowering,
    /// A core operator surfaced through the registry.
    Operator,
}

/// Coarse observable-effect class of one overload.
///
/// The foundation distinguishes only pure value production from anything with an observable effect. Finer effect
/// structure (document reads and mutations, diagnostics) attaches with the deferred state and host requirement fields
/// in the builtin-registry vertical; it is intentionally not modelled here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effects {
    /// The output depends only on the inputs, with no observable side effect.
    Pure,
    /// The overload has some observable effect beyond its value output.
    Impure,
}

/// How demand propagates BACKWARD through one builtin overload: the closed combinator vocabulary of the
/// demand-projection classifier's transfer table.
///
/// A variant is a COMBINATOR the classifier interprets (see `crate::analysis::projection`'s `call_demand`), never a
/// constant class and never an open function: the classifier owns the lattice arithmetic, the record owns only which
/// combinator applies. That is what keeps a wrong declaration a bounded, reviewable risk instead of arbitrary code in a
/// data table.
///
/// The field is REQUIRED, so a registered overload without a declaration cannot be written; the classifier is therefore
/// total over the CALL vocabulary by construction. The totality law is Call-scoped on purpose: the transfers that live
/// on program NODES (`Binary`'s `+`-fold, `Logical`, `Conditional`, `Try`, the constructors, `Reduce`/`Foreach` state)
/// have no overload id to hang a record on, and stay guarded by the classifier's no-wildcard exhaustive match over the
/// engine's `ProgramNode` arena.
///
/// Declaring a coarser transfer than an overload deserves is always SOUND (it demands more of the document than
/// needed); declaring a finer one is the new risk class, which is why every declaration is checked by the sdk-smoke
/// `demand_transfer_registry` receipt and cross-checked by the equivalence classes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DemandTransfer {
    /// The conservative default: the call consumes its whole input, whatever its output is demanded for. Every overload
    /// whose demand law is not yet pinned declares this — and `error`, which captures its input as the raised value
    /// under the output rule.
    Subtree,
    /// The call's input demand IS its output demand: the demand passes through unchanged (`not`).
    ///
    /// Sound because the overload is TOTAL on every value (it never raises, so no error path can observe an unprojected
    /// payload) and its output is a fresh boolean carrying no part of the input: when the output is demanded
    /// payload-free the input's truthiness is observationally irrelevant, and when it is demanded whole the
    /// pass-through hands the input the same payload demand.
    InputPassThrough,
    /// A count over a CONSTRUCTED container's boundaries (`length`): the constructor already knows how many elements it
    /// produced, so no element payload is needed. On a DOCUMENT value the same overload falls back to the conservative
    /// default, because a codepoint count or a numeric magnitude reads the payload.
    CountOfConstructedInput,
    /// The input passes THROUGH when a predicate holds, so the call's demand is the union of its output demand and
    /// every argument graph's full demand (`select`).
    ConditionUnionPassThrough,
    /// The overload declares no transfer of its own because no `Call` survives to the classifier:
    /// [`BuiltinExecution::Lowering`] rewrites it into other program nodes at lower time, and those nodes' own arms
    /// carry the demand (`map`, which lowers to `[.[] | f]`).
    ///
    /// The registry's const validation pins the biconditional `execution == Lowering ⇔ transfer == ViaLowering`.
    ViaLowering,
}

/// One executable documentation example for a builtin overload.
///
/// The examples-as-tests harness compiles [`Self::program`], runs it against [`Self::input`] through the real SDK
/// pipeline, and asserts the published bytes equal [`Self::expected`]. This is why docs cannot rot: a wrong example
/// fails the build's test battery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BuiltinExample {
    /// The jqf program text to compile and run.
    pub program: &'static str,
    /// The input document text (JSON, in this codec) the program runs against.
    pub input: &'static str,
    /// The exact bytes the JSON facade publishes, including the trailing newline the facade frames each emitted item
    /// with.
    pub expected: &'static str,
}

/// One family of builtin overloads: the pinned home of its prose docs.
///
/// Const-constructible pure data. Summary is load-bearing: an empty summary fails the const validation in
/// [`super::validate`], so a family cannot be registered without documentation.
#[derive(Clone, Copy, Debug)]
pub struct BuiltinFamilyRecord {
    /// The family's compact stable id.
    pub id: BuiltinFamilyId,
    /// The family's canonical name (for example `log`).
    pub canonical_name: &'static str,
    /// The family's documentation grouping. A stable grouping label the declaration manifest canonicalizes; dispatch
    /// never consults it.
    pub category: &'static str,
    /// One-line summary. Required: an empty summary fails const validation.
    pub summary: &'static str,
    /// Extended prose detail; may be empty when the summary suffices.
    pub detail: &'static str,
}

/// One builtin overload: the resolved dispatch identity and its docs.
///
/// Const-constructible pure data carrying exactly the foundation field subset.
/// Examples are load-bearing: an empty examples slice fails the const validation in [`super::validate`], so an overload
/// cannot be registered without executable documentation. [`Self::demand_transfer`] is load-bearing the same way, one
/// step further: it is a REQUIRED field, so its absence is not even expressible, and its cross-field agreement with
/// [`Self::execution`] is asserted in the same const validation.
#[derive(Clone, Copy, Debug)]
pub struct BuiltinOverloadRecord {
    /// The overload's compact stable id, stored by executable plans.
    pub id: BuiltinOverloadId,
    /// The family this overload belongs to.
    pub family: BuiltinFamilyId,
    /// The overload's canonical name; `(name, arity)` resolves to [`Self::id`].
    pub canonical_name: &'static str,
    /// The overload's exact arity.
    pub arity: u8,
    /// The ordered parameter kinds; its length matches [`Self::arity`].
    pub parameters: &'static [ParameterKind],
    /// How the resolved call runs.
    pub execution: BuiltinExecution,
    /// How demand propagates backward through this overload. Required: the classifier reads this instead of matching
    /// names, so an overload cannot be registered without declaring its transfer.
    pub demand_transfer: DemandTransfer,
    /// The pinned semantic revision.
    pub semantic_revision: SemanticRevision,
    /// The overload's coarse effect class.
    pub effects: Effects,
    /// Executable documentation examples. Required: an empty slice fails const validation.
    pub examples: &'static [BuiltinExample],
}
