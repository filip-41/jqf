//! The engine's public error vocabulary, extracted from `exec` so the future builtins crate can own it without
//! re-exporting engine internals.
//!
//! One job: the poll-time failure channel ([`EngineRunError`]) plus the two arithmetic failure classes
//! ([`ArithFailure`], [`ArithMismatchOp`]) and the parity message renderer ([`message`]). These are pure data over
//! [`CodecError`] and [`Value`]; nothing here drives execution or owns machine state.

use alloc::string::String;

use jqf_codec_core::{CodecError, CodecFailureKind};
use jqf_data::{Value, ValueKind};
use jqf_resource::ResourceContext;

use crate::semantics::binary;
use crate::semantics::binary::BinaryKind;

pub mod message;
pub mod mismatch;

/// The public poll-time failure channel of an engine result stream.
///
/// A residual stream can fail mid-iteration with a semantic mismatch whose typed fields must reach the SDK's own
/// failure surface — they cannot travel through [`CodecError`], which has no path-mismatch vocabulary. Machine
/// failures collapse into [`EngineRunError::Codec`]; the two mismatch classes stay distinct end to end (index vs
/// iterate).
#[derive(Debug)]
pub enum EngineRunError {
    /// A machine failure (control, ledger, or internal contract).
    Codec(CodecError),
    /// A program-raised error VALUE (`error/0` raises the current input, `error/1` raises its argument's first output).
    /// It carries the arbitrary owned value a `catch` handler receives, or the CLI renders on an uncaught raise (a
    /// string printed as-is, a non-string as `(not a string): <json>`).
    /// Catch-eligible: it routes to the nearest enclosing `try` barrier.
    Raised(Value),
    /// `halt`/`halt_error` terminated the whole run: `halt` carries no message and status 0; `halt_error(n)` carries
    /// the current input as its message and the (truncated) argument as its status. Deliberately NOT catch-eligible and
    /// NOT suppressed by `?`: `halt` is process level, so `try halt catch …` still terminates the run.
    Halt {
        /// The process exit status `halt` asked for.
        status: u32,
        /// The current input, which `halt_error` prints compact to stderr before exiting; `halt` has none.
        message: Option<Value>,
    },
    /// A `Key`/`Index` step addressed the wrong non-null type. `step_index` is global (pushdown-prefix-relative indices
    /// already are global). `key` is the offending accessor pre-rendered in the typed form (`string ("a")` / `number
    /// (0)`) for the catch-visible message.
    TypeMismatch {
        /// Zero-based global failing path step.
        step_index: usize,
        /// Payload-transparent type observed at that step.
        actual_type: ValueKind,
        /// The offending accessor, rendered (`string ("a")` / `number (0)`).
        key: String,
        /// A markup accessor hint, appended to the rendered message: when a member step over a markup element array
        /// misses (`Cannot index array with string ("price")`) but the name exists as the element's own name or one of
        /// its attributes, the hint names the correct accessor (`.@name` / `.&price`). `None` on every non-markup
        /// mismatch — the message stays byte-identical.
        hint: Option<String>,
    },
    /// An `.[]` step iterated a non-iterable value: the DISTINCT iterate class (the "Cannot iterate over X (v)" versus
    /// "Cannot index X with Y" classes). Global step. `operand` is the value's bounded compact JSON for the message.
    IterateMismatch {
        /// Zero-based global failing iterate step.
        step_index: usize,
        /// Payload-transparent type observed at that step.
        actual_type: ValueKind,
        /// The non-iterable value's bounded compact JSON.
        operand: String,
    },
    /// An object construction dynamic key produced a non-string (or a tagged string, which is not silently unwrapped):
    /// the THIRD mismatch class (the "Cannot use number (5) as object key" refusal). It is discovered mid-construction,
    /// so — like a fan-out mismatch — it surfaces as a later-poll `Err` after any already-emitted objects were
    /// published.
    ObjectKeyMismatch {
        /// Payload-transparent type observed for the offending key.
        actual_type: ValueKind,
        /// The offending key value's bounded compact JSON.
        operand: String,
    },
    /// `length` over a value with no length (a boolean, or a non-JSON kind):
    /// renders "<type> (<value>) has no length". A builtin-domain error with no path step, surfaced mid-execution like
    /// [`Self::ObjectKeyMismatch`].
    NoLength {
        /// Payload-transparent type observed for the value.
        actual_type: ValueKind,
        /// The value's bounded compact JSON.
        operand: String,
    },
    /// `keys` over a value with no keys (null, a boolean, a number, or a string):
    /// renders "<type> (<value>) has no keys". A builtin-domain error with no path step, surfaced mid-execution.
    NoKeys {
        /// Payload-transparent type observed for the value.
        actual_type: ValueKind,
        /// The value's bounded compact JSON.
        operand: String,
    },
    /// A `.[a:b]` slice over an ARRAY or STRING carried a bound that is neither a number nor `null`: the dedicated
    /// slice-bound class, whose message is a bare constant with no operand at all. It is distinct from
    /// [`Self::TypeMismatch`], which is what a slice over a NON-sliceable input raises — the input's type is
    /// dispatched first, so `{"a":1} | .["a":2]` is the index class and `[1] | .["a":2]` is this one. Suppressed by the
    /// slice step's own `?`.
    SliceIndices,
    /// A mismatch cell raised under the strict dial: the request's `MismatchPolicy::Strict` turned a site where the
    /// floor answers a VALUE into a raise. Catch-eligible like the other typed semantic arms — but the suppression
    /// law means a cell never fires inside a `try` body, so a `try` around the site still answers exactly what the
    /// floor answers. The cell identity is the frozen table's row index; the cell's NAME is the registry payload.
    MismatchRaised {
        /// The frozen table's row index.
        cell: u16,
    },
    /// A `~generator`'s `init`/`update`/`extract`, or a `~rng`'s `seed`, emitted TWO OR MORE values on one pull: one
    /// state, one emission, per pull. The cardinality is not statically knowable, so this is a runtime typed class
    /// raised on the pull that observes the excess — catch-eligible like the other semantic arms, so `try ~x.next
    /// catch .` sees it.
    EngineCardinality {
        /// The constructor that owns the excess (`"generator"` or `"rng"`), rendered into the message.
        constructor: &'static str,
        /// Which filter emitted the excess (`"init"`, `"update"`, `"extract"`, or `"seed"`), rendered into the message.
        phase: &'static str,
    },
    /// A binary operator (`+ - * / %`) failed on its operands: an operand-type mismatch, a zero divisor, or a
    /// binary64-path numeric-range overflow. A builtin-domain-class error with no path step, surfaced mid-execution
    /// like [`Self::ObjectKeyMismatch`] (the ordered prefix stands, the failing pair aborts the value). `left`/`right`
    /// are the operands' bounded compact JSON.
    Arithmetic {
        /// The exact arithmetic failure class (operator + operand kinds).
        failure: ArithFailure,
        /// The left operand's bounded compact JSON.
        left: String,
        /// The right operand's bounded compact JSON.
        right: String,
    },
}

/// The exact class of a binary-arithmetic failure, carried through the pipeline to the facade for parity error
/// rendering.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithFailure {
    /// An operand-type mismatch for `+ - * / %` (the "X and Y cannot be added/subtracted/multiplied/divided" family,
    /// plus `%`'s own "cannot be divided (remainder)").
    TypeMismatch {
        /// Which operator's mismatch template applies.
        op: ArithMismatchOp,
        /// Payload-transparent category of the left operand.
        left: ValueKind,
        /// Payload-transparent category of the right operand.
        right: ValueKind,
    },
    /// `/` by a zero divisor (the "... cannot be divided because the divisor is zero" refusal).
    DivideByZero,
    /// `%` whose divisor truncates to zero (the "... cannot be divided (remainder) because the divisor is zero"
    /// refusal).
    RemainderByZero,
    /// A result left the representable range on a DEFENSIVE exact-path arm in [`crate::semantics::arith`] (a scale gap
    /// past `usize`, a canonical decimal that fails to normalize). The binary64 path never raises it: a non-finite
    /// float result is a value like any other, rendered the reference way.
    NumericRange,
}

/// The operator whose error template a [`ArithFailure::TypeMismatch`] names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArithMismatchOp {
    /// "cannot be added"
    Add,
    /// "cannot be subtracted"
    Subtract,
    /// "cannot be multiplied"
    Multiply,
    /// "cannot be divided"
    Divide,
    /// "cannot be divided (remainder)" — `%`'s own template.
    Modulo,
}

impl ArithFailure {
    /// Maps a value-level binary error onto the pipeline-facing failure class, or onto the machine allocation-failure
    /// channel.
    pub fn from_binary(
        error: crate::semantics::binary::BinaryError,
        resources: &ResourceContext<'_>,
    ) -> Result<Self, EngineRunError> {
        use crate::semantics::binary::{BinaryError, MismatchOp};
        Ok(match error {
            BinaryError::TypeMismatch { op, left, right } => Self::TypeMismatch {
                op: match op {
                    MismatchOp::Add => ArithMismatchOp::Add,
                    MismatchOp::Subtract => ArithMismatchOp::Subtract,
                    MismatchOp::Multiply => ArithMismatchOp::Multiply,
                    MismatchOp::Divide => ArithMismatchOp::Divide,
                    MismatchOp::Modulo => ArithMismatchOp::Modulo,
                },
                left,
                right,
            },
            BinaryError::DivideByZero => Self::DivideByZero,
            BinaryError::RemainderByZero => Self::RemainderByZero,
            BinaryError::NumericRange => Self::NumericRange,
            // A depth cap is a RAISED value, not an arithmetic class: the operator already chose its spelling, so this
            // channel just carries the string out as `error("...")` would have.
            BinaryError::TooDeep(text) => {
                return Err(crate::semantics::path::raise(text, resources));
            }
            // A repeat that would exceed the output ceiling is likewise a RAISED value with the sentence:
            // catch-eligible, and the text a `try (… * n) catch .` handler receives.
            BinaryError::RepeatTooLong => {
                return Err(crate::semantics::path::raise(
                    "Repeat string result too long",
                    resources,
                ));
            }
            BinaryError::Allocation => return Err(EngineRunError::allocation_failure()),
            // A strict-dial cell raise rebuilds the semantic EngineRunError from the frozen row index: it must surface
            // at the operator's position, catchable exactly as the typed semantic arms are.
            BinaryError::MismatchRaised(cell) => {
                return Err(EngineRunError::MismatchRaised { cell });
            }
        })
    }
}

/// Applies one binary operator to two OWNED operands, mapping a failure onto the pipeline-facing arithmetic class with
/// its rendered operands.
///
/// The `Binary` node's own consumer does these three steps inline over the frame that holds its right operand; this is
/// the same law for a caller that already owns both. One caller today: `flatten($depth)`'s `$x - 1`, which is the
/// subtraction reached from inside an evaluator and must raise the message.
pub fn apply_binary(
    op: BinaryKind,
    left: &Value,
    right: &Value,
    resources: &ResourceContext<'_>,
) -> Result<Value, EngineRunError> {
    match binary::apply(op, left, right, resources) {
        Ok(value) => Ok(value),
        Err(error) => {
            let failure = ArithFailure::from_binary(error, resources)?;
            Err(EngineRunError::Arithmetic {
                failure,
                left: message::dump_trunc_owned(left)?,
                right: message::dump_trunc_owned(right)?,
            })
        }
    }
}

impl EngineRunError {
    /// The machine allocation-failure error, for the crate-private builtin evaluators whose owned builders are not
    /// themselves ledger-charged.
    pub fn allocation_failure() -> Self {
        EngineRunError::Codec(CodecError::new(CodecFailureKind::AllocationFailure))
    }

    /// A broken-internal-contract error naming the violated invariant.
    pub fn internal_contract(contract: &'static str) -> Self {
        EngineRunError::Codec(CodecError::new(CodecFailureKind::InternalContractViolation {
            contract,
        }))
    }

    /// Whether a `try` barrier may catch this error.
    ///
    /// Catch-eligible = a program `Raised(value)` plus every semantic typed arm (index / iterate / object-key /
    /// no-length / no-keys / arithmetic).
    /// [`Self::Codec`] (a machine class: cancellation, deadline, ledger rejection, internal contract) tears THROUGH
    /// catch barriers — a catchable resource error would let programs observe allocation failure.
    /// [`Self::MismatchRaised`] is likewise not catchable: intent suppression keeps a cell from firing inside a barrier
    /// at all, so there is never a barrier to tear through and no message to materialize.
    pub const fn is_catch_eligible(&self) -> bool {
        !matches!(self, Self::Codec(_) | Self::Halt { .. } | Self::MismatchRaised { .. })
    }

    /// The exact message text for a TYPED runtime class — the text a `catch` handler receives and an uncaught
    /// occurrence prints after the frame.
    ///
    /// `Ok(None)` for the three channels that carry no class message: the machine channel (no class message), a
    /// program-raised VALUE (whose uncaught rendering is [`crate::raised_body`] plus [`crate::raised_frame_note`]
    /// instead), and the strict mismatch cell (never fires inside a barrier, so never materializes — see
    /// [`Self::is_catch_eligible`]).
    ///
    /// # Errors
    ///
    /// Returns [`Self::Codec`] (allocation failure) when reserving the message buffer fails.
    pub fn typed_message(&self) -> Result<Option<String>, Self> {
        let text = match self {
            Self::Codec(_) | Self::Raised(_) | Self::Halt { .. } | Self::MismatchRaised { .. } => {
                return Ok(None);
            }
            Self::EngineCardinality { constructor, phase } => message::engine_cardinality_message(constructor, phase),
            Self::TypeMismatch {
                actual_type, key, hint, ..
            } => {
                let base = message::index_message(*actual_type, key)?;
                match hint {
                    Some(hint) => {
                        let joined = alloc::format!("{base} — hint: {hint}");
                        Ok(joined)
                    }
                    None => Ok(base),
                }
            }
            Self::IterateMismatch {
                actual_type, operand, ..
            } => message::iterate_message(*actual_type, operand),
            Self::ObjectKeyMismatch { actual_type, operand } => message::object_key_message(*actual_type, operand),
            Self::NoLength { actual_type, operand } => message::no_length_message(*actual_type, operand),
            Self::NoKeys { actual_type, operand } => message::no_keys_message(*actual_type, operand),
            Self::SliceIndices => message::slice_indices_message(),
            Self::Arithmetic { failure, left, right } => message::arith_message(*failure, left, right),
        };
        text.map(Some)
    }

    /// The registry code for this failure family — the binding contract's spine (the generated diagnostic-code
    /// registry in jqf-resource). Stable per family: a changed meaning is a new code, never a repurposed id.
    #[must_use]
    pub fn diagnostic_code(&self) -> u16 {
        use jqf_resource::diag::codes;
        match self {
            Self::MismatchRaised { .. } => codes::MISMATCH_STRICT,
            Self::EngineCardinality { .. } => codes::RAISE_ENGINE_CARDINALITY,
            Self::TypeMismatch { .. } => codes::RAISE_INDEX,
            Self::IterateMismatch { .. } => codes::RAISE_ITERATE,
            Self::ObjectKeyMismatch { .. } => codes::RAISE_OBJECT_KEY,
            Self::NoLength { .. } => codes::RAISE_NO_LENGTH,
            Self::NoKeys { .. } => codes::RAISE_NO_KEYS,
            Self::SliceIndices => codes::RAISE_SLICE_INDICES,
            Self::Arithmetic { failure, .. } => match failure {
                ArithFailure::TypeMismatch { .. } => codes::RAISE_ARITHMETIC,
                ArithFailure::DivideByZero => codes::RAISE_DIVIDE_BY_ZERO,
                ArithFailure::RemainderByZero => codes::RAISE_REMAINDER_BY_ZERO,
                ArithFailure::NumericRange => codes::RAISE_NUMERIC_RANGE,
            },
            Self::Codec(error) => match error.kind() {
                CodecFailureKind::InvalidInput => codes::MACHINE_INPUT,
                CodecFailureKind::UnsupportedRepresentation => codes::MACHINE_REPRESENTATION,
                CodecFailureKind::RequirementMismatch => codes::MACHINE_REQUIREMENT,
                CodecFailureKind::ProviderRouteMismatch => codes::MACHINE_ROUTE_MISMATCH,
                CodecFailureKind::InvalidTag => codes::MACHINE_INVALID_TAG,
                CodecFailureKind::CollidingTags => codes::MACHINE_COLLIDING_TAGS,
                CodecFailureKind::Resource(_) => codes::MACHINE_RESOURCE,
                CodecFailureKind::Control(jqf_resource::ControlError::Cancelled) => codes::MACHINE_CANCELLED,
                CodecFailureKind::Control(jqf_resource::ControlError::DeadlineExceeded) => codes::MACHINE_DEADLINE,
                CodecFailureKind::Control(jqf_resource::ControlError::MemoryExceeded) => codes::MACHINE_MEMORY,
                CodecFailureKind::Overflow => codes::MACHINE_OVERFLOW,
                CodecFailureKind::AllocationFailure => codes::MACHINE_ALLOCATION,
                CodecFailureKind::InternalContractViolation { .. } => codes::MACHINE_INTERNAL_CONTRACT,
                CodecFailureKind::RawNulByte => codes::MACHINE_RAW_NUL,
            },
            Self::Raised(_) => codes::RAISE_PROGRAM,
            Self::Halt { .. } => codes::RAISE_HALT,
        }
    }

    /// The payload-free operand rendering (type + path step) this error carries, for the structured record — never
    /// the offending value's payload (the projection soundness law, re-stated on data).
    #[must_use]
    pub fn diagnostic_operand(&self) -> Option<&str> {
        match self {
            Self::IterateMismatch { operand, .. }
            | Self::ObjectKeyMismatch { operand, .. }
            | Self::NoLength { operand, .. }
            | Self::NoKeys { operand, .. } => Some(operand),
            Self::TypeMismatch { key, .. } => Some(key),
            _ => None,
        }
    }

    /// The observed kind (payload-transparent category) a typed failure carries, for the record's `kind` field.
    #[must_use]
    pub fn diagnostic_operand_kind(&self) -> Option<ValueKind> {
        match self {
            Self::TypeMismatch { actual_type, .. }
            | Self::IterateMismatch { actual_type, .. }
            | Self::ObjectKeyMismatch { actual_type, .. }
            | Self::NoLength { actual_type, .. }
            | Self::NoKeys { actual_type, .. } => Some(*actual_type),
            // The arithmetic mismatch carries BOTH kinds (payload-free): the record's single kind names the left
            // operand, and the operand text renders the pair.
            Self::Arithmetic {
                failure: ArithFailure::TypeMismatch { left, .. },
                ..
            } => Some(*left),
            _ => None,
        }
    }

    /// The failing step's index in the program, when the failure class carries one.
    #[must_use]
    pub fn diagnostic_step_index(&self) -> Option<u32> {
        match self {
            Self::TypeMismatch { step_index, .. } | Self::IterateMismatch { step_index, .. } => {
                Some(u32::try_from(*step_index).unwrap_or(u32::MAX))
            }
            _ => None,
        }
    }

    /// Consumes this error into the VALUE a `catch` handler receives: a `Raised` value passes through; a typed runtime
    /// error materializes ONCE into the exact message string. A [`Self::Codec`] machine error is returned as `Err` —
    /// it is not catchable and tears through the barrier.
    pub fn into_caught(self) -> Result<Value, Self> {
        if let Self::Raised(value) = self {
            return Ok(value);
        }
        match self.typed_message()? {
            Some(text) => {
                Value::try_string(&text).map_err(|_| Self::Codec(CodecError::new(CodecFailureKind::AllocationFailure)))
            }
            // The machine channel: not catchable, returned for the barrier to tear through.
            None => Err(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// One sample per [`CodecFailureKind`] variant, so the parity sweep below covers the whole enum rather than the
    /// kinds someone remembered.
    fn every_kind() -> Vec<CodecFailureKind> {
        use jqf_resource::{ControlError, ResourceError, ResourceLimit};
        let resource = ResourceError::LimitExceeded {
            limit_kind: ResourceLimit::MemoryBytes,
            limit: 104_857_600,
            current: 92_812_697,
            requested_delta: 25_165_872,
        };
        Vec::from([
            CodecFailureKind::InvalidInput,
            CodecFailureKind::UnsupportedRepresentation,
            CodecFailureKind::RequirementMismatch,
            CodecFailureKind::ProviderRouteMismatch,
            CodecFailureKind::InvalidTag,
            CodecFailureKind::CollidingTags,
            CodecFailureKind::Resource(resource),
            CodecFailureKind::Control(ControlError::Cancelled),
            CodecFailureKind::Control(ControlError::DeadlineExceeded),
            CodecFailureKind::Control(ControlError::MemoryExceeded),
            CodecFailureKind::Overflow,
            CodecFailureKind::AllocationFailure,
            CodecFailureKind::InternalContractViolation {
                contract: "codec-kind code parity",
            },
            CodecFailureKind::RawNulByte,
        ])
    }

    /// Coverage guard for [`every_kind`]: an EXHAUSTIVE mirror match with no wildcard arm, so a variant added to
    /// `CodecFailureKind` breaks this compile until the enumeration grows a sample for it — the parity sweep cannot
    /// silently miss a kind.
    fn assert_enumeration_covers_the_enum(kind: &CodecFailureKind) {
        match kind {
            CodecFailureKind::InvalidInput
            | CodecFailureKind::UnsupportedRepresentation
            | CodecFailureKind::RequirementMismatch
            | CodecFailureKind::ProviderRouteMismatch
            | CodecFailureKind::InvalidTag
            | CodecFailureKind::CollidingTags
            | CodecFailureKind::Resource(_)
            | CodecFailureKind::Control(
                jqf_resource::ControlError::Cancelled
                | jqf_resource::ControlError::DeadlineExceeded
                | jqf_resource::ControlError::MemoryExceeded,
            )
            | CodecFailureKind::Overflow
            | CodecFailureKind::AllocationFailure
            | CodecFailureKind::InternalContractViolation { .. }
            | CodecFailureKind::RawNulByte => {}
        }
    }

    /// The engine's inlined kind-to-code map (the `Self::Codec` arm of [`EngineRunError::diagnostic_code`]) must agree
    /// with codec-core's own map, per kind. The two answer the same question — which registry code a machine failure
    /// carries — so they may never drift apart silently; a divergence here is a broken diagnostic contract, not a
    /// wording nit.
    #[test]
    fn codec_kind_codes_match_the_codec_core_map() {
        for kind in every_kind() {
            assert_enumeration_covers_the_enum(&kind);
            let via_core = kind.diagnostic_code();
            let via_engine = EngineRunError::Codec(CodecError::new(kind)).diagnostic_code();
            assert_eq!(via_engine, via_core, "engine and codec-core maps disagree for {kind:?}");
        }
    }

    /// The catch-eligibility law: a program raise and the typed semantic arms are catchable; the machine channel,
    /// `halt`, and the strict mismatch cell are not. The mismatch cell is the load-bearing row: intent suppression
    /// keeps it from firing inside a barrier, so a `catch` can never observe one (see
    /// [`EngineRunError::is_catch_eligible`]).
    #[test]
    fn catch_eligibility_splits_program_and_typed_raises_from_machine_classes() {
        let catchable = [
            EngineRunError::Raised(Value::Null),
            EngineRunError::TypeMismatch {
                step_index: 0,
                actual_type: ValueKind::Null,
                key: String::from("string (\"a\")"),
                hint: None,
            },
            EngineRunError::Arithmetic {
                failure: ArithFailure::DivideByZero,
                left: String::from("1"),
                right: String::from("0"),
            },
        ];
        for error in &catchable {
            assert!(error.is_catch_eligible(), "{error:?} must be catchable");
        }
        let machine = [
            EngineRunError::Codec(CodecError::new(CodecFailureKind::AllocationFailure)),
            EngineRunError::Halt {
                status: 5,
                message: None,
            },
            EngineRunError::MismatchRaised { cell: 0 },
        ];
        for error in &machine {
            assert!(!error.is_catch_eligible(), "{error:?} must tear through barriers");
        }
    }
}
