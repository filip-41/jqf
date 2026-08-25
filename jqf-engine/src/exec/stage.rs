//! The Stage node family's per-step navigation over located and owned values.
//!
//! One job: apply a residual stage's static steps to one input value, walking
//! borrowed located handles field/index by field/index until it reaches the next
//! `.[]` iteration point ([`descend`] returns [`Descended::FanOut`]) or the leaf
//! output ([`Descended::Leaf`]), and materializing one child at a time out of a
//! located or owned container ([`next_child`]). Located values stay located: a
//! resolved child is a fresh [`LocatedProduct`] into the same authoritative
//! document. Owned values also navigate here now that literals and constructors
//! produce them: `null` (null-index precedence / a pushed-down missing path), an
//! owned scalar (a literal base, e.g. `3[]?`, `(1).a?`), and an owned
//! array/object (a constructor base, e.g. `[.[]][0]`, `{a: .b}.a`) — an owned
//! container's children clone out as owned values.
//!
//! Negative space: it drives no cardinality itself — the fan-out frame stack and
//! the depth-first ordering live in [`crate::exec`]'s driver, which calls
//! [`descend`] and [`next_child`]. It collects nothing (one child per call). It
//! interprets the exact per-step navigation law: missing keys and out-of-bounds indices
//! and indexing-into-null yield `null` and continue; indexing a non-null
//! non-container is a type mismatch (suppressed by that step's own `?`);
//! iterating a non-container is the distinct iterate-mismatch class (suppressed
//! by the iterate step's own `?`).

use alloc::string::String;
use alloc::vec::Vec;

use jqf_codec_core::{CodecError, CodecFailureKind, LocatedProduct};
use jqf_data::{Array, DataError, Document, NodeHandle, NodeId, Object, ScalarView, Value, ValueKind};
use jqf_resource::ResourceContext;

use crate::exec::EngineRunError;
use crate::exec::StepScratch;
use crate::exec::overlay_read_payload;
use crate::program::{ProgramNode, ProgramNodeId, SliceBound, SliceBounds, StageStep, StepAccess, VarSlot};
use jqf_builtins::codec_result::EngineResult;
use jqf_builtins::error::message;
use jqf_builtins::error::mismatch;
use jqf_builtins::semantics::{
    DynAccess, OwnedNav, accessor_matches_fact, clone_owned, dyn_index, navigate_owned_index, navigate_owned_key,
    owned_kind, resolve_slice_bound, unrepresentable_index,
};

/// A container an `.[]` step fans out — either still located in its authoritative
/// document, or an owned container produced by a construction barrier (a literal
/// or constructor result, e.g. `[1,2,3][]`). Owned children clone out.
#[derive(Debug)]
pub(crate) enum Container<'source> {
    /// A located array/object iterated by borrowed handle; each child is a fresh
    /// located product into the same document.
    Located(LocatedProduct<'source>),
    /// A located array iterated ONLY at the pre-matched child positions — the
    /// The markup member step's plural arm (`<b/>` twice → `.b` emits both),
    /// and nothing else. The cursor walks `positions` in ascending original
    /// order; each entry is a position into the located array.
    LocatedFiltered(LocatedProduct<'source>, alloc::vec::Vec<usize>),
    /// An owned array/object; each child is cloned out as an owned value.
    Owned(Value),
}

/// The result of applying a residual's steps to one input value.
#[derive(Debug)]
pub(crate) enum Descended<'source> {
    /// The steps were exhausted; `value` is one ordered leaf output.
    Leaf(EngineResult<'source>),
    /// An `.[]` step was reached over a container; the driver iterates
    /// `container`, applying `steps[next_at ..]` to each child.
    FanOut {
        /// The residual index the frame's children resume at (for `Each`, the
        /// step AFTER the iteration).
        next_at: usize,
        /// The container to iterate (located or owned).
        container: Container<'source>,
    },
    /// A `..` step was reached over a CONTAINER: the pre-order self-first
    /// fan-out. The driver pushes a frame over `container` whose children resume
    /// at `next_at` — the descend step's OWN index, so every child descends
    /// again — while continuing `value` (the input itself) at `at`, the step
    /// after the descent. Because the driver drains the pending continuation
    /// before advancing any frame, self is emitted before its descendants.
    ///
    /// A `..` over a NON-container never reaches here: it has no children, so it
    /// simply continues to the next step ([`Descended::Leaf`] in the common case).
    Descend {
        /// The residual index the frame's children resume at (the `..` step).
        next_at: usize,
        /// The container whose children recurse.
        container: Container<'source>,
        /// The input itself, continuing the residual past the `..` step.
        value: EngineResult<'source>,
        /// The residual index `value` continues at (the `..` step plus one).
        at: usize,
    },
    /// This value produced nothing: an exact-step `?` suppressed a mismatch or an
    /// iterate error on it. The driver continues with the next sibling.
    Skip,
}

/// One single-valued navigation outcome (a `Key` or `Index` step).
enum Nav<'source> {
    /// The step resolved to a located child.
    Value(EngineResult<'source>),
    /// Null precedence: a missing key, an out-of-bounds index, or indexing into
    /// null. Null precedence yields `null` and continues regardless of the step's `?` flag.
    Null,
    /// The step addressed the wrong non-null semantic category. The hint is
    /// the markup accessor hint (`None` everywhere else): a member step that
    /// missed but matches a fact/attribute name carries the `.@`/`.&` hint to
    /// the raise site.
    Mismatch {
        kind: ValueKind,
        hint: Option<alloc::string::String>,
    },
}

/// One Key-step outcome: markup element-array fan-out, or ordinary navigation.
enum KeyStep<'source> {
    /// A markup ELEMENT array matched one or more children; the walk fans out.
    FanOut(Descended<'source>),
    /// Ordinary object-key navigation (hit, missing-null, or mismatch).
    Nav(Nav<'source>),
}

/// Applies `steps[at..]` to `value`, walking single-valued `Key`/`Index` steps
/// until it reaches a leaf, an `Each` fan-out, or a suppressed skip.
///
/// `base` is the global step offset (the pushdown prefix length): a mismatch at
/// residual step `j` reports global step `base + j`, so CLI path-step rendering
/// and the `?` flag lookups agree with the codec's prefix-relative indices.
///
/// # Errors
///
/// Returns [`EngineRunError::TypeMismatch`] for an unsuppressed non-null
/// `Key`/`Index` mismatch, [`EngineRunError::IterateMismatch`] for an
/// unsuppressed `Each` over a non-container, and [`EngineRunError::Codec`] for an
/// internal navigation contract violation over an otherwise valid document.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per stage-step family: the dynamic slice/subsequence arms joined the existing               total single-valued-navigation table"
)]
pub(crate) fn descend<'source>(
    steps: &[StageStep],
    base: usize,
    env: &[EngineResult<'source>],
    mut value: EngineResult<'source>,
    mut at: usize,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Descended<'source>, EngineRunError> {
    // A codec may defer the document ROOT to one container span (the CBOR
    // whole-document route's source-retention mode). The engine never walks a
    // span, so the root is materialized once before the first step and every
    // subsequent step navigates the owned value exactly as a construction
    // barrier's result would. Span CHILDREN are already handled at each child's
    // own materialization ([`retain_or_materialize`]); this is the root's
    // counterpart, and it runs once per walk entry over a span-backed document.
    if let EngineResult::Located(located) = &value {
        let document = located.product().document();
        if let Some(owned) = materialize_if_span(document, located.node(), scratch)? {
            value = EngineResult::owned(owned);
        }
    }
    loop {
        let Some(step) = steps.get(at) else {
            return Ok(Descended::Leaf(value));
        };
        let global = base + at;
        let nav = match step.access() {
            StepAccess::Each => return each_step(value, at + 1, global, step.is_optional()),
            StepAccess::Descend => match descend_step(value, at)? {
                DescendStep::FanOut(fan_out) => return Ok(fan_out),
                // A scalar (or `null`) descends trivially: it emits only itself,
                // so the walk simply continues past the step.
                DescendStep::Scalar(scalar) => {
                    value = scalar;
                    at += 1;
                    continue;
                }
            },
            StepAccess::Slice(bounds) => match slice_step(&value, bounds, env, step.cell_suppressed(), scratch)? {
                Sliced::Null => Nav::Null,
                Sliced::Value(sliced) => Nav::Value(EngineResult::owned(sliced)),
                Sliced::BadBound => {
                    return if step.is_optional() {
                        Ok(Descended::Skip)
                    } else {
                        Err(EngineRunError::SliceIndices)
                    };
                }
                Sliced::NotSliceable { kind, key } => {
                    return if step.is_optional() {
                        Ok(Descended::Skip)
                    } else {
                        Err(EngineRunError::TypeMismatch {
                            step_index: global,
                            actual_type: kind,
                            key,
                            hint: None,
                        })
                    };
                }
            },
            StepAccess::Key(key) => match key_step(&value, key, step.cell_suppressed(), at + 1, scratch)? {
                KeyStep::FanOut(fan_out) => return Ok(fan_out),
                KeyStep::Nav(nav) => nav,
            },
            StepAccess::Index(index) => navigate_index(&value, *index, step.cell_suppressed(), scratch)?,
            StepAccess::DynVar(slot) => match dyn_access(env, *slot, scratch)? {
                DynAccess::Key(key) => navigate_key(&value, key, step.cell_suppressed(), scratch, None)?,
                DynAccess::Index(index) => navigate_index(&value, index, step.cell_suppressed(), scratch)?,
                DynAccess::OutOfRange => match result_kind(&value)? {
                    ValueKind::Null | ValueKind::Array => mismatch::resolve_at(
                        scratch.resources(),
                        mismatch::MismatchCell::IndexOutOfRange,
                        step.cell_suppressed(),
                        Nav::Null,
                    )?,
                    kind => Nav::Mismatch { kind, hint: None },
                },
                DynAccess::Slice(bounds) => {
                    match dynamic_slice(&value, &bounds, step.cell_suppressed(), scratch)? {
                        Sliced::Null => Nav::Null,
                        Sliced::Value(sliced) => Nav::Value(EngineResult::owned(sliced)),
                        Sliced::BadBound => {
                            return if step.is_optional() {
                                Ok(Descended::Skip)
                            } else {
                                Err(EngineRunError::SliceIndices)
                            };
                        }
                        // A dynamic slice over a non-sliceable input renders the
                        // ACTUAL bound key (mismatch_key), never the synthesized
                        // authored-bounds object.
                        Sliced::NotSliceable { kind, .. } => Nav::Mismatch { kind, hint: None },
                    }
                }
                DynAccess::Subsequence(needle) => dynamic_subsequence(&value, &needle, scratch)?,
                DynAccess::Mismatch => Nav::Mismatch {
                    kind: result_kind(&value)?,
                    hint: None,
                },
            },
            // A `.@name`/`.&name` accessor is a TOTAL single-output fact read:
            // it answers the input node's attached fact (or `null` for absent),
            // never a mismatch, so it needs no `?` handling and never skips.
            StepAccess::NodeAccessor(selector) => {
                Nav::Value(EngineResult::owned(accessor_answer(&value, selector, false, scratch)?))
            }
            StepAccess::Attribute(selector) => {
                Nav::Value(EngineResult::owned(accessor_answer(&value, selector, true, scratch)?))
            }
            StepAccess::DynNodeAccessor(slot) => match dyn_accessor_name(env, *slot, false)? {
                Some(name) => Nav::Value(EngineResult::owned(accessor_answer(&value, &name, false, scratch)?)),
                None => Nav::Mismatch {
                    kind: result_kind(&value)?,
                    hint: None,
                },
            },
            StepAccess::DynAttribute(slot) => match dyn_accessor_name(env, *slot, true)? {
                Some(name) => Nav::Value(EngineResult::owned(accessor_answer(&value, &name, true, scratch)?)),
                None => Nav::Mismatch {
                    kind: result_kind(&value)?,
                    hint: None,
                },
            },
        };
        match nav {
            Nav::Value(next) => value = next,
            Nav::Null => value = EngineResult::owned(Value::Null),
            Nav::Mismatch { kind, hint } => {
                return if step.is_optional() {
                    Ok(Descended::Skip)
                } else {
                    Err(EngineRunError::TypeMismatch {
                        step_index: global,
                        actual_type: kind,
                        key: mismatch_key(step, env)?,
                        hint,
                    })
                };
            }
        }
        at += 1;
    }
}

/// Applies `steps[at..]` to a BORROW of an OWNED bound slot value (the
/// per-reference clone rule): the whole slot value is NEVER deep-cloned up front.
/// A leaf clones only the emitted value, and an `.[]` clones only the container it
/// fans out.
///
/// This is the owned half of a `Variable`-start stage. A LOCATED slot takes
/// [`descend`] instead — the engine's native located walk, entered on a re-borrow
/// of the handle that copies no document bytes.
///
/// `root` is the borrowed slot value (a `Variable`-start stage's start) and `env`
/// is the whole slot vector, which `DynVar` steps read; the two may alias, so both
/// are shared borrows.
///
/// # Errors
///
/// The same typed classes [`descend`] reports, plus an allocation failure when
/// cloning the emitted leaf or fan-out container fails.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per step access: the walk IS the table, and the accessor arms share its shape"
)]
pub(crate) fn descend_borrowed<'source>(
    steps: &[StageStep],
    base: usize,
    env: &[EngineResult<'source>],
    root: &Value,
    at: usize,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Descended<'source>, EngineRunError> {
    // Null precedence yields a value the walk continues from; it lives here so
    // the cursor can stay a borrow all the way to the emitted leaf.
    let null = Value::Null;
    let mut cursor: &Value = root;
    let mut at = at;
    loop {
        let Some(step) = steps.get(at) else {
            return Ok(Descended::Leaf(EngineResult::owned(clone_owned(cursor))));
        };
        let global = base + at;
        let nav = match step.access() {
            StepAccess::Each => return each_owned_step(cursor, at + 1, global, step.is_optional()),
            // `..` over a borrowed OWNED value: the fanned container clones out
            // for the frame AND the self value clones out for its own
            // continuation, so an owned descent pays one extra container clone
            // per level (a RECORDED cost of walking an owned slot; a located
            // slot takes the zero-copy walk above instead).
            StepAccess::Descend => match owned_kind(cursor) {
                ValueKind::Array | ValueKind::Object => {
                    return Ok(Descended::Descend {
                        next_at: at,
                        container: Container::Owned(clone_owned(cursor)),
                        value: EngineResult::owned(clone_owned(cursor)),
                        at: at + 1,
                    });
                }
                // A scalar descends trivially: it emits only itself.
                _ => {
                    at += 1;
                    continue;
                }
            },
            StepAccess::Slice(bounds) => match slice_owned(cursor, bounds, env, step.cell_suppressed(), scratch)? {
                Sliced::Null => mismatch::resolve_at(
                    scratch.resources(),
                    mismatch::MismatchCell::IndexOrSliceOnNull,
                    step.cell_suppressed(),
                    OwnedNav::Null,
                )?,
                Sliced::Value(sliced) => {
                    return descend(steps, base, env, EngineResult::owned(sliced), at + 1, scratch);
                }
                Sliced::BadBound => {
                    return if step.is_optional() {
                        Ok(Descended::Skip)
                    } else {
                        Err(EngineRunError::SliceIndices)
                    };
                }
                Sliced::NotSliceable { kind, key } => {
                    return if step.is_optional() {
                        Ok(Descended::Skip)
                    } else {
                        Err(EngineRunError::TypeMismatch {
                            step_index: global,
                            actual_type: kind,
                            key,
                            hint: None,
                        })
                    };
                }
            },
            StepAccess::Key(key) => navigate_owned_key(cursor, key, step.cell_suppressed(), scratch.resources())?,
            StepAccess::Index(index) => {
                navigate_owned_index(cursor, *index, step.cell_suppressed(), scratch.resources())?
            }
            StepAccess::DynVar(slot) => match dyn_access(env, *slot, scratch)? {
                DynAccess::Key(key) => navigate_owned_key(cursor, key, step.cell_suppressed(), scratch.resources())?,
                DynAccess::Index(index) => {
                    navigate_owned_index(cursor, index, step.cell_suppressed(), scratch.resources())?
                }
                DynAccess::OutOfRange => unrepresentable_index(cursor, step.cell_suppressed(), scratch.resources())?,

                DynAccess::Slice(bounds) => {
                    match dynamic_slice_owned(cursor, &bounds, step.cell_suppressed(), scratch)? {
                        Sliced::Null => mismatch::resolve_at(
                            scratch.resources(),
                            mismatch::MismatchCell::IndexOrSliceOnNull,
                            step.cell_suppressed(),
                            OwnedNav::Null,
                        )?,
                        Sliced::Value(sliced) => {
                            return descend(steps, base, env, EngineResult::owned(sliced), at + 1, scratch);
                        }
                        Sliced::BadBound => {
                            return if step.is_optional() {
                                Ok(Descended::Skip)
                            } else {
                                Err(EngineRunError::SliceIndices)
                            };
                        }
                        Sliced::NotSliceable { kind, .. } => OwnedNav::Mismatch(kind),
                    }
                }
                DynAccess::Subsequence(needle) => {
                    match dynamic_subsequence_owned(cursor, &needle, scratch.resources())? {
                        Some(positions) => {
                            return descend(steps, base, env, EngineResult::owned(positions), at + 1, scratch);
                        }
                        None => OwnedNav::Mismatch(owned_kind(cursor)),
                    }
                }
                DynAccess::Mismatch => OwnedNav::Mismatch(owned_kind(cursor)),
            },
            // A `.@name`/`.&name` accessor over an OWNED slot value produces a
            // fresh owned result (the tag text, or `null` for a fact an owned
            // value cannot carry), so the walk hands it to the owned `descend`
            // — the same switch a `Slice` result takes.
            StepAccess::NodeAccessor(selector) => {
                let result = accessor_owned(cursor, selector, false)?;
                return descend(steps, base, env, EngineResult::owned(result), at + 1, scratch);
            }
            StepAccess::Attribute(selector) => {
                let result = accessor_owned(cursor, selector, true)?;
                return descend(steps, base, env, EngineResult::owned(result), at + 1, scratch);
            }
            StepAccess::DynNodeAccessor(slot) => match dyn_accessor_name(env, *slot, false)? {
                Some(name) => {
                    let result = accessor_owned(cursor, &name, false)?;
                    return descend(steps, base, env, EngineResult::owned(result), at + 1, scratch);
                }
                None => OwnedNav::Mismatch(owned_kind(cursor)),
            },
            StepAccess::DynAttribute(slot) => match dyn_accessor_name(env, *slot, true)? {
                Some(name) => {
                    let result = accessor_owned(cursor, &name, true)?;
                    return descend(steps, base, env, EngineResult::owned(result), at + 1, scratch);
                }
                None => OwnedNav::Mismatch(owned_kind(cursor)),
            },
        };
        match nav {
            OwnedNav::Value(next) => cursor = next,
            OwnedNav::Null => cursor = &null,
            OwnedNav::Mismatch(kind) => {
                return if step.is_optional() {
                    Ok(Descended::Skip)
                } else {
                    Err(EngineRunError::TypeMismatch {
                        step_index: global,
                        actual_type: kind,
                        key: mismatch_key(step, env)?,
                        hint: None,
                    })
                };
            }
        }
        at += 1;
    }
}

/// Interprets an `Each` step over a borrowed owned value: a container clones out
/// as the fan-out container (only the iterated subtree, never the whole slot).
fn each_owned_step<'source>(
    value: &Value,
    next_at: usize,
    global: usize,
    optional: bool,
) -> Result<Descended<'source>, EngineRunError> {
    match owned_kind(value) {
        ValueKind::Array | ValueKind::Object => Ok(Descended::FanOut {
            next_at,
            container: Container::Owned(clone_owned(value)),
        }),
        _ if optional => Ok(Descended::Skip),
        kind => Err(EngineRunError::IterateMismatch {
            step_index: global,
            actual_type: kind,
            operand: message::dump_trunc_owned(value)?,
        }),
    }
}

/// One `..` step outcome: a container fans out pre-order, any other value passes
/// straight through to the next step.
#[derive(Debug)]
enum DescendStep<'source> {
    /// A container: the self-first fan-out the driver turns into a frame.
    FanOut(Descended<'source>),
    /// A non-container: `..` emits it alone, so the walk continues past the step.
    Scalar(EngineResult<'source>),
}

/// Interprets a `..` step over `value`.
///
/// A container becomes the pre-order self-first fan-out: its children resume AT
/// this step (so each recurses), while the value itself continues at `at + 1`.
/// Any other value emits ONLY itself, so it passes through — `..` is the one step
/// with no error path at all, because every value descends.
///
/// The fanned container and the self emission are the SAME value. A located
/// handle re-borrows Arc-cheap (no document bytes copied, so a located walk stays
/// zero-copy); an OWNED container must be cloned once per level — the recorded
/// cost of descending a constructor/literal product rather than a document.
fn descend_step(value: EngineResult<'_>, at: usize) -> Result<DescendStep<'_>, EngineRunError> {
    let kind = match &value {
        EngineResult::Located(located) => located_kind(located)?,
        EngineResult::Owned(owned) => owned_kind(owned),
    };
    if !matches!(kind, ValueKind::Array | ValueKind::Object) {
        return Ok(DescendStep::Scalar(value));
    }
    let container = match &value {
        EngineResult::Located(located) => Container::Located(located.try_clone().map_err(EngineRunError::Codec)?),
        EngineResult::Owned(owned) => Container::Owned(clone_owned(owned)),
    };
    Ok(DescendStep::FanOut(Descended::Descend {
        next_at: at,
        container,
        value,
        at: at + 1,
    }))
}

/// The outcome of one `.[a:b]` step — the four arms of the pinned runtime
/// dispatch.
#[derive(Debug)]
enum Sliced {
    /// The input was `null`: null precedence yields `null` and continues, EVEN with bounds
    /// that would raise over an array (`null | .["a":2]` is `null`, exit 0).
    Null,
    /// The materialized subrange: a NEW owned array, or a new owned string cut on
    /// CODEPOINT boundaries.
    Value(Value),
    /// An array/string input with a bound that is neither a number nor `null`:
    /// The dedicated `Array/string slice indices must be integers` class.
    BadBound,
    /// A non-sliceable input: the index-class mismatch, whose accessor is the
    /// synthesized object of the AUTHORED bounds.
    NotSliceable {
        /// The input's payload-transparent category (the `Cannot index <kind>` message).
        kind: ValueKind,
        /// The pre-rendered accessor (`object ({"start":1.5,"end":3})`).
        key: String,
    },
}

/// One resolved slice bound.
enum Bound {
    /// An open end: an omitted bound, or an authored/bound `null`.
    Open,
    /// A numeric bound, projected to binary64 (located numbers project through
    /// their borrowed view, so a `$var` bound never materializes).
    Number(f64),
    /// A bound of any other type — the integer-bound raise over an array/string.
    Bad,
}

/// The ONE runtime slice resolver, in the pinned dispatch precedence: the
/// INPUT's type is examined first, then the bounds.
///
/// `null` input yields `null` whatever the bounds are; an array or string
/// resolves its bounds under the len-relative law and materializes the subrange
/// (a non-number, non-null bound raising the integer-bound class); every other
/// input is the index-class mismatch, whose operand object is built from the
/// AUTHORED bound values — never the floored/clamped ones.
fn slice_step(
    value: &EngineResult<'_>,
    bounds: &SliceBounds,
    env: &[EngineResult<'_>],
    suppressed: bool,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Sliced, EngineRunError> {
    match value {
        EngineResult::Owned(owned) => slice_owned(owned, bounds, env, suppressed, scratch),
        EngineResult::Located(located) => slice_located(located, bounds, env, suppressed, scratch),
    }
}

/// The owned half of the slice resolver (a literal, constructor, or bound-slot
/// value). A tagged value slices its payload — tags are payload-transparent.
fn slice_owned(
    value: &Value,
    bounds: &SliceBounds,
    env: &[EngineResult<'_>],
    suppressed: bool,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Sliced, EngineRunError> {
    match value {
        Value::Null => mismatch::resolve_at(
            scratch.resources(),
            mismatch::MismatchCell::IndexOrSliceOnNull,
            suppressed,
            Sliced::Null,
        ),
        Value::Tagged { payload, .. } => slice_owned(payload, bounds, env, suppressed, scratch),
        Value::Array(array) => {
            let Some((start, end)) = slice_span(bounds, env, array.len(), suppressed, scratch.resources())? else {
                return Ok(Sliced::BadBound);
            };
            let mut out = Array::try_with_capacity(end - start).map_err(|_| EngineRunError::allocation_failure())?;
            for position in start..end {
                let child = array
                    .get(position)
                    .ok_or_else(|| internal("owned slice ran past its own clamped range"))?;
                out.try_push(clone_owned(child))
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Sliced::Value(Value::Array(out)))
        }
        Value::String(text) => slice_text(text, bounds, env, suppressed, scratch.resources()),
        other => Ok(Sliced::NotSliceable {
            kind: owned_kind(other),
            key: slice_operand_key(bounds, env, scratch)?,
        }),
    }
}

/// The located half of the slice resolver. The array subrange CLONES OUT (a slice
/// is semantically a new array), materializing one element at a time through the
/// run's reusable workspace so the O(node-count) cycle bitmap is paid once per
/// run, not once per element.
fn slice_located(
    located: &LocatedProduct<'_>,
    bounds: &SliceBounds,
    env: &[EngineResult<'_>],
    suppressed: bool,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Sliced, EngineRunError> {
    let document = located.product().document();
    let view = located_view(document, located.node()).map_err(internal_data)?;
    let kind = view.kind().map_err(internal_data)?;
    // Under `--types-as-strings` a located temporal's kind reads as
    // string, and its canonical text IS the slice operand.
    let kind = match kind {
        kind if kind.is_temporal() && scratch.resources().types_as_strings() => ValueKind::String,
        other => other,
    };
    match kind {
        ValueKind::Null => mismatch::resolve_at(
            scratch.resources(),
            mismatch::MismatchCell::IndexOrSliceOnNull,
            suppressed,
            Sliced::Null,
        ),
        ValueKind::Array => {
            let array = view
                .array()
                .map_err(internal_data)?
                .ok_or_else(|| internal("array node without an array projection"))?;
            let Some((start, end)) = slice_span(bounds, env, array.len(), suppressed, scratch.resources())? else {
                return Ok(Sliced::BadBound);
            };
            let mut out = Array::try_with_capacity(end - start).map_err(|_| EngineRunError::allocation_failure())?;
            for position in start..end {
                let child = array
                    .get(position)
                    .ok_or_else(|| internal("located slice ran past its own clamped range"))?;
                let handle = document.node_handle(child.node()).map_err(internal_data)?;
                let element = scratch.materialize(document, handle)?;
                out.try_push(element)
                    .map_err(|_| EngineRunError::allocation_failure())?;
            }
            Ok(Sliced::Value(Value::Array(out)))
        }
        ValueKind::String => match view.scalar().map_err(internal_data)? {
            Some(ScalarView::String(text)) => slice_text(text, bounds, env, suppressed, scratch.resources()),
            // A temporal scalar reads as its canonical text under the
            // flag (the kind mapping above said string; now provide the text).
            _ => {
                match jqf_builtins::registry::builtins::core::flag_temporal_text(
                    view.scalar().map_err(internal_data)?.as_ref(),
                    scratch.resources(),
                ) {
                    Some(text) => slice_text(&text, bounds, env, suppressed, scratch.resources()),
                    None => Err(internal("string node without a string scalar")),
                }
            }
        },
        kind => Ok(Sliced::NotSliceable {
            kind,
            key: slice_operand_key(bounds, env, scratch)?,
        }),
    }
}

/// Slices one string by CODEPOINT (never by byte): `"aébç" | .[1:3]` is `"éb"`.
///
/// All-ASCII text short-circuits — its codepoint indices ARE its byte indices —
/// and other text resolves both boundaries in ONE forward pass over the char
/// boundaries rather than re-scanning per bound.
fn slice_text(
    text: &str,
    bounds: &SliceBounds,
    env: &[EngineResult<'_>],
    suppressed: bool,
    resources: &ResourceContext<'_>,
) -> Result<Sliced, EngineRunError> {
    let ascii = text.is_ascii();
    let len = if ascii { text.len() } else { text.chars().count() };
    let Some((start, end)) = slice_span(bounds, env, len, suppressed, resources)? else {
        return Ok(Sliced::BadBound);
    };
    let (start_byte, end_byte) = if ascii {
        (start, end)
    } else {
        codepoint_span(text, start, end)
    };
    let cut = text
        .get(start_byte..end_byte)
        .ok_or_else(|| internal("string slice resolved to a non-boundary byte range"))?;
    Ok(Sliced::Value(
        Value::try_string(cut).map_err(|_| EngineRunError::allocation_failure())?,
    ))
}

/// The byte offsets of codepoint positions `start` and `end` in non-ASCII `text`,
/// found in one forward pass. Both positions are already clamped to `[0, len]`,
/// so a position at the very end resolves to the string's byte length.
fn codepoint_span(text: &str, start: usize, end: usize) -> (usize, usize) {
    let mut start_byte = text.len();
    let mut end_byte = text.len();
    for (position, (offset, _)) in text.char_indices().enumerate() {
        if position == start {
            start_byte = offset;
        }
        if position == end {
            end_byte = offset;
            break;
        }
    }
    (start_byte, end_byte)
}

/// The half-open element/codepoint range `[start, end)` this step selects from a
/// container of `len`, or `None` when a bound disqualifies itself by type.
///
/// A backwards range collapses to empty (the law never errors on it), so the caller
/// can always iterate `start..end`.
fn slice_span(
    bounds: &SliceBounds,
    env: &[EngineResult<'_>],
    len: usize,
    suppressed: bool,
    resources: &ResourceContext<'_>,
) -> Result<Option<(usize, usize)>, EngineRunError> {
    let start = match slice_bound(&bounds.start, env)? {
        Bound::Open => 0,
        Bound::Number(value) => resolve_slice_bound(value, len, true),
        Bound::Bad => return Ok(None),
    };
    let end = match slice_bound(&bounds.end, env)? {
        Bound::Open => len,
        Bound::Number(value) => resolve_slice_bound(value, len, false),
        Bound::Bad => return Ok(None),
    };
    // The slice-clamp cell: an authored bound whose
    // len-relative value lies OUTSIDE `[0, len]` clamped. The predicate tests
    // the AUTHORED value before rounding — a bound resolving exactly to the
    // length (`.[0:2]` over `[1,2]`) is the ordinary end, not a clamp, and
    // a fractional bound that rounds UP to it (`.[0.5:1.5]`) is not either.
    // `-0.0` is NOT a clamp (`.[:-0.0]` is the empty-slice law, not a bound that
    // moved); a NaN bound never reaches this point (it resolves as open).
    // The clamp class fires on it.
    #[allow(
        clippy::cast_precision_loss,
        reason = "a container longer than 2^53 elements cannot exist in a decoded document"
    )]
    let clamped = |value: f64| {
        let length = len as f64;
        let relative = if value < 0.0 { length + value } else { value };
        relative < 0.0 || relative > length
    };
    let authored = |bound: &SliceBound| match bound {
        SliceBound::Literal(Value::Number(number)) => Some(jqf_builtins::semantics::order::to_f64(number)),
        _ => None,
    };
    if authored(&bounds.start).is_some_and(clamped) {
        mismatch::resolve_at(resources, mismatch::MismatchCell::SliceClamped, suppressed, ())?;
    }
    if authored(&bounds.end).is_some_and(clamped) {
        mismatch::resolve_at(resources, mismatch::MismatchCell::SliceClamped, suppressed, ())?;
    }
    Ok(Some(if end <= start { (start, start) } else { (start, end) }))
}

/// Resolves one authored bound to its runtime classification, WITHOUT
/// materializing a located `$var` bound: a located number projects to binary64
/// through its borrowed view, exactly as a `.[$x]` index does.
fn slice_bound(bound: &SliceBound, env: &[EngineResult<'_>]) -> Result<Bound, EngineRunError> {
    match bound {
        SliceBound::Open => Ok(Bound::Open),
        SliceBound::Literal(value) => Ok(owned_bound(value)),
        SliceBound::Var(slot) => match slot_result(env, *slot)? {
            EngineResult::Owned(value) => Ok(owned_bound(value)),
            EngineResult::Located(located) => {
                let view = located_view(located.product().document(), located.node()).map_err(internal_data)?;
                Ok(match view.kind().map_err(internal_data)? {
                    ValueKind::Null => Bound::Open,
                    ValueKind::Number => match view.scalar().map_err(internal_data)? {
                        Some(ScalarView::Number(number)) => {
                            let value = jqf_builtins::semantics::order::view_to_f64(number);
                            if value.is_nan() {
                                Bound::Open
                            } else {
                                Bound::Number(value)
                            }
                        }
                        _ => Bound::Bad,
                    },
                    _ => Bound::Bad,
                })
            }
        },
    }
}

/// One owned bound's classification (tags are payload-transparent).
fn owned_bound(value: &Value) -> Bound {
    match value {
        Value::Null => Bound::Open,
        Value::Number(number) => {
            let value = jqf_builtins::semantics::order::to_f64(number);
            if value.is_nan() {
                Bound::Open
            } else {
                Bound::Number(value)
            }
        }
        Value::Tagged { payload, .. } => owned_bound(payload),
        _ => Bound::Bad,
    }
}

/// The index-class accessor for a slice over a non-sliceable input: the operand
/// is a SYNTHESIZED object of the AUTHORED bounds — `.[1.5:3]` over an object
/// reports `object ({"start":1.5,"end":3})`, never the floored `1`. An open bound
/// renders as `null`, exactly like an authored `null` one.
///
/// The object is an ordinary owned value, so it renders through the same bounded
/// writer (and the same truncation law) as any other operand; the
/// renderer must not try to navigate or charge it like a document value, and
/// [`message::render_owned_key`] does not.
fn slice_operand_key(
    bounds: &SliceBounds,
    env: &[EngineResult<'_>],
    scratch: &mut StepScratch<'_, '_>,
) -> Result<String, EngineRunError> {
    let mut operand = Object::try_new().map_err(|_| EngineRunError::allocation_failure())?;
    for (name, bound) in [("start", &bounds.start), ("end", &bounds.end)] {
        let key = jqf_data::ObjectKey::try_from_str(name).map_err(|_| EngineRunError::allocation_failure())?;
        let bound = authored_bound(bound, env, scratch)?;
        operand
            .try_insert_unique(key, bound)
            .map_err(|_| EngineRunError::allocation_failure())?;
    }
    message::render_owned_key(&Value::Object(operand))
}

/// One authored bound as the owned value the mismatch operand renders.
fn authored_bound(
    bound: &SliceBound,
    env: &[EngineResult<'_>],
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Value, EngineRunError> {
    match bound {
        SliceBound::Open => Ok(Value::Null),
        SliceBound::Literal(value) => Ok(clone_owned(value)),
        SliceBound::Var(slot) => match slot_result(env, *slot)? {
            EngineResult::Owned(value) => Ok(clone_owned(value)),
            EngineResult::Located(located) => {
                let document = located.product().document();
                scratch.materialize(document, located.node())
            }
        },
    }
}

/// The result bound in `slot`, or an internal-contract error when the executor's
/// env is not sized for the program's slot count. A slot holds either a located
/// handle (bound zero-copy from a document-backed source) or an owned value.
fn slot_result<'env, 'source>(
    env: &'env [EngineResult<'source>],
    slot: u32,
) -> Result<&'env EngineResult<'source>, EngineRunError> {
    env.get(slot as usize)
        .ok_or_else(|| internal("a variable step read an env slot outside the sized env"))
}

/// The writable NODE-FACT vocabulary: the three comment-position
/// selectors of the shared vocabulary plus the four METADATA roles of the YAML
/// write channel. The lowering validates STATIC selectors against this list
/// and the executor validates DYNAMIC write selectors at run time — an
/// unknown name is a typed rejection, never a silent write. Attribute
/// selectors take arbitrary names instead: facts are codec/format facts,
/// attributes are not.
pub(crate) const WRITABLE_NODE_ROLES: &[&str] = &[
    jqf_codec_core::comment::HEAD,
    jqf_codec_core::comment::INLINE,
    jqf_codec_core::comment::FOOT,
    "style",
    "tag",
    "anchor",
    "alias",
];

/// Resolves one dynamic fact-WRITE selector (`PATH.@(expr)` / `PATH.&(expr)`)
/// against the env slot the lowering bound it with. A node-fact name must be a
/// string in [`WRITABLE_NODE_ROLES`] (the `comment_head` alias normalizes like
/// its static twin); an attribute name is any string. Everything else is a
/// raised rejection naming the refusal — never a silent write, never a panic.
pub(crate) fn resolve_fact_write_selector(
    env: &[EngineResult<'_>],
    slot: VarSlot,
    attribute: bool,
    resources: &ResourceContext<'_>,
) -> Result<(String, String), EngineRunError> {
    let Some(name) = dyn_accessor_name(env, slot, attribute)? else {
        return Err(jqf_builtins::semantics::path::raise(
            "a fact assignment's dynamic selector must be a string",
            resources,
        ));
    };
    if attribute {
        return Ok((String::from(jqf_codec_core::markup::ATTRIBUTE_FACT), name));
    }
    if !WRITABLE_NODE_ROLES.contains(&name.as_str()) {
        let message = alloc::format!(
            "unknown fact write role \"{name}\" — the writable roles are {}",
            WRITABLE_NODE_ROLES.join(", ")
        );
        return Err(jqf_builtins::semantics::path::raise(&message, resources));
    }
    Ok((name, String::new()))
}

/// The accessor selector bound in `slot`: a string, with the `.@comment_head`
/// alias already normalized. `None` when the bound value is not a string —
/// a number is never an accessor name.
fn dyn_accessor_name(env: &[EngineResult<'_>], slot: u32, attribute: bool) -> Result<Option<String>, EngineRunError> {
    let name = match slot_result(env, slot)? {
        EngineResult::Owned(Value::String(key)) => String::from(key.as_str()),
        EngineResult::Owned(_) => return Ok(None),
        EngineResult::Located(located) => {
            let document = located.product().document();
            let view = located_view(document, located.node()).map_err(internal_data)?;
            match view.kind().map_err(internal_data)? {
                ValueKind::String => {
                    let Some(ScalarView::String(key)) = view.scalar().map_err(internal_data)? else {
                        return Err(internal("a located string kind without a string scalar"));
                    };
                    String::from(key)
                }
                _ => return Ok(None),
            }
        }
    };
    Ok(Some(if !attribute && name == jqf_codec_core::comment::HEAD_ALIAS {
        String::from(jqf_codec_core::comment::HEAD)
    } else {
        name
    }))
}

/// Resolves one `.[$x]` accessor by dispatching on the BOUND value's runtime type
/// (the lowerer cannot know which access `$x` denotes), WITHOUT materializing the
/// bound value: a located string keys off document text and a located number
/// projects to binary64 through its borrowed view.
///
/// A string reads an object key, a number an array position; every other bound
/// type is the index-class mismatch, INCLUDING over a null container:
/// `null | true as $i | .[$i]` errors while `null | 1 as $i | .[$i]` is null.
fn dyn_access<'env>(
    env: &'env [EngineResult<'_>],
    slot: u32,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<DynAccess<'env>, EngineRunError> {
    match slot_result(env, slot)? {
        EngineResult::Owned(Value::String(key)) => Ok(DynAccess::Key(key)),
        EngineResult::Owned(Value::Number(number)) => Ok(dyn_index(jqf_builtins::semantics::order::to_f64(number))),
        EngineResult::Owned(Value::Object(bounds)) => Ok(DynAccess::Slice(bounds.clone())),
        EngineResult::Owned(Value::Array(needle)) => Ok(DynAccess::Subsequence(needle.clone())),
        EngineResult::Owned(_) => Ok(DynAccess::Mismatch),
        EngineResult::Located(located) => {
            let document = located.product().document();
            let view = located_view(document, located.node()).map_err(internal_data)?;
            let kind = view.kind().map_err(internal_data)?;
            Ok(match kind {
                ValueKind::String => {
                    let Some(ScalarView::String(key)) = view.scalar().map_err(internal_data)? else {
                        return Err(internal("a located string kind without a string scalar"));
                    };
                    DynAccess::Key(key)
                }
                ValueKind::Number => {
                    let Some(ScalarView::Number(number)) = view.scalar().map_err(internal_data)? else {
                        return Err(internal("a located number kind without a number scalar"));
                    };
                    dyn_index(jqf_builtins::semantics::order::view_to_f64(number))
                }
                // A located OBJECT/ARRAY component (a bound container node used
                // as a key) materializes so the slice/subsequence key law is the
                // owned one — the rare path, and the price is one node.
                ValueKind::Object | ValueKind::Array => {
                    let owned = scratch.materialize(document, located.node())?;
                    match owned.untagged() {
                        Value::Object(bounds) => DynAccess::Slice(bounds.clone()),
                        Value::Array(needle) => DynAccess::Subsequence(needle.clone()),
                        _ => DynAccess::Mismatch,
                    }
                }
                _ => DynAccess::Mismatch,
            })
        }
    }
}

/// The slice-object key (`.[{"start":a,"end":b}]`) as the step's authored
/// bounds, raising the slice-indices rejection for a MISSING bound — but only
/// when the input is actually sliceable: `null | .[{}]` is `null` with the
/// bounds never consulted, and the index-class mismatch over any other input
/// renders the actual key. The caller dispatches the kind before
/// calling this.
fn slice_bounds_from_object(bounds: &jqf_data::Object) -> Option<SliceBounds> {
    let (Some(start), Some(end)) = (
        bounds.get(jqf_builtins::semantics::path::SLICE_START),
        bounds.get(jqf_builtins::semantics::path::SLICE_END),
    ) else {
        // A MISSING bound is the slice-indices rejection, rendered as the
        // step's BadBound class so an owning `?` suppresses it exactly like a
        // malformed authored bound (`[1,2,3] | .[{"start":1}]?` is
        // empty, `.[{"start":1}]` raises).
        return None;
    };
    Some(SliceBounds {
        start: SliceBound::Literal(start.clone()),
        end: SliceBound::Literal(end.clone()),
    })
}

/// The slice-object key (`.[{"start":a,"end":b}]`) over a LOCATED or OWNED
/// step value: the slice law over an array or string, `null` over `null`, and the
/// index-class mismatch over anything else — the same law the path walk's
/// `get_component` implements (the step and the path now agree).
fn dynamic_slice(
    value: &EngineResult<'_>,
    bounds: &jqf_data::Object,
    suppressed: bool,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Sliced, EngineRunError> {
    let kind = result_kind(value)?;
    // Under `--types-as-strings` a located temporal slices as its
    // canonical text.
    let kind = match kind {
        kind if kind.is_temporal() && scratch.resources().types_as_strings() => ValueKind::String,
        other => other,
    };
    match kind {
        ValueKind::Null => slice_step(
            value,
            &SliceBounds {
                start: SliceBound::Open,
                end: SliceBound::Open,
            },
            &[],
            suppressed,
            scratch,
        ),
        ValueKind::Array | ValueKind::String => {
            let Some(authored) = slice_bounds_from_object(bounds) else {
                return Ok(Sliced::BadBound);
            };
            slice_step(value, &authored, &[], suppressed, scratch)
        }
        _ => Ok(Sliced::NotSliceable {
            kind,
            key: alloc::string::String::new(),
        }),
    }
}

/// The slice-object key over an OWNED slot value — the owned mirror of
/// [`dynamic_slice`].
fn dynamic_slice_owned(
    cursor: &Value,
    bounds: &jqf_data::Object,
    suppressed: bool,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Sliced, EngineRunError> {
    let kind = owned_kind(cursor);
    match kind {
        ValueKind::Null => slice_owned(
            cursor,
            &SliceBounds {
                start: SliceBound::Open,
                end: SliceBound::Open,
            },
            &[],
            suppressed,
            scratch,
        ),
        ValueKind::Array | ValueKind::String => {
            let Some(authored) = slice_bounds_from_object(bounds) else {
                return Ok(Sliced::BadBound);
            };
            slice_owned(cursor, &authored, &[], suppressed, scratch)
        }
        _ => Ok(Sliced::NotSliceable {
            kind,
            key: alloc::string::String::new(),
        }),
    }
}

/// The subsequence-search key (`.[[1,2]]`) over a LOCATED or OWNED step value:
/// the positions where the needle appears, the same law the path walk's
/// `get_component` implements (the step divergence lifted).
fn dynamic_subsequence<'source>(
    value: &EngineResult<'source>,
    needle: &jqf_data::Array,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Nav<'source>, EngineRunError> {
    match value {
        EngineResult::Owned(owned) => match owned.untagged() {
            Value::Array(array) => {
                let positions = jqf_builtins::semantics::text::subsequence_indices(array, needle, scratch.resources())?;
                Ok(Nav::Value(EngineResult::owned(positions)))
            }
            _ => Ok(Nav::Mismatch {
                kind: owned_kind(owned),
                hint: None,
            }),
        },
        EngineResult::Located(located) => {
            let document = located.product().document();
            let owned = scratch.materialize(document, located.node())?;
            match owned.untagged() {
                Value::Array(array) => {
                    let positions =
                        jqf_builtins::semantics::text::subsequence_indices(array, needle, scratch.resources())?;
                    Ok(Nav::Value(EngineResult::owned(positions)))
                }
                _ => Ok(Nav::Mismatch {
                    kind: owned_kind(&owned),
                    hint: None,
                }),
            }
        }
    }
}

/// The subsequence-search key over an OWNED slot value — the owned mirror of
/// [`dynamic_subsequence`]. The positions array is a FRESH owned value (it
/// cannot borrow the cursor), so the caller returns it through `descend` like
/// any owned navigation result; `None` names the index-class mismatch.
fn dynamic_subsequence_owned(
    cursor: &Value,
    needle: &jqf_data::Array,
    resources: &ResourceContext<'_>,
) -> Result<Option<Value>, EngineRunError> {
    match cursor.untagged() {
        Value::Array(array) => jqf_builtins::semantics::text::subsequence_indices(array, needle, resources).map(Some),
        _ => Ok(None),
    }
}

/// The payload-transparent category of a located or owned engine result.
fn result_kind(value: &EngineResult<'_>) -> Result<ValueKind, EngineRunError> {
    match value {
        EngineResult::Located(located) => located_kind(located),
        EngineResult::Owned(owned) => Ok(owned_kind(owned)),
    }
}

/// Interprets an `Each` step over `value`: a located or owned array/object fans
/// out, any other value (including owned `null`) is a non-iterable — an iterate
/// mismatch, or a skip when the step carries its own `?`.
fn each_step(
    value: EngineResult<'_>,
    next_at: usize,
    global: usize,
    optional: bool,
) -> Result<Descended<'_>, EngineRunError> {
    // Peek the kind without moving `value`, so the non-iterable branch can render
    // the operand for the catch-visible message before the value is consumed.
    let kind = match &value {
        EngineResult::Located(container) => located_kind(container)?,
        EngineResult::Owned(owned) => owned_kind(owned),
    };
    match kind {
        ValueKind::Array | ValueKind::Object => {
            let container = match value {
                EngineResult::Located(container) => Container::Located(container),
                EngineResult::Owned(owned) => Container::Owned(owned),
            };
            Ok(Descended::FanOut { next_at, container })
        }
        _ if optional => Ok(Descended::Skip),
        kind => Err(EngineRunError::IterateMismatch {
            step_index: global,
            actual_type: kind,
            operand: message::dump_trunc(&value)?,
        }),
    }
}

/// The typed rendering of the offending accessor at an index-class mismatch.
fn mismatch_key(step: &StageStep, env: &[EngineResult<'_>]) -> Result<alloc::string::String, EngineRunError> {
    match step.access() {
        StepAccess::Key(name) => message::render_object_key(name),
        StepAccess::Index(index) => message::render_array_key(*index),
        // A `.[$x]` mismatch renders the BOUND value's own type and JSON
        // ("Cannot index array with boolean (true)", "… with null (null)").
        StepAccess::DynVar(slot) | StepAccess::DynNodeAccessor(slot) | StepAccess::DynAttribute(slot) => {
            message::render_dynamic_key(slot_result(env, *slot)?)
        }
        // Neither `Each` nor `Descend` nor `Slice` nor a static accessor
        // reaches the generic index-class mismatch: `Each` is the iterate
        // class (`each_step`), `Descend` has no error path at all, a static
        // accessor is a total single-output fact read (it answers `null`,
        // never a mismatch), and a `Slice` renders its SYNTHESIZED
        // authored-bounds operand at its own raise site. A dynamic accessor
        // whose selector is not a string uses the DynVar rendering above.
        // Should a new access kind ever route here, fail loud rather than
        // publish a plausible wrong accessor in the message.
        StepAccess::Each
        | StepAccess::Descend
        | StepAccess::Slice(_)
        | StepAccess::NodeAccessor(_)
        | StepAccess::Attribute(_) => Err(internal(
            "an access kind with no mismatch path reached the index-class renderer",
        )),
    }
}

/// Payload-transparent view and kind of a located Key-step input. Owned
/// values have no document view; [`navigate_key`]'s owned arm answers them.
fn located_payload<'d, 's>(
    value: &'d EngineResult<'s>,
) -> Result<Option<(jqf_data::ValueView<'d, 's>, ValueKind)>, EngineRunError> {
    match value {
        EngineResult::Owned(_) => Ok(None),
        EngineResult::Located(located) => {
            let view = located_view(located.product().document(), located.node()).map_err(internal_data)?;
            let kind = view.kind().map_err(internal_data)?;
            Ok(Some((view, kind)))
        }
    }
}

/// One Key step: one payload-transparent view/kind, then the markup member
/// probe and the ordinary object-key lookup both read that same pair.
/// Markup element-array matching, optional-suffix skip, and the `.@`/`.&`
/// accessor steps are unchanged — accessors are a different [`StepAccess`].
fn key_step<'source>(
    value: &EngineResult<'source>,
    key: &str,
    suppressed: bool,
    next_at: usize,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<KeyStep<'source>, EngineRunError> {
    let payload = located_payload(value)?;
    match markup_member_step(value, key, scratch, payload)? {
        // A member step over a MARKUP ELEMENT array navigates the children by
        // element name. Every match fans out through LocatedFiltered so path
        // recording sees the document's real index (one child or many); no
        // match (or a non-markup array) falls through to the ordinary
        // array-with-string mismatch — never a silent null.
        MarkupStep::Many(positions) => {
            let located = match value {
                EngineResult::Located(located) => located.try_clone().map_err(EngineRunError::Codec)?,
                EngineResult::Owned(_) => unreachable!("markup steps are located-only"),
            };
            Ok(KeyStep::FanOut(Descended::FanOut {
                next_at,
                container: Container::LocatedFiltered(located, positions),
            }))
        }
        MarkupStep::NoMatch { hint } => {
            // The ordinary hard mismatch (never a silent null), with the
            // accessor hint attached when the missed name matched a
            // fact/attribute: `navigate_key` reports the kind, then the hint
            // rides on the nav into the raise below.
            let nav = match navigate_key(value, key, suppressed, scratch, payload)? {
                Nav::Mismatch { kind, .. } => Nav::Mismatch { kind, hint },
                other => other,
            };
            Ok(KeyStep::Nav(nav))
        }
        MarkupStep::NotMarkup => Ok(KeyStep::Nav(navigate_key(value, key, suppressed, scratch, payload)?)),
    }
}

/// Navigates one object key over `value` (null precedence, missing → null). A
/// located object resolves to a fresh located child; an owned object (a
/// constructor/literal result) clones its child out.
///
/// `payload` is the already-derived located view/kind from the Key step.
/// [`key_step`] supplies it so the markup probe and this lookup do not each
/// open a view; a dynamic string index that never ran the markup probe
/// passes `None` and this function derives once.
fn navigate_key<'d, 'source>(
    value: &'d EngineResult<'source>,
    key: &str,
    suppressed: bool,
    scratch: &mut StepScratch<'_, '_>,
    payload: Option<(jqf_data::ValueView<'d, 'source>, ValueKind)>,
) -> Result<Nav<'source>, EngineRunError> {
    // Payload-transparent in BOTH authorities: the located branch descends tag
    // layers through `located_view`, so the owned branch must see through
    // `Value::Tagged` or the two routes disagree about the same value.
    let located = match value {
        EngineResult::Owned(owned) => match owned.untagged() {
            Value::Null => {
                return mismatch::resolve_at(
                    scratch.resources(),
                    mismatch::MismatchCell::FieldOnNull,
                    suppressed,
                    Nav::Null,
                );
            }
            Value::Object(object) => {
                return match object.get(key) {
                    Some(child) => Ok(Nav::Value(EngineResult::owned(clone_owned(child)))),
                    None => mismatch::resolve_at(
                        scratch.resources(),
                        mismatch::MismatchCell::MissingKey,
                        suppressed,
                        Nav::Null,
                    ),
                };
            }
            other => {
                return Ok(Nav::Mismatch {
                    kind: owned_kind(other),
                    hint: None,
                });
            }
        },
        EngineResult::Located(located) => located,
    };
    let document = located.product().document();
    let (view, kind) = if let Some(pair) = payload {
        pair
    } else {
        let view = located_view(document, located.node()).map_err(internal_data)?;
        let kind = view.kind().map_err(internal_data)?;
        (view, kind)
    };
    match kind {
        ValueKind::Null => mismatch::resolve_at(
            scratch.resources(),
            mismatch::MismatchCell::FieldOnNull,
            suppressed,
            Nav::Null,
        ),
        ValueKind::Object => {
            let object = view
                .object()
                .map_err(internal_data)?
                .ok_or_else(|| internal("object node without an object projection"))?;
            match object.get(key) {
                Some(child) => located_child(located, document, child.node(), scratch),
                None => mismatch::resolve_at(
                    scratch.resources(),
                    mismatch::MismatchCell::MissingKey,
                    suppressed,
                    Nav::Null,
                ),
            }
        }
        other => Ok(Nav::Mismatch {
            kind: other,
            hint: None,
        }),
    }
}

/// The markup member step's decision.
#[derive(Debug)]
pub(crate) enum MarkupStep {
    /// The value is not a located markup ELEMENT array: use the ordinary
    /// navigation law (a plain array keeps the array-with-string mismatch).
    NotMarkup,
    /// No child matched: the ordinary hard mismatch, never a silent null.
    /// The hint names the correct accessor when the missed name exists as
    /// the element's own name or one of its attributes (`None` otherwise,
    /// so the mismatch message stays byte-identical to the reference's).
    NoMatch { hint: Option<alloc::string::String> },
    /// One or more children matched: fan out over the matched positions, the
    /// walk resuming after the key step (plural generator semantics for
    /// repeated elements; a single match is the one-child fan-out).
    Many(alloc::vec::Vec<usize>),
}

/// A member step `.name` over a MARKUP ELEMENT array navigates the
/// element's children by their element name — `.b` over the XML document
/// `<a><b>1</b><b>2</b></a>` (the value `[["1"],["2"]]`) selects both `b`
/// elements, plural, in order; `[0]` then composes over each selected
/// element's own array exactly as it would over any array. The law is
/// UNIFORM over every located array carrying the kernel element-name fact
/// (`name`, including a format-prefixed spelling that ends on that segment), so the
/// document wrapper is its own special case. A missing attribute fact or a
/// non-markup array keeps the ordinary mismatch; an owned value never has
/// markup facts.
/// The name facts are read through the finalize-time owner index when the
/// document has one — O(facts on the array's members), never a whole-arena
/// scan; the non-indexed fallback (a document built through the
/// non-cooperative finish path) scans the whole fact arena once per step, the
/// same fallback the accessor path keeps.
#[allow(
    clippy::too_many_lines,
    reason = "the cooperative fact-read refill arm sits beside the markup matching it serves"
)]
pub(crate) fn markup_member_step(
    value: &EngineResult<'_>,
    key: &str,
    scratch: &mut StepScratch<'_, '_>,
    payload: Option<(jqf_data::ValueView<'_, '_>, ValueKind)>,
) -> Result<MarkupStep, EngineRunError> {
    // The Key step already opened the payload-transparent view/kind; reuse
    // it. A non-array (the JSON-object hot path) returns before any fact
    // scan or a second handle resolve.
    let Some((view, kind)) = payload else {
        return Ok(MarkupStep::NotMarkup);
    };
    if kind != ValueKind::Array {
        return Ok(MarkupStep::NotMarkup);
    }
    let EngineResult::Located(located) = value else {
        return Ok(MarkupStep::NotMarkup);
    };
    let document = located.product().document();
    let node = located.node();
    let node_id = document
        .resolve_node_handle(node)
        .map_err(|_| internal("markup member node resolution"))?;
    let array = view
        .array()
        .map_err(internal_data)?
        .ok_or_else(|| internal("array node without an array projection"))?;
    // One fact pass learns whether THIS node is an element (it carries a
    // name fact) and every child element's name. The finalize-time owner
    // index names each node's own facts, so an indexed document reads the
    // element's own name and every member's name through it; a document
    // built through the non-cooperative finish path has no index and keeps
    // the arena reader.
    let mut names: alloc::vec::Vec<(jqf_data::NodeId, alloc::string::String)> = alloc::vec::Vec::new();
    let mut self_is_element = false;
    // The element's own name text (its `.@name` fact payload), kept for the
    // accessor hint: a member step `.key` that misses but equals the element's
    // OWN name means the user wanted `.@name` (the tag accessor) — but a
    // hint costs one string, so it is collected during the one pass the step
    // already pays.
    let mut self_name: Option<alloc::string::String> = None;
    if document.fact_owner_indexed() {
        for fact_id in document.owner_fact_ids(node_id) {
            let fact = document
                .fact(*fact_id)
                .map_err(|_| internal("indexed markup fact resolution"))?;
            if !accessor_matches_fact(fact.role().as_str(), "name") {
                continue;
            }
            let jqf_data::FactPayloadView::Text(text) = fact.payload() else {
                continue;
            };
            self_is_element = true;
            self_name = Some(alloc::string::String::from(text));
        }
        if self_is_element {
            for index in 0..array.len() {
                let Some(child) = array.get(index) else {
                    continue;
                };
                for fact_id in document.owner_fact_ids(child.node()) {
                    let fact = document
                        .fact(*fact_id)
                        .map_err(|_| internal("indexed markup fact resolution"))?;
                    if !accessor_matches_fact(fact.role().as_str(), "name") {
                        continue;
                    }
                    let jqf_data::FactPayloadView::Text(text) = fact.payload() else {
                        continue;
                    };
                    names.push((child.node(), alloc::string::String::from(text)));
                }
            }
        }
    } else {
        let mut reader = match document.fact_reader(scratch.resources_mut()) {
            Ok(reader) => reader,
            Err(jqf_data::DataError::CapabilityUnavailable {
                capability: jqf_data::DocumentCapability::AttachedFacts,
            }) => return Ok(MarkupStep::NotMarkup),
            Err(_) => {
                return Err(internal_data(jqf_data::DataError::CapabilityUnavailable {
                    capability: jqf_data::DocumentCapability::AttachedFacts,
                }));
            }
        };
        let limit = jqf_data::BatchLimit::new(usize::MAX).ok_or_else(|| internal("markup fact batch limit"))?;
        loop {
            let poll = reader
                .poll_batch(limit, scratch.resources_mut())
                .map_err(|_| internal("attached-fact read over a valid document"))?;
            match poll {
                jqf_data::ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        if !accessor_matches_fact(fact.role().as_str(), "name") {
                            continue;
                        }
                        let jqf_data::FactPayloadView::Text(text) = fact.payload() else {
                            continue;
                        };
                        let jqf_data::LocalOwnerRef::Node(owner) = fact.owner() else {
                            continue;
                        };
                        if owner == node_id {
                            self_is_element = true;
                            self_name = Some(alloc::string::String::from(text));
                        } else {
                            // Record every name fact; the child filter below keeps
                            // only the array's own members.
                            names.push((owner, alloc::string::String::from(text)));
                        }
                    }
                }
                jqf_data::ReaderPoll::Pending => {
                    // The cooperative entry's work credits are exhausted; refill
                    // so the scan can continue — spinning without refilling would
                    // loop forever once the per-entry budget is gone.
                    scratch
                        .resources_mut()
                        .try_begin_next_cooperative_entry(4096)
                        .map_err(|error| EngineRunError::Codec(CodecError::from(error)))?;
                }
                jqf_data::ReaderPoll::End(_) => break,
            }
        }
    }
    if !self_is_element {
        return Ok(MarkupStep::NotMarkup);
    }

    // Match the array's members by name. Text leaves carry no name fact and
    // never match; only element children do.
    let mut positions: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    for index in 0..array.len() {
        let Some(child) = array.get(index) else {
            continue;
        };
        if names.iter().any(|(owner, name)| *owner == child.node() && name == key) {
            positions.push(index);
        }
    }
    if positions.is_empty() {
        // The ordinary hard mismatch — but first, the accessor hint: when the
        // missed name equals this element's OWN name or the expanded name of
        // one of its attributes, the user almost certainly wanted the fact/
        // attribute accessors, which a member step can never reach (attributes
        // are facts, not children). The attribute check is a SECOND targeted
        // fact pass over the node's own facts, early-exiting on the first
        // match — only the error path pays it.
        let hint = markup_miss_hint(document, node_id, key, self_name.as_deref(), scratch.resources_mut())?;
        return Ok(MarkupStep::NoMatch { hint });
    }
    Ok(MarkupStep::Many(positions))
}

/// The accessor hint for a missed markup member step.
///
/// `key` is the missed member name. When it equals the element's OWN name
/// (the `.@name` fact payload), the hint names the tag accessor — except on
/// the DOCUMENT ROOT, where this miss is the projection seam and the hint
/// says the root element IS `key` (navigate children directly, or read the
/// name with `.@name`); when it equals an attribute's expanded name (a fact
/// with role `attribute`), the hint names the `.&` accessor. Both are
/// facts, never children, so no member step can reach them — the hint tells
/// the user which accessor can. `None` when nothing matches, so the message
/// stays byte-identical to the reference's.
/// The attribute pass is a second targeted fact read over the node's own
/// facts, early-exiting on the first match — only the error path pays it
/// (the member step's own scan already paid its pass).
fn markup_miss_hint(
    document: &jqf_data::Document<'_>,
    node_id: jqf_data::NodeId,
    key: &str,
    self_name: Option<&str>,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<alloc::string::String>, EngineRunError> {
    if self_name == Some(key) {
        // On the document ROOT this miss is the projection seam
        // (`json_facts`): the shown JSON wraps the root under its own key
        // (`{"feed": …}`), so a user descending from that shape writes
        // `.feed` — but the root IS the feed element and its own name is a
        // fact, never a child. The actionable hint names the real navigation
        // (children by name) and the accessor that reads the name itself. On
        // a NESTED element the same miss means the user wanted the element's
        // own name as a value, and the accessor is the whole answer.
        if document.root() == node_id {
            return Ok(Some(jqf_codec_core::markup::root_element_miss_hint(key)));
        }
        return Ok(Some(alloc::string::String::from(
            jqf_codec_core::markup::OWN_NAME_MISS_HINT,
        )));
    }
    // The `.&name` accessor law: role `attribute`, kind the expanded name.
    // The owner index names the node's own facts, so an indexed document
    // answers with a targeted read; the non-indexed fallback scans the arena.
    if document.fact_owner_indexed() {
        for fact_id in document.owner_fact_ids(node_id) {
            let fact = document
                .fact(*fact_id)
                .map_err(|_| internal("indexed markup fact resolution"))?;
            if fact.role().as_str() == jqf_codec_core::markup::ATTRIBUTE_FACT && fact.kind().as_str() == key {
                return Ok(Some(jqf_codec_core::markup::attribute_miss_hint(key)));
            }
        }
        return Ok(None);
    }
    let mut reader = match document.fact_reader(resources) {
        Ok(reader) => reader,
        Err(jqf_data::DataError::CapabilityUnavailable {
            capability: jqf_data::DocumentCapability::AttachedFacts,
        }) => return Ok(None),
        Err(_) => {
            return Err(internal_data(jqf_data::DataError::CapabilityUnavailable {
                capability: jqf_data::DocumentCapability::AttachedFacts,
            }));
        }
    };
    let limit = jqf_data::BatchLimit::new(usize::MAX).ok_or_else(|| internal("markup fact batch limit"))?;
    let owner = jqf_data::LocalOwnerRef::Node(node_id);
    loop {
        let poll = reader
            .poll_batch(limit, resources)
            .map_err(|_| internal("attached-fact read over a valid document"))?;
        match poll {
            jqf_data::ReaderPoll::Batch(batch) => {
                for fact in batch.iter() {
                    // The `.&name` accessor law: role `attribute`, kind the
                    // expanded name.
                    if fact.owner() == owner
                        && fact.role().as_str() == jqf_codec_core::markup::ATTRIBUTE_FACT
                        && fact.kind().as_str() == key
                    {
                        return Ok(Some(jqf_codec_core::markup::attribute_miss_hint(key)));
                    }
                }
            }
            jqf_data::ReaderPoll::Pending => {
                // The cooperative entry's work credits are exhausted; refill
                // so the scan can continue — spinning without refilling would
                // loop forever once the per-entry budget is gone.
                resources
                    .try_begin_next_cooperative_entry(4096)
                    .map_err(|error| EngineRunError::Codec(CodecError::from(error)))?;
            }
            jqf_data::ReaderPoll::End(_) => break,
        }
    }
    Ok(None)
}

/// Navigates one signed array index over `value` (null precedence, out of
/// bounds → null, negative-index wraparound). A located array resolves to a
/// fresh located child; an owned array clones its child out.
fn navigate_index<'source>(
    value: &EngineResult<'source>,
    index: i64,
    suppressed: bool,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Nav<'source>, EngineRunError> {
    // Payload-transparent in both authorities, for `navigate_key`'s reason.
    let located = match value {
        EngineResult::Owned(owned) => match owned.untagged() {
            Value::Null => {
                return mismatch::resolve_at(
                    scratch.resources(),
                    mismatch::MismatchCell::IndexOrSliceOnNull,
                    suppressed,
                    Nav::Null,
                );
            }
            Value::Array(array) => {
                return match jqf_data::resolve_index(array.len(), index).and_then(|position| array.get(position)) {
                    Some(child) => Ok(Nav::Value(EngineResult::owned(clone_owned(child)))),
                    None => mismatch::resolve_at(
                        scratch.resources(),
                        mismatch::MismatchCell::IndexOutOfRange,
                        suppressed,
                        Nav::Null,
                    ),
                };
            }
            other => {
                return Ok(Nav::Mismatch {
                    kind: owned_kind(other),
                    hint: None,
                });
            }
        },
        EngineResult::Located(located) => located,
    };
    let document = located.product().document();
    let view = located_view(document, located.node()).map_err(internal_data)?;
    match view.kind().map_err(internal_data)? {
        ValueKind::Null => mismatch::resolve_at(
            scratch.resources(),
            mismatch::MismatchCell::IndexOrSliceOnNull,
            suppressed,
            Nav::Null,
        ),
        ValueKind::Array => {
            let array = view
                .array()
                .map_err(internal_data)?
                .ok_or_else(|| internal("array node without an array projection"))?;
            match jqf_data::resolve_index(array.len(), index) {
                Some(position) => match array.get(position) {
                    Some(child) => located_child(located, document, child.node(), scratch),
                    None => mismatch::resolve_at(
                        scratch.resources(),
                        mismatch::MismatchCell::IndexOutOfRange,
                        suppressed,
                        Nav::Null,
                    ),
                },
                None => mismatch::resolve_at(
                    scratch.resources(),
                    mismatch::MismatchCell::IndexOutOfRange,
                    suppressed,
                    Nav::Null,
                ),
            }
        }
        other => Ok(Nav::Mismatch {
            kind: other,
            hint: None,
        }),
    }
}

/// Materializes the `cursor`-th child of a located container as a fresh located
/// value: arrays in position order, objects in insertion order (values only).
///
/// Returns `None` once the cursor passes the last child, exhausting the frame.
///
/// # Errors
///
/// Returns [`EngineRunError::Codec`] for an internal navigation contract
/// violation, or when retaining the per-child located product against the ledger
/// fails.
pub(crate) fn next_child<'source>(
    container: &Container<'source>,
    cursor: usize,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Option<EngineResult<'source>>, EngineRunError> {
    match container {
        Container::Located(located) => next_located_child(located, cursor, scratch),
        Container::LocatedFiltered(located, positions) => {
            let Some(position) = positions.get(cursor) else {
                return Ok(None);
            };
            next_located_child(located, *position, scratch)
        }
        Container::Owned(owned) => next_owned_child(owned, cursor),
    }
}

/// Materializes the `cursor`-th child of a LOCATED container as a fresh located
/// value.
fn next_located_child<'source>(
    container: &LocatedProduct<'source>,
    cursor: usize,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Option<EngineResult<'source>>, EngineRunError> {
    let product = container.product();
    let document = product.document();
    let view = located_view(document, container.node()).map_err(internal_data)?;
    let child_node = match view.kind().map_err(internal_data)? {
        ValueKind::Array => {
            let array = view
                .array()
                .map_err(internal_data)?
                .ok_or_else(|| internal("array node without an array projection"))?;
            match array.get(cursor) {
                Some(child) => child.node(),
                None => return Ok(None),
            }
        }
        ValueKind::Object => {
            let object = view
                .object()
                .map_err(internal_data)?
                .ok_or_else(|| internal("object node without an object projection"))?;
            match object.get_index(cursor).map_err(internal_data)? {
                Some(entry) => entry.value().node(),
                None => return Ok(None),
            }
        }
        _ => return Err(internal("engine frame over a non-container value")),
    };
    Ok(Some(retain_or_materialize(product, document, child_node, scratch)?))
}

/// Clones the `cursor`-th child of an OWNED container out as an owned value:
/// arrays in position order, objects in insertion order (values only).
fn next_owned_child<'source>(
    container: &Value,
    cursor: usize,
) -> Result<Option<EngineResult<'source>>, EngineRunError> {
    // The frame's kind is payload-transparent (`owned_kind` untags), so the
    // CONTAINER here may be a `Value::Tagged` around an array/object. Untag
    // for iteration — a `.[]`/`..` over a tagged container yields the
    // children, never the wrapper (the tag applies to the container, and a
    // child of a tagged collection is an ordinary value).
    let untagged = container.untagged();
    let child = match untagged {
        Value::Array(array) => array.get(cursor),
        Value::Object(object) => object.get_index(cursor).map(jqf_data::ObjectEntry::value),
        _ => return Err(internal("engine frame over a non-container owned value")),
    };
    match child {
        Some(child) => Ok(Some(EngineResult::owned(clone_owned(child)))),
        None => Ok(None),
    }
}

/// The child count of an engine value, or zero for anything else — the
/// resumable path walk's container test over both representations.
#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "the view projections borrow, so the method item cannot match the by-value Option map"
)]
pub(crate) fn engine_container_len(value: &EngineResult<'_>) -> Result<usize, EngineRunError> {
    match value {
        EngineResult::Located(located) => {
            let document = located.product().document();
            let view = located_view(document, located.node()).map_err(internal_data)?;
            match view.kind().map_err(internal_data)? {
                ValueKind::Array => Ok(view.array().map_err(internal_data)?.map_or(0, |route| route.len())),
                ValueKind::Object => Ok(view.object().map_err(internal_data)?.map_or(0, |entries| entries.len())),
                _ => Ok(0),
            }
        }
        EngineResult::Owned(value) => match value.untagged() {
            Value::Array(array) => Ok(array.len()),
            Value::Object(object) => Ok(object.len()),
            _ => Ok(0),
        },
    }
}

/// The `cursor`-th child of a LOCATED container, with the register step that
/// reaches it — the resumable path walk's located half (the owned half is
/// path mode's `child_at`).
pub(crate) fn located_walk_child<'source>(
    container: &LocatedProduct<'source>,
    cursor: usize,
    keys: &mut jqf_builtins::semantics::path::KeyIntern,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Option<(jqf_builtins::semantics::path::PathStep, EngineResult<'source>)>, EngineRunError> {
    let product = container.product();
    let document = product.document();
    let view = located_view(document, container.node()).map_err(internal_data)?;
    let (step, child_node) = match view.kind().map_err(internal_data)? {
        ValueKind::Array => {
            let array = view
                .array()
                .map_err(internal_data)?
                .ok_or_else(|| internal("array node without an array projection"))?;
            let Some(child) = array.get(cursor) else {
                return Ok(None);
            };
            let position = i64::try_from(cursor).map_err(|_| EngineRunError::allocation_failure())?;
            (jqf_builtins::semantics::path::PathStep::Index(position), child.node())
        }
        ValueKind::Object => {
            let object = view
                .object()
                .map_err(internal_data)?
                .ok_or_else(|| internal("object node without an object projection"))?;
            let Some(entry) = object.get_index(cursor).map_err(internal_data)? else {
                return Ok(None);
            };
            let step = jqf_builtins::semantics::path::PathStep::Key(keys.intern(entry.key(), scratch.resources())?);
            (step, entry.value().node())
        }
        _ => return Err(internal("engine frame over a non-container value")),
    };
    let child = retain_or_materialize(product, document, child_node, scratch)?;
    Ok(Some((step, child)))
}

/// Resolves one FACT-ASSIGNMENT value path against a located
/// document, returning the value path AND the addressed node.
///
/// The fact target is a key/index chain: static
/// [`StepAccess::Key`] / [`StepAccess::Index`] steps and runtime-resolved
/// [`StepAccess::DynVar`] slots, whose bound value dispatches on its TYPE — a
/// string reads an object key (or, over a MARKUP ELEMENT array, a child by
/// its name fact — the write twin of the read Navigator's member step), a
/// number an array position (truncation and negative wrap exactly as the
/// read-side `DynVar` law), anything else the index-class rejection. A missing
/// member or an out-of-range index is
/// `Ok(None)` — the fact write has no value to create (the assignment operator
/// would create the path; a fact has no owned value to fabricate). A TYPE
/// mismatch at a step raises the index-class message, exactly as the
/// assignment law does (`5 | .a = 1` errors).
pub(crate) fn walk_fact_path(
    nodes: &[ProgramNode],
    paths: ProgramNodeId,
    document: &jqf_data::Document<'_>,
    start: NodeHandle,
    env: &[EngineResult<'_>],
    resources: &mut ResourceContext<'_>,
) -> Result<Option<(Value, NodeId)>, EngineRunError> {
    let ProgramNode::Stage { steps, .. } = &nodes[paths.index()] else {
        return Err(internal("fact assignment path is not a stage"));
    };
    let mut node = start;
    let mut path: Vec<Value> = Vec::new();
    path.try_reserve(steps.len())
        .map_err(|_| EngineRunError::allocation_failure())?;
    for step in steps {
        // One dynamic operand resolves to the SAME shape the static arms walk;
        // an owned `Cow` keeps a resolved key alive for the arms below.
        let mut index: Option<i64> = None;
        let key: Option<alloc::borrow::Cow<'_, str>> = match step.access() {
            StepAccess::Key(key) => Some(alloc::borrow::Cow::Borrowed(key.as_str())),
            StepAccess::Index(resolved) => {
                index = Some(*resolved);
                None
            }
            StepAccess::DynVar(slot) => match fact_path_operand(env, *slot)? {
                FactPathOperand::Key(key) => Some(alloc::borrow::Cow::Owned(key)),
                FactPathOperand::Index(resolved) => {
                    index = Some(resolved);
                    None
                }
                // A magnitude no i64 carries: no member can sit there, so
                // this is the missing-path outcome, never a raise.
                FactPathOperand::OutOfRange => return Ok(None),
                FactPathOperand::Mismatch(rendered) => {
                    let kind = located_view(document, node)
                        .map_err(internal_data)?
                        .kind()
                        .map_err(internal_data)?;
                    return Err(jqf_builtins::semantics::path::raise(
                        &message::index_message(kind, &rendered)?,
                        resources,
                    ));
                }
            },
            _ => {
                return Err(internal("fact assignment path step is not a key/index"));
            }
        };
        let view = located_view(document, node).map_err(internal_data)?;
        if let Some(key) = key.as_deref() {
            let kind = view.kind().map_err(internal_data)?;
            let target = if kind == ValueKind::Object {
                let object = view
                    .object()
                    .map_err(internal_data)?
                    .ok_or_else(|| internal("fact path object view"))?;
                match object.get(key) {
                    Some(child) => KeyTarget::Found(child.node()),
                    None => KeyTarget::Missing,
                }
            } else {
                // A MARKUP ELEMENT's located payload is its children array, so
                // a key step over one navigates a child by NAME FACT — the
                // write twin of the read Navigator's markup member step. An
                // unmatched name keeps the ordinary index-class mismatch; a
                // PLURAL match raises it too, because a single-target write
                // over several same-named children would otherwise publish a
                // silent partial edit (`[n]` disambiguates).
                let node_id = document
                    .resolve_node_handle(node)
                    .map_err(|_| internal("fact path node resolution"))?;
                match fact_path_element_child(document, node_id, view, key, resources)? {
                    ElementChild::Matched(child_node) => KeyTarget::Found(child_node),
                    ElementChild::Plural | ElementChild::Unmatched => {
                        return Err(jqf_builtins::semantics::path::raise(
                            &message::index_message(kind, &message::render_object_key(key)?)?,
                            resources,
                        ));
                    }
                }
            };
            let KeyTarget::Found(child_node) = target else {
                return Ok(None);
            };
            path.push(Value::try_string(key).map_err(|_| EngineRunError::allocation_failure())?);
            node = document.node_handle(child_node).map_err(internal_data)?;
        } else if let Some(index) = index {
            let kind = view.kind().map_err(internal_data)?;
            if kind != ValueKind::Array {
                return Err(jqf_builtins::semantics::path::raise(
                    &message::index_message(kind, &message::render_array_key(index)?)?,
                    resources,
                ));
            }
            let array = view
                .array()
                .map_err(internal_data)?
                .ok_or_else(|| internal("fact path array view"))?;
            let Some(child) = jqf_data::resolve_index(array.len(), index).and_then(|position| array.get(position))
            else {
                return Ok(None);
            };
            path.push(Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(
                index,
            ))));
            node = document.node_handle(child.node()).map_err(internal_data)?;
        } else {
            return Err(internal("fact path step resolved neither key nor index"));
        }
    }
    Ok(Some((
        Value::Array(Array::try_from_vec(path).map_err(|_| EngineRunError::allocation_failure())?),
        document
            .resolve_node_handle(node)
            .map_err(|_| internal("fact path node resolution"))?,
    )))
}

/// Resolves one MARKUP ELEMENT child of `node_id` by NAME for a
/// fact-assignment value path — the write twin of the read Navigator's
/// [`markup_member_step`]. A markup element's located payload is its children
/// array, so a key step over an element resolves a child by NAME FACT, never
/// by object key.
///
/// [`ElementChild::Plural`] reports several same-named children: the READ
/// side fans out over every match, but one fact write addresses ONE target,
/// and writing only the first would publish a silent partial edit. The caller
/// raises the index-class mismatch so the request refuses loudly instead;
/// `[n]` disambiguates. [`ElementChild::Unmatched`] covers a non-element (a
/// node with no name fact) and an unmatched name, keeping the ordinary
/// array-with-string mismatch byte-identical for plain arrays.
///
/// The name facts are read through the finalize-time owner index when the
/// document has one; otherwise the attached-fact arena reader scans once,
/// refilling the cooperative entry when its budget is exhausted (the same
/// fallback [`markup_member_step`] keeps).
enum ElementChild {
    Matched(NodeId),
    Plural,
    Unmatched,
}

/// One key step's resolution over a fact-assignment value path.
enum KeyTarget {
    /// The member exists; its node id.
    Found(NodeId),
    /// A missing OBJECT member: the no-value outcome (the assignment
    /// operator would create the path; a fact has no owned value to
    /// fabricate).
    Missing,
}

fn fact_path_element_child(
    document: &jqf_data::Document<'_>,
    node_id: NodeId,
    view: jqf_data::ValueView<'_, '_>,
    key: &str,
    resources: &mut ResourceContext<'_>,
) -> Result<ElementChild, EngineRunError> {
    let kind = view.kind().map_err(internal_data)?;
    if kind != ValueKind::Array {
        return Ok(ElementChild::Unmatched);
    }
    let array = view
        .array()
        .map_err(internal_data)?
        .ok_or_else(|| internal("array node without an array projection"))?;
    let Some(names) = element_name_facts(document, node_id, &array, resources)? else {
        return Ok(ElementChild::Unmatched);
    };
    let mut matched: Option<NodeId> = None;
    for index in 0..array.len() {
        let Some(child) = array.get(index) else {
            continue;
        };
        if names.iter().any(|(owner, name)| *owner == child.node() && name == key) {
            if matched.is_some() {
                return Ok(ElementChild::Plural);
            }
            matched = Some(child.node());
        }
    }
    Ok(matched.map_or(ElementChild::Unmatched, ElementChild::Matched))
}

/// Collects the NAME FACTS of `node_id`'s array members, alongside whether
/// `node_id` is itself a markup element (it carries its own name fact).
/// `None` when `node_id` is not an element — the caller keeps the ordinary
/// mismatch. The finalize-time owner index names each node's own facts, so
/// an indexed document reads every name through it; a document built through
/// the non-cooperative finish path scans the whole fact arena once (the same
/// fallback [`markup_member_step`] keeps), refilling the cooperative entry
/// when its budget is exhausted.
fn element_name_facts(
    document: &jqf_data::Document<'_>,
    node_id: NodeId,
    array: &jqf_data::ArrayView<'_, '_>,
    resources: &mut ResourceContext<'_>,
) -> Result<Option<Vec<(NodeId, String)>>, EngineRunError> {
    let mut self_is_element = false;
    let mut names: Vec<(NodeId, String)> = Vec::new();
    if document.fact_owner_indexed() {
        for fact_id in document.owner_fact_ids(node_id) {
            let fact = document
                .fact(*fact_id)
                .map_err(|_| internal("indexed markup fact resolution"))?;
            if !accessor_matches_fact(fact.role().as_str(), "name") {
                continue;
            }
            let jqf_data::FactPayloadView::Text(_) = fact.payload() else {
                continue;
            };
            self_is_element = true;
        }
        if !self_is_element {
            return Ok(None);
        }
        for index in 0..array.len() {
            let Some(child) = array.get(index) else {
                continue;
            };
            for fact_id in document.owner_fact_ids(child.node()) {
                let fact = document
                    .fact(*fact_id)
                    .map_err(|_| internal("indexed markup fact resolution"))?;
                if !accessor_matches_fact(fact.role().as_str(), "name") {
                    continue;
                }
                let jqf_data::FactPayloadView::Text(text) = fact.payload() else {
                    continue;
                };
                names.push((child.node(), String::from(text)));
            }
        }
    } else {
        let mut reader = match document.fact_reader(resources) {
            Ok(reader) => reader,
            // A document that does not retain attached facts carries no
            // element names: not a markup element, the ordinary mismatch
            // stands.
            Err(jqf_data::DataError::CapabilityUnavailable {
                capability: jqf_data::DocumentCapability::AttachedFacts,
            }) => return Ok(None),
            Err(_) => {
                return Err(internal_data(jqf_data::DataError::CapabilityUnavailable {
                    capability: jqf_data::DocumentCapability::AttachedFacts,
                }));
            }
        };
        let limit = jqf_data::BatchLimit::new(usize::MAX).ok_or_else(|| internal("fact batch limit"))?;
        loop {
            let poll = reader
                .poll_batch(limit, resources)
                .map_err(|_| internal("attached-fact read over a valid document"))?;
            match poll {
                jqf_data::ReaderPoll::Batch(batch) => {
                    for fact in batch.iter() {
                        if !accessor_matches_fact(fact.role().as_str(), "name") {
                            continue;
                        }
                        let jqf_data::FactPayloadView::Text(text) = fact.payload() else {
                            continue;
                        };
                        let jqf_data::LocalOwnerRef::Node(owner) = fact.owner() else {
                            continue;
                        };
                        if owner == node_id {
                            self_is_element = true;
                        } else {
                            // Record every name fact; the match below keeps
                            // only this array's own members.
                            names.push((owner, String::from(text)));
                        }
                    }
                }
                jqf_data::ReaderPoll::Pending => {
                    // The cooperative entry's work credits are exhausted;
                    // refill so the scan can continue — spinning without
                    // refilling would loop forever once the per-entry budget
                    // is gone.
                    resources
                        .try_begin_next_cooperative_entry(4096)
                        .map_err(|error| EngineRunError::Codec(CodecError::from(error)))?;
                }
                jqf_data::ReaderPoll::End(_) => break,
            }
        }
        if !self_is_element {
            return Ok(None);
        }
    }
    Ok(Some(names))
}

/// One `.[$x]` operand of a fact-assignment value path, resolved to the step
/// kind the static walk understands. The type dispatch is the read-side `DynVar`
/// law: string → key, number → position (truncate toward zero, wrap negatives),
/// everything else the mismatch class.
enum FactPathOperand {
    Key(String),
    Index(i64),
    /// A magnitude no i64 carries: treated as the missing-path outcome.
    OutOfRange,
    /// Any other bound type, pre-rendered for a diagnostic.
    Mismatch(String),
}

fn fact_path_operand(env: &[EngineResult<'_>], slot: u32) -> Result<FactPathOperand, EngineRunError> {
    let number_operand = |raw: f64| {
        let truncated = jqf_builtins::semantics::arith::trunc_toward_zero(raw);
        if !truncated.is_finite() || truncated.abs() >= 9_223_372_036_854_775_808.0 {
            return FactPathOperand::OutOfRange;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the magnitude bound above keeps the truncated value inside i64"
        )]
        FactPathOperand::Index(truncated as i64)
    };
    match slot_result(env, slot)? {
        EngineResult::Owned(value) => match value.untagged() {
            Value::String(key) => Ok(FactPathOperand::Key(String::from(key.as_str()))),
            Value::Number(number) => Ok(number_operand(jqf_builtins::semantics::order::to_f64(number))),
            other => Ok(FactPathOperand::Mismatch(message::dump_trunc_owned(other)?)),
        },
        EngineResult::Located(located) => {
            let document = located.product().document();
            let view = located_view(document, located.node()).map_err(internal_data)?;
            match view.kind().map_err(internal_data)? {
                ValueKind::String => {
                    let Some(ScalarView::String(key)) = view.scalar().map_err(internal_data)? else {
                        return Err(internal("a located string kind without a string scalar"));
                    };
                    Ok(FactPathOperand::Key(String::from(key)))
                }
                ValueKind::Number => {
                    let Some(ScalarView::Number(number)) = view.scalar().map_err(internal_data)? else {
                        return Err(internal("a located number kind without a number scalar"));
                    };
                    Ok(number_operand(jqf_builtins::semantics::order::view_to_f64(number)))
                }
                _ => {
                    let owned = message::dump_trunc_view(&view)?;
                    Ok(FactPathOperand::Mismatch(owned))
                }
            }
        }
    }
}

/// Retains one located child by node id in the same authoritative document.
fn located_child<'source>(
    parent: &LocatedProduct<'source>,
    document: &jqf_data::Document<'source>,
    child: jqf_data::NodeId,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Nav<'source>, EngineRunError> {
    Ok(Nav::Value(retain_or_materialize(
        parent.product(),
        document,
        child,
        scratch,
    )?))
}

/// Produces one child of a located container as an engine value.
///
/// An ordinary child is RETAINED — a second handle on the same authoritative
/// document, copying no bytes, which is the located walk's whole point. A child
/// the decode deferred to a container span carries no occurrences to navigate,
/// so it is materialized here instead and enters the engine OWNED.
///
/// The materialization is the engine's existing one
/// ([`StepScratch::materialize`]): the same reusable cycle-detection workspace,
/// the same Working residency for the produced bytes. Nothing is cached on the
/// document — the toucher receives the only handle on its allocation, which is
/// what keeps a subsequent `.a += $x` a vacating update rather than a copy.
pub(crate) fn retain_or_materialize<'source>(
    product: &jqf_codec_core::DocumentProduct<'source>,
    document: &jqf_data::Document<'source>,
    child: jqf_data::NodeId,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<EngineResult<'source>, EngineRunError> {
    let handle = document.node_handle(child).map_err(internal_data)?;
    if let Some(owned) = materialize_if_span(document, handle, scratch)? {
        return Ok(EngineResult::owned(owned));
    }
    let child = LocatedProduct::try_new(product, handle).map_err(EngineRunError::Codec)?;
    Ok(EngineResult::Located(child))
}

/// Materializes one located node to an owned value WHEN the decode deferred it
/// to a container span; an ordinary eager node is left untouched (`None`).
///
/// The decode's container-span frontier leaves a touched subtree as a span of
/// the validated source rather than built occurrences. `retain_or_materialize`
/// applies this to each located CHILD as it enters the engine; a codec that
/// defers the document ROOT to one span (the CBOR whole-document route's
/// source-retention mode) needs the same law at the top of the stage walk
/// ([`descend`]), or navigation reads `DataError::UnmaterializedContainerSpan`
/// on the value it is about to walk. Both are the same "materialize on touch"
/// model, applied at two entry points.
pub(crate) fn materialize_if_span(
    document: &jqf_data::Document<'_>,
    node: jqf_data::NodeHandle,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Option<Value>, EngineRunError> {
    // An eager document has no spans at all, so the default route pays one
    // integer compare per child and never opens the view a second time.
    if document.container_span_count() > 0
        && document
            .value_view(node)
            .map_err(internal_data)?
            .is_container_span()
            .map_err(internal_data)?
    {
        return Ok(Some(scratch.materialize(document, node)?));
    }
    Ok(None)
}

/// The payload-transparent semantic category of a located value.
fn located_kind(located: &LocatedProduct<'_>) -> Result<ValueKind, EngineRunError> {
    located_view(located.product().document(), located.node())
        .and_then(jqf_data::ValueView::kind)
        .map_err(internal_data)
}

/// The payload-transparent value view of a located node: tag layers are
/// descended to their single payload, so the navigation steps see through the
/// wrapper (the binary-tag vertical's tag-layer law).
pub(crate) fn located_view<'d, 's>(
    document: &'d Document<'s>,
    handle: jqf_data::NodeHandle,
) -> Result<jqf_data::ValueView<'d, 's>, DataError> {
    // The one descent implementation lives on the document
    // ([`Document::payload_view`], next to [`Document::value_view`]); every
    // engine copy delegates to it so the tag-layer law cannot drift between
    // readers.
    document.payload_view(handle)
}

/// Answers one `.@`/`.&` accessor over `value` as an OWNED value.
///
/// This is the accessor's total law, shared by the step walk and path mode:
///
/// - the `tag` selector reads the input's intrinsic tag — a located node's
///   intrinsic tag, or an owned `Value::Tagged`'s wrapper — answering `null`
///   when there is none;
/// - every other `.@name` selector and every `.&name` selector read ATTACHED
///   FACTS, which only a located document node carries. An owned value has no
///   document and therefore answers `null`, the standing spelling for an
///   absent fact.
///
/// The step never produces a mismatch: whatever the input's kind, the accessor
/// answers one value (the fact, or `null`). That is what makes it a
/// single-output step with no error path, like `Descend`.
fn accessor_answer(
    value: &EngineResult<'_>,
    selector: &str,
    attribute: bool,
    scratch: &mut StepScratch<'_, '_>,
) -> Result<Value, EngineRunError> {
    match value {
        EngineResult::Owned(owned) => accessor_owned(owned, selector, attribute),
        EngineResult::Located(located) => {
            let document = located.product().document();
            let node = located.node();
            if !attribute && selector == "tag" {
                // A tag write recorded earlier in this run overlays the
                // intrinsic tag (last-write-wins), exactly as every sibling
                // role overlays through `read_attached_fact` — without this
                // consult, `.p.@tag = "!m" | .p.@tag` would answer the
                // pre-write value.
                let node_id = document
                    .resolve_node_handle(node)
                    .map_err(|_| internal("accessor node resolution"))?;
                if let Some(payload) = scratch.tag_delta(node_id) {
                    return Ok(overlay_read_payload(payload));
                }
                return match document
                    .value_view(node)
                    .map_err(internal_data)?
                    .tag()
                    .map_err(internal_data)?
                {
                    Some(tag) => Value::try_string(tag.as_str()).map_err(|_| EngineRunError::allocation_failure()),
                    None => Ok(Value::Null),
                };
            }
            scratch.read_attached_fact(document, node, selector, attribute)
        }
    }
}

/// The OWNED half of [`accessor_answer`]: an owned value's only fact is the
/// `Value::Tagged` wrapper behind the `tag` selector; every attached-fact
/// selector answers `null` because an owned value carries no document.
pub(crate) fn accessor_owned(value: &Value, selector: &str, attribute: bool) -> Result<Value, EngineRunError> {
    if !attribute && selector == "tag" {
        return match value {
            Value::Tagged { tag, .. } => {
                Value::try_string(tag.as_str()).map_err(|_| EngineRunError::allocation_failure())
            }
            _ => Ok(Value::Null),
        };
    }
    Ok(Value::Null)
}

fn internal(contract: &'static str) -> EngineRunError {
    EngineRunError::Codec(CodecError::new(CodecFailureKind::InternalContractViolation {
        contract,
    }))
}

fn internal_data(_: DataError) -> EngineRunError {
    internal("engine navigation over a valid located document failed")
}

#[cfg(test)]
mod tests {
    use super::{Bound, Container, DescendStep, Descended, Sliced, descend, descend_step, slice_owned, slice_span};
    use crate::exec::{EngineRunError, StepScratch};
    use crate::program::{SliceBound, SliceBounds, StageStep, StepAccess};
    use alloc::boxed::Box;
    use alloc::string::String;
    use alloc::vec;
    use jqf_builtins::codec_result::EngineResult;
    use jqf_builtins::error::mismatch::MismatchCell;
    use jqf_builtins::semantics::arith::{ceil_binary64, floor_binary64};
    use jqf_codec_core::{DocumentProduct, LocatedProduct};
    use jqf_data::{
        AccountedDocumentBuilder, AccountedIntrinsicTag, AccountedOccurrenceKey, AccountedSemanticNode, Array,
        FactPayload, LocalOwnerRef, MaterializeWorkspace, NodeHandle, Number, Object, ObjectKey, Value, ValueKind,
    };
    use jqf_resource::policy::MismatchPolicy;
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    /// The per-run scratch the step walk needs. Only a `Slice` over a LOCATED
    /// container ever touches it, so these owned-value tests never grow it.
    struct Scratch {
        workspace: Option<MaterializeWorkspace>,
        resources: ResourceContext<'static>,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                workspace: None,
                resources: ResourceContext::new(
                    RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                        .expect("account"),
                    &CONTROL,
                    WorkMeter::try_new_v1(4_096).expect("work"),
                )
                .expect("resources"),
            }
        }

        fn borrow(&mut self) -> StepScratch<'_, 'static> {
            StepScratch::new(&mut self.workspace, &mut self.resources)
        }
    }

    /// One unlimited request ledger for a fixture value. An owned payload is
    /// charged at its own construction, so a fixture cannot be built without one.
    fn ledger() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    fn text(value: &str) -> Value {
        Value::try_string(value).expect("fixture string")
    }

    fn key(name: &str, optional: bool) -> StageStep {
        StageStep::new(StepAccess::Key(String::from(name)), optional)
    }

    fn each(optional: bool) -> StageStep {
        StageStep::new(StepAccess::Each, optional)
    }

    fn descent(optional: bool) -> StageStep {
        StageStep::new(StepAccess::Descend, optional)
    }

    fn number(text: &str) -> Value {
        Value::Number(Number::try_json_literal(text).expect("numeric literal"))
    }

    fn array(len: usize) -> Value {
        let _resources = ledger();
        let mut out = Array::try_new().expect("array");
        for index in 0..len {
            out.try_push(number(&alloc::format!("{index}"))).expect("push");
        }
        Value::Array(out)
    }

    fn bounds(start: SliceBound, end: SliceBound) -> SliceBounds {
        SliceBounds { start, end }
    }

    /// The element range `.[start:end]` selects from a container of `len`.
    fn span(start: SliceBound, end: SliceBound, len: usize) -> Option<(usize, usize)> {
        slice_span(&SliceBounds { start, end }, &[], len, false, &ledger()).expect("bounds resolve")
    }

    fn literal(text: &str) -> SliceBound {
        SliceBound::Literal(number(text))
    }

    fn accessor(name: &str, optional: bool) -> StageStep {
        StageStep::new(StepAccess::NodeAccessor(String::from(name)), optional)
    }

    fn attribute(name: &str, optional: bool) -> StageStep {
        StageStep::new(StepAccess::Attribute(String::from(name)), optional)
    }

    fn tagged(text: &str) -> Value {
        Value::try_tagged(jqf_data::TagId::try_new_unaccounted(text).expect("tag"), Value::Null)
            .expect("tagged fixture")
    }

    /// Builds a located document whose root is a tagged or untagged null node,
    /// with `attach` adding any attached facts against the SAME ledger the
    /// builder uses, returned together with its authoritative product and the
    /// root handle.
    fn located_document(
        tag: Option<&str>,
        attach: impl FnOnce(&mut AccountedDocumentBuilder<'static>, jqf_data::NodeId, &ResourceContext<'static>),
    ) -> (DocumentProduct<'static>, NodeHandle) {
        let resources = ledger();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let root = builder
            .add_node(
                "test.value",
                AccountedSemanticNode::Null,
                tag.map(AccountedIntrinsicTag::Tagged),
                &resources,
            )
            .expect("root");
        attach(&mut builder, root, &resources);
        let document = builder.finish(root, &resources).expect("document");
        let product = DocumentProduct::try_new(document, &resources).expect("product");
        let handle = product.document().root_handle();
        (product, handle)
    }

    /// Applies one stage of steps to a LOCATED input and returns the owned leaf.
    fn located_leaf(product: &DocumentProduct<'static>, handle: NodeHandle, steps: &[StageStep]) -> Value {
        let located = LocatedProduct::try_new(product, handle).expect("located");
        let mut scratch = Scratch::new();
        let outcome =
            descend(steps, 0, &[], EngineResult::Located(located), 0, &mut scratch.borrow()).expect("accessor leaf");
        match outcome {
            Descended::Leaf(EngineResult::Owned(value)) => value,
            other => panic!("expected an owned accessor leaf, got {other:?}"),
        }
    }

    /// Applies one stage of steps to an OWNED input and returns the leaf.
    fn owned_leaf(value: Value, steps: &[StageStep]) -> Value {
        let mut scratch = Scratch::new();
        let outcome =
            descend(steps, 0, &[], EngineResult::owned(value), 0, &mut scratch.borrow()).expect("owned accessor leaf");
        match outcome {
            Descended::Leaf(EngineResult::Owned(value)) => value,
            other => panic!("expected an owned accessor leaf, got {other:?}"),
        }
    }

    fn string_value(value: &Value) -> &str {
        match value {
            Value::String(text) => text.as_str(),
            other => panic!("expected a string accessor leaf, got {other:?}"),
        }
    }

    #[test]
    fn empty_residual_forwards_the_input_as_a_leaf() {
        let mut scratch = Scratch::new();
        let outcome = descend(
            &[],
            0,
            &[],
            EngineResult::owned(Value::Bool(true)),
            0,
            &mut scratch.borrow(),
        )
        .expect("identity leaf");
        match outcome {
            Descended::Leaf(EngineResult::Owned(Value::Bool(value))) => assert!(value),
            other => panic!("identity must forward its input, got {other:?}"),
        }
    }

    #[test]
    fn key_into_owned_null_yields_null_and_continues() {
        // `.b` over an owned null (a pushed-down missing path or an earlier
        // null-index) is null precedence: the leaf is null, no error.
        let mut scratch = Scratch::new();
        let outcome = descend(
            &[key("b", false)],
            0,
            &[],
            EngineResult::owned(Value::Null),
            0,
            &mut scratch.borrow(),
        )
        .expect("null leaf");
        assert!(matches!(outcome, Descended::Leaf(EngineResult::Owned(Value::Null))));
    }

    #[test]
    fn each_over_owned_null_is_an_iterate_mismatch() {
        // `.[]` over null is the distinct iterate class ("Cannot iterate over
        // null"), reported at the global step index (base offset preserved).
        let mut scratch = Scratch::new();
        let error = descend(
            &[each(false)],
            3,
            &[],
            EngineResult::owned(Value::Null),
            0,
            &mut scratch.borrow(),
        )
        .expect_err("iterating null must fail");
        assert!(matches!(
            error,
            EngineRunError::IterateMismatch {
                step_index: 3,
                actual_type: ValueKind::Null,
                ..
            }
        ));
    }

    #[test]
    fn optional_each_over_owned_null_skips() {
        // `.[]?` over null suppresses: this value produces nothing.
        let mut scratch = Scratch::new();
        let outcome = descend(
            &[each(true)],
            0,
            &[],
            EngineResult::owned(Value::Null),
            0,
            &mut scratch.borrow(),
        )
        .expect("optional iterate suppresses");
        assert!(matches!(outcome, Descended::Skip));
    }

    #[test]
    fn each_children_resume_after_the_iterating_step() {
        // The resume-at index is FRAME state now: `.[]` resumes its children at
        // the step AFTER itself.
        let mut scratch = Scratch::new();
        let steps = vec![each(false), key("a", false)];
        let outcome =
            descend(&steps, 0, &[], EngineResult::owned(array(2)), 0, &mut scratch.borrow()).expect("array fans out");
        match outcome {
            Descended::FanOut { next_at, .. } => assert_eq!(next_at, 1),
            other => panic!("`.[]` must fan out, got {other:?}"),
        }
    }

    #[test]
    fn resolve_index_wraps_negatives_and_bounds_range() {
        assert_eq!(jqf_data::resolve_index(3, 0), Some(0));
        assert_eq!(jqf_data::resolve_index(3, 2), Some(2));
        assert_eq!(jqf_data::resolve_index(3, 3), None);
        assert_eq!(jqf_data::resolve_index(3, -1), Some(2));
        assert_eq!(jqf_data::resolve_index(3, -3), Some(0));
        assert_eq!(jqf_data::resolve_index(3, -4), None);
        assert_eq!(jqf_data::resolve_index(0, 0), None);
    }

    #[test]
    fn owned_null_indexed_forwards_as_leaf_via_owned_arm() {
        // A chain of key/index steps over owned null stays null to the leaf,
        // matching `[{"x":null}] | .[].x.y` reaching null at `.y`.
        let mut scratch = Scratch::new();
        let steps = vec![key("x", false), key("y", false)];
        let outcome = descend(
            &steps,
            0,
            &[],
            EngineResult::owned(Value::Null),
            0,
            &mut scratch.borrow(),
        )
        .expect("null composes");
        assert!(matches!(outcome, Descended::Leaf(EngineResult::Owned(Value::Null))));
    }

    #[test]
    fn descent_over_a_container_recurses_children_at_its_own_step() {
        // The whole shape of `..`: the frame's children resume AT the descend
        // step (so each child descends again) while the SELF value continues one
        // step past it. Getting these two indices the same way round is what
        // makes the walk pre-order and recursive.
        let outcome = descend_step(EngineResult::owned(array(3)), 4).expect("container descends");
        match outcome {
            DescendStep::FanOut(Descended::Descend {
                next_at, container, at, ..
            }) => {
                assert_eq!(next_at, 4, "children re-enter the descend step");
                assert_eq!(at, 5, "self continues past it");
                assert!(matches!(container, Container::Owned(Value::Array(_))));
            }
            other => panic!("a container must fan out, got {other:?}"),
        }
    }

    #[test]
    fn descent_over_a_scalar_emits_only_itself() {
        // `5 | ..` is `5`: a scalar has no children, so the step has no error
        // path and no frame — the walk just continues.
        let outcome = descend_step(EngineResult::owned(number("5")), 0).expect("scalar descends");
        assert!(matches!(
            outcome,
            DescendStep::Scalar(EngineResult::Owned(Value::Number(_)))
        ));

        let mut scratch = Scratch::new();
        let leaf = descend(
            &[descent(false)],
            0,
            &[],
            EngineResult::owned(Value::Null),
            0,
            &mut scratch.borrow(),
        )
        .expect("null descends trivially");
        assert!(matches!(leaf, Descended::Leaf(EngineResult::Owned(Value::Null))));
    }

    #[test]
    fn no_std_floor_and_ceil_match_the_rounding_the_law_needs() {
        assert!((floor_binary64(-1.5) + 2.0).abs() < f64::EPSILON);
        assert!((ceil_binary64(-1.5) + 1.0).abs() < f64::EPSILON);
        assert!((floor_binary64(1.5) - 1.0).abs() < f64::EPSILON);
        assert!((ceil_binary64(1.5) - 2.0).abs() < f64::EPSILON);
        assert!(floor_binary64(-0.0).is_sign_positive() || floor_binary64(-0.0) == 0.0);
        // An already-integral value is its own floor and ceil.
        assert!((ceil_binary64(3.0) - 3.0).abs() < f64::EPSILON);
        assert!((floor_binary64(3.0) - 3.0).abs() < f64::EPSILON);
        assert!(ceil_binary64(f64::INFINITY).is_infinite());
    }

    #[test]
    fn a_backwards_or_open_span_collapses_without_erroring() {
        assert_eq!(span(literal("2"), literal("1"), 6), Some((2, 2)));
        assert_eq!(span(literal("1"), literal("1"), 6), Some((1, 1)));
        assert_eq!(span(SliceBound::Open, SliceBound::Open, 6), Some((0, 6)));
        // An authored `null` bound is the same open end as an omitted one.
        assert_eq!(
            span(SliceBound::Literal(Value::Null), SliceBound::Literal(Value::Null), 6),
            Some((0, 6))
        );
        // A bound of any other type disqualifies itself by TYPE, which the
        // caller turns into the integer-bound raise (over an array or string).
        assert_eq!(span(SliceBound::Literal(text("a")), literal("2"), 6), None);
        assert_eq!(span(literal("1"), SliceBound::Literal(Value::Bool(true)), 6), None);
    }

    #[test]
    fn the_slice_clamp_cell_fires_only_on_authored_bounds_past_the_length() {
        // A bound resolving exactly to the length — `[1,2] | .[0:2]`,
        // `.[2:3]` over a len-3 array, `[] | .[0:0]` over the empty container,
        // and `.[0.5:1.5]` whose end rounds UP to the length — is the ordinary
        // end and fires no cell. Only an
        // authored value OUTSIDE `[0, len]` clamps: `.[5:]` fires once, and
        // `.[-1:]` (wrapping within) stays silent. Counts the WARN report's
        // SliceClamped row.
        fn clamp_events(start: SliceBound, end: SliceBound, len: usize) -> u64 {
            let resources = ledger().with_mismatch_policy(MismatchPolicy::Warn);
            slice_span(&SliceBounds { start, end }, &[], len, false, &resources).expect("bounds resolve");
            resources.take_mismatch_report()[MismatchCell::SliceClamped.index()]
        }
        assert_eq!(clamp_events(literal("0"), literal("2"), 2), 0);
        assert_eq!(clamp_events(literal("2"), literal("3"), 3), 0);
        assert_eq!(clamp_events(literal("0"), literal("0"), 0), 0);
        assert_eq!(clamp_events(literal("0.5"), literal("1.5"), 2), 0);
        assert_eq!(clamp_events(literal("5"), SliceBound::Open, 2), 1);
        assert_eq!(clamp_events(literal("-1"), SliceBound::Open, 2), 0);
    }

    #[test]
    fn a_slice_over_null_yields_null_even_with_a_disqualified_bound() {
        // The pinned dispatch precedence: the INPUT's type is examined first, so
        // `null | .["a":2]` is `null` at exit 0, never the integer-bound raise.
        let mut scratch = Scratch::new();
        let outcome = slice_owned(
            &Value::Null,
            &SliceBounds {
                start: SliceBound::Literal(text("a")),
                end: literal("2"),
            },
            &[],
            false,
            &mut scratch.borrow(),
        )
        .expect("null slices");
        assert!(matches!(outcome, Sliced::Null));
    }

    #[test]
    fn a_slice_over_a_non_sliceable_input_reports_the_authored_bounds() {
        // The operand object is SYNTHESIZED from the authored bounds and rendered
        // through the ordinary owned writer, so an open bound reports as `null`
        // and a string bound keeps its quotes — byte-identical to the reference.
        let mut scratch = Scratch::new();
        let outcome = slice_owned(
            &Value::Bool(true),
            &SliceBounds {
                start: SliceBound::Literal(text("a")),
                end: SliceBound::Open,
            },
            &[],
            false,
            &mut scratch.borrow(),
        )
        .expect("mismatch renders");
        match outcome {
            Sliced::NotSliceable { kind, key } => {
                assert_eq!(kind, ValueKind::Bool);
                assert_eq!(key, "object ({\"start\":\"a\",\"end\":null})");
            }
            other => panic!("a boolean is not sliceable, got a different arm: {other:?}"),
        }
    }

    #[test]
    fn a_slice_mismatch_never_reports_the_normalized_bound() {
        // The whole point of carrying the AUTHORED bound: `.[1.5:3]` reports the
        // bound it was written with, never the `1` the start law would floor it
        // to. The authored values render through the ordinary owned writer, so
        // the operand is byte-identical to the reference's
        // (`{"start":1.5,"end":3}`) and the full spelling is pinned below.
        let mut scratch = Scratch::new();
        let outcome = slice_owned(
            &Value::Bool(true),
            &SliceBounds {
                start: literal("1.5"),
                end: literal("3"),
            },
            &[],
            false,
            &mut scratch.borrow(),
        )
        .expect("mismatch renders");
        match outcome {
            Sliced::NotSliceable { key, .. } => {
                // The FULL authored spelling, byte-identical to the reference's
                // operand: the floored `1` must never appear and the trailing
                // scale of `1.5` survives.
                assert_eq!(key, "object ({\"start\":1.5,\"end\":3})");
            }
            other => panic!("a boolean is not sliceable, got a different arm: {other:?}"),
        }
    }

    #[test]
    fn an_owned_array_slice_materializes_a_new_array() {
        let mut scratch = Scratch::new();
        let outcome = slice_owned(
            &array(6),
            &SliceBounds {
                start: literal("1"),
                end: literal("3"),
            },
            &[],
            false,
            &mut scratch.borrow(),
        )
        .expect("array slices");
        match outcome {
            Sliced::Value(Value::Array(out)) => assert_eq!(out.len(), 2),
            other => panic!("an array slice is a new array, got {other:?}"),
        }
    }

    #[test]
    fn a_string_slice_cuts_on_codepoint_boundaries() {
        // `"aébç" | .[1:3]` is `"éb"` — codepoints, never bytes (a byte cut
        // at 1..3 would split the two-byte `é`).
        let mut scratch = Scratch::new();
        let outcome = slice_owned(
            &text("aébç"),
            &SliceBounds {
                start: literal("1"),
                end: literal("3"),
            },
            &[],
            false,
            &mut scratch.borrow(),
        )
        .expect("string slices");
        match outcome {
            Sliced::Value(Value::String(text)) => assert_eq!(text, "éb"),
            other => panic!("a string slice is a new string, got {other:?}"),
        }
    }

    #[test]
    fn an_open_bound_and_a_null_bound_classify_identically() {
        assert!(matches!(
            super::slice_bound(&SliceBound::Open, &[]).expect("open"),
            Bound::Open
        ));
        assert!(matches!(
            super::slice_bound(&SliceBound::Literal(Value::Null), &[]).expect("null"),
            Bound::Open
        ));
        assert!(matches!(
            super::slice_bound(&literal("2"), &[]).expect("number"),
            Bound::Number(_)
        ));
        assert!(matches!(
            super::slice_bound(&SliceBound::Literal(Value::Bool(false)), &[]).expect("bool"),
            Bound::Bad
        ));
    }

    #[test]
    fn a_slice_step_suppresses_both_of_its_raise_classes_with_its_own_question_mark() {
        let mut scratch = Scratch::new();
        let optional = StageStep::new(
            StepAccess::Slice(Box::new(bounds(SliceBound::Literal(text("a")), literal("2")))),
            true,
        );
        // The integer-bound class, over an array.
        let outcome = descend(
            core::slice::from_ref(&optional),
            0,
            &[],
            EngineResult::owned(array(3)),
            0,
            &mut scratch.borrow(),
        )
        .expect("`?` suppresses the integer-bound raise");
        assert!(matches!(outcome, Descended::Skip));
        // ... and the index class, over a non-sliceable input.
        let outcome = descend(
            core::slice::from_ref(&optional),
            0,
            &[],
            EngineResult::owned(Value::Bool(true)),
            0,
            &mut scratch.borrow(),
        )
        .expect("`?` suppresses the index-class mismatch");
        assert!(matches!(outcome, Descended::Skip));
    }

    #[test]
    fn an_unsuppressed_slice_raises_its_two_distinct_classes() {
        let mut scratch = Scratch::new();
        let step = StageStep::new(
            StepAccess::Slice(Box::new(bounds(SliceBound::Literal(text("a")), literal("2")))),
            false,
        );
        let error = descend(
            core::slice::from_ref(&step),
            0,
            &[],
            EngineResult::owned(array(3)),
            0,
            &mut scratch.borrow(),
        )
        .expect_err("a bad bound over an array raises");
        assert!(matches!(error, EngineRunError::SliceIndices));

        let error = descend(
            core::slice::from_ref(&step),
            7,
            &[],
            EngineResult::owned(Value::Bool(true)),
            0,
            &mut scratch.borrow(),
        )
        .expect_err("a slice over a boolean raises");
        assert!(matches!(
            error,
            EngineRunError::TypeMismatch {
                step_index: 7,
                actual_type: ValueKind::Bool,
                ..
            }
        ));
    }

    #[test]
    fn tag_accessor_reads_the_intrinsic_tag_of_a_located_node() {
        let (product, handle) = located_document(Some("!money"), |_, _, _| {});
        let leaf = located_leaf(&product, handle, core::slice::from_ref(&accessor("tag", false)));
        assert_eq!(string_value(&leaf), "!money");
    }

    #[test]
    fn tag_accessor_answers_null_when_a_located_node_has_no_tag() {
        let (product, handle) = located_document(None, |_, _, _| {});
        let leaf = located_leaf(&product, handle, core::slice::from_ref(&accessor("tag", false)));
        assert!(matches!(leaf, Value::Null));
    }

    #[test]
    fn tag_accessor_exposes_an_owned_tagged_wrapper() {
        let leaf = owned_leaf(tagged("!money"), core::slice::from_ref(&accessor("tag", false)));
        assert_eq!(string_value(&leaf), "!money");
    }

    #[test]
    fn tag_accessor_answers_null_for_an_owned_untagged_value() {
        let leaf = owned_leaf(Value::Bool(true), core::slice::from_ref(&accessor("tag", false)));
        assert!(matches!(leaf, Value::Null));
    }

    #[test]
    fn comment_accessor_reads_the_attached_fact_with_that_role() {
        let (product, handle) = located_document(None, |builder, root, resources| {
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "comment",
                    "text",
                    1,
                    &FactPayload::Text(String::from("leading note")),
                    resources,
                )
                .expect("comment fact");
        });
        let leaf = located_leaf(&product, handle, core::slice::from_ref(&accessor("comment", false)));
        assert_eq!(string_value(&leaf), "leading note");
    }

    #[test]
    fn name_content_and_attrs_accessors_read_their_role_facts() {
        let (product, handle) = located_document(None, |builder, root, resources| {
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "name",
                    "name",
                    1,
                    &FactPayload::Text(String::from("article")),
                    resources,
                )
                .expect("name fact");
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "content",
                    "text",
                    1,
                    &FactPayload::Text(String::from("inside")),
                    resources,
                )
                .expect("content fact");
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "attrs",
                    "map",
                    1,
                    &FactPayload::Map(vec![(String::from("class"), FactPayload::Text(String::from("a")))]),
                    resources,
                )
                .expect("attrs fact");
        });

        let name = located_leaf(&product, handle, core::slice::from_ref(&accessor("name", false)));
        assert_eq!(string_value(&name), "article");

        let content = located_leaf(&product, handle, core::slice::from_ref(&accessor("content", false)));
        assert_eq!(string_value(&content), "inside");

        let attrs = located_leaf(&product, handle, core::slice::from_ref(&accessor("attrs", false)));
        match attrs {
            Value::Object(object) => {
                let class = object.get("class").expect("class attribute");
                assert_eq!(string_value(class), "a");
            }
            other => panic!("expected an attribute map, got {other:?}"),
        }
    }

    #[test]
    fn node_accessor_reads_a_namespaced_fact_role() {
        // The XML codec attaches its facts under namespaced roles such as
        // `xml.name@1`; the `.@name` selector serves the role's semantic
        // segment, exactly as it serves a plain `name` role.
        let (product, handle) = located_document(None, |builder, root, resources| {
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "xml.name@1",
                    "xml.name@1",
                    1,
                    &FactPayload::Text(String::from("article")),
                    resources,
                )
                .expect("name fact");
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "xml.content@1",
                    "xml.content@1",
                    1,
                    &FactPayload::Text(String::from("inside")),
                    resources,
                )
                .expect("content fact");
        });

        let name = located_leaf(&product, handle, core::slice::from_ref(&accessor("name", false)));
        assert_eq!(string_value(&name), "article");

        let content = located_leaf(&product, handle, core::slice::from_ref(&accessor("content", false)));
        assert_eq!(string_value(&content), "inside");

        // An unrelated selector does not match a namespaced role.
        let none = located_leaf(&product, handle, core::slice::from_ref(&accessor("price", false)));
        assert!(matches!(none, Value::Null));
    }

    #[test]
    fn attribute_accessor_reads_the_attribute_kind_fact() {
        let (product, handle) = located_document(None, |builder, root, resources| {
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "attribute",
                    "href",
                    1,
                    &FactPayload::Text(String::from("/docs")),
                    resources,
                )
                .expect("href fact");
        });
        let leaf = located_leaf(&product, handle, core::slice::from_ref(&attribute("href", false)));
        assert_eq!(string_value(&leaf), "/docs");
    }

    #[test]
    fn attribute_accessor_is_null_without_a_matching_kind() {
        let (product, handle) = located_document(None, |builder, root, resources| {
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "attribute",
                    "href",
                    1,
                    &FactPayload::Text(String::from("/docs")),
                    resources,
                )
                .expect("href fact");
        });
        let leaf = located_leaf(&product, handle, core::slice::from_ref(&attribute("class", false)));
        assert!(matches!(leaf, Value::Null));
    }

    #[test]
    fn absent_fact_answers_null_and_owned_values_carry_no_facts() {
        let (product, handle) = located_document(None, |_, _, _| {});
        let leaf = located_leaf(&product, handle, core::slice::from_ref(&accessor("comment", false)));
        assert!(matches!(leaf, Value::Null));

        let leaf = owned_leaf(Value::Bool(true), core::slice::from_ref(&accessor("comment", false)));
        assert!(matches!(leaf, Value::Null));
    }

    #[test]
    fn an_accessor_step_never_raises_and_answers_one_value_for_any_input() {
        // The step is total: it answers (the fact, or `null`) for every input
        // kind and every selector, so even a tagged NUMBER input answers.
        let leaf = owned_leaf(tagged("!n"), core::slice::from_ref(&accessor("tag", false)));
        assert_eq!(string_value(&leaf), "!n");
    }

    #[test]
    fn a_document_without_attached_facts_answers_null_instead_of_raising() {
        // A strict-JSON-style document retains no attached facts, so the
        // fact reader is a capability-absent answer, not a matching-fact scan:
        // the accessor must report the absence as `null`, never an error.
        let resources = ledger();
        let mut builder = AccountedDocumentBuilder::try_new_with_coverage(
            "test",
            None,
            jqf_data::BuilderCoverage::minimal_semantic(),
        )
        .expect("builder");
        let root = builder
            .add_node("test.value", AccountedSemanticNode::Null, None, &resources)
            .expect("root");
        let document = builder.finish(root, &resources).expect("document");
        let product = DocumentProduct::try_new(document, &resources).expect("product");
        let handle = product.document().root_handle();
        let leaf = located_leaf(&product, handle, core::slice::from_ref(&accessor("comment", false)));
        assert!(matches!(leaf, Value::Null));
    }

    fn dyn_node(slot: u32, optional: bool) -> StageStep {
        StageStep::new(StepAccess::DynNodeAccessor(slot), optional)
    }

    fn dyn_attr(slot: u32, optional: bool) -> StageStep {
        StageStep::new(StepAccess::DynAttribute(slot), optional)
    }

    fn located_leaf_env(
        product: &DocumentProduct<'static>,
        handle: NodeHandle,
        steps: &[StageStep],
        env: &[EngineResult<'static>],
    ) -> Value {
        let located = LocatedProduct::try_new(product, handle).expect("located");
        let mut scratch = Scratch::new();
        let outcome =
            descend(steps, 0, env, EngineResult::Located(located), 0, &mut scratch.borrow()).expect("accessor leaf");
        match outcome {
            Descended::Leaf(EngineResult::Owned(value)) => value,
            other => panic!("expected an owned accessor leaf, got {other:?}"),
        }
    }

    fn owned_leaf_env(value: Value, steps: &[StageStep], env: &[EngineResult<'static>]) -> Value {
        let mut scratch = Scratch::new();
        let outcome =
            descend(steps, 0, env, EngineResult::owned(value), 0, &mut scratch.borrow()).expect("owned accessor leaf");
        match outcome {
            Descended::Leaf(EngineResult::Owned(value)) => value,
            other => panic!("expected an owned accessor leaf, got {other:?}"),
        }
    }

    #[test]
    fn dynamic_tag_accessor_matches_the_static_form() {
        let env = [EngineResult::owned(text("tag"))];
        let (product, handle) = located_document(Some("!money"), |_, _, _| {});
        let dynamic = located_leaf_env(&product, handle, core::slice::from_ref(&dyn_node(0, false)), &env);
        let static_leaf = located_leaf(&product, handle, core::slice::from_ref(&accessor("tag", false)));
        assert_eq!(string_value(&dynamic), string_value(&static_leaf));
        assert_eq!(string_value(&dynamic), "!money");

        let owned_dynamic = owned_leaf_env(tagged("!money"), core::slice::from_ref(&dyn_node(0, false)), &env);
        let owned_static = owned_leaf(tagged("!money"), core::slice::from_ref(&accessor("tag", false)));
        assert_eq!(string_value(&owned_dynamic), string_value(&owned_static));
    }

    #[test]
    fn dynamic_attribute_accessor_matches_the_static_form() {
        let env = [EngineResult::owned(text("href"))];
        let (product, handle) = located_document(None, |builder, root, resources| {
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "attribute",
                    "href",
                    1,
                    &FactPayload::Text(String::from("/docs")),
                    resources,
                )
                .expect("href fact");
        });
        let dynamic = located_leaf_env(&product, handle, core::slice::from_ref(&dyn_attr(0, false)), &env);
        let static_leaf = located_leaf(&product, handle, core::slice::from_ref(&attribute("href", false)));
        assert_eq!(string_value(&dynamic), string_value(&static_leaf));
        assert_eq!(string_value(&dynamic), "/docs");
    }

    #[test]
    fn dynamic_comment_head_alias_normalizes_like_the_static_form() {
        let env = [EngineResult::owned(text("comment_head"))];
        let (product, handle) = located_document(None, |builder, root, resources| {
            builder
                .add_fact(
                    LocalOwnerRef::Node(root),
                    "comment",
                    "text",
                    1,
                    &FactPayload::Text(String::from("leading note")),
                    resources,
                )
                .expect("comment fact");
        });
        let dynamic = located_leaf_env(&product, handle, core::slice::from_ref(&dyn_node(0, false)), &env);
        let static_leaf = located_leaf(&product, handle, core::slice::from_ref(&accessor("comment", false)));
        assert_eq!(string_value(&dynamic), string_value(&static_leaf));
    }

    #[test]
    fn dynamic_accessor_rejects_a_non_string_selector() {
        let env = [EngineResult::owned(number("1"))];
        let mut scratch = Scratch::new();
        let error = descend(
            core::slice::from_ref(&dyn_node(0, false)),
            0,
            &env,
            EngineResult::owned(tagged("!n")),
            0,
            &mut scratch.borrow(),
        )
        .expect_err("a numeric selector is not an accessor name");
        assert!(matches!(error, EngineRunError::TypeMismatch { .. }));
    }

    /// A located `{name: integer}` document so a Key step walks the located
    /// view/kind path rather than the owned constructor arm.
    fn located_int_object(name: &str, value: &str) -> (DocumentProduct<'static>, NodeHandle) {
        let resources = ledger();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let child = builder
            .add_node("test.int", AccountedSemanticNode::Integer(value), None, &resources)
            .expect("child");
        let root = builder
            .add_node(
                "test.object",
                AccountedSemanticNode::Object {
                    member_role: "test.member",
                },
                None,
                &resources,
            )
            .expect("root");
        builder
            .add_occurrence(
                LocalOwnerRef::Node(root),
                "test.member",
                Some(AccountedOccurrenceKey::Text(name)),
                child,
                &resources,
            )
            .expect("member");
        let document = builder.finish(root, &resources).expect("document");
        let product = DocumentProduct::try_new(document, &resources).expect("product");
        let handle = product.document().root_handle();
        (product, handle)
    }

    /// A located `{outer: {inner: integer}}` document — two Key steps, one
    /// view/kind pair per step.
    fn located_nested_int_object(outer: &str, inner: &str, value: &str) -> (DocumentProduct<'static>, NodeHandle) {
        let resources = ledger();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let leaf = builder
            .add_node("test.int", AccountedSemanticNode::Integer(value), None, &resources)
            .expect("leaf");
        let nested = builder
            .add_node(
                "test.object",
                AccountedSemanticNode::Object {
                    member_role: "test.member",
                },
                None,
                &resources,
            )
            .expect("nested");
        builder
            .add_occurrence(
                LocalOwnerRef::Node(nested),
                "test.member",
                Some(AccountedOccurrenceKey::Text(inner)),
                leaf,
                &resources,
            )
            .expect("inner member");
        let root = builder
            .add_node(
                "test.object",
                AccountedSemanticNode::Object {
                    member_role: "test.member",
                },
                None,
                &resources,
            )
            .expect("root");
        builder
            .add_occurrence(
                LocalOwnerRef::Node(root),
                "test.member",
                Some(AccountedOccurrenceKey::Text(outer)),
                nested,
                &resources,
            )
            .expect("outer member");
        let document = builder.finish(root, &resources).expect("document");
        let product = DocumentProduct::try_new(document, &resources).expect("product");
        let handle = product.document().root_handle();
        (product, handle)
    }

    /// A located integer array — a string Key is the ordinary array-with-string
    /// mismatch, never a silent null, whether or not the markup probe ran.
    fn located_int_array(values: &[&str]) -> (DocumentProduct<'static>, NodeHandle) {
        let resources = ledger();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let mut items = alloc::vec::Vec::new();
        for value in values {
            items.push(
                builder
                    .add_node("test.int", AccountedSemanticNode::Integer(value), None, &resources)
                    .expect("item"),
            );
        }
        let root = builder
            .add_node(
                "test.array",
                AccountedSemanticNode::Array { item_role: "test.item" },
                None,
                &resources,
            )
            .expect("root");
        for item in items {
            builder
                .add_occurrence(LocalOwnerRef::Node(root), "test.item", None, item, &resources)
                .expect("edge");
        }
        let document = builder.finish(root, &resources).expect("document");
        let product = DocumentProduct::try_new(document, &resources).expect("product");
        let handle = product.document().root_handle();
        (product, handle)
    }

    fn descend_located<'a>(
        product: &'a DocumentProduct<'static>,
        handle: NodeHandle,
        steps: &[StageStep],
        scratch: &mut Scratch,
    ) -> Result<Descended<'a>, EngineRunError> {
        let located = LocatedProduct::try_new(product, handle).expect("located");
        descend(steps, 0, &[], EngineResult::Located(located), 0, &mut scratch.borrow())
    }

    fn assert_leaf_int(outcome: Descended<'_>, scratch: &mut Scratch, expected: i64) {
        let value = match outcome {
            Descended::Leaf(EngineResult::Owned(value)) => value,
            Descended::Leaf(EngineResult::Located(located)) => located
                .product()
                .document()
                .materialize_node(located.node(), &mut scratch.resources)
                .expect("materialize"),
            other => panic!("expected a leaf, got {other:?}"),
        };
        match value {
            Value::Number(n) => assert_eq!(n.as_machine(), Some(expected)),
            other => panic!("expected integer {expected}, got {other:?}"),
        }
    }

    /// A Key step opens one payload-transparent view/kind; the markup probe
    /// and the object lookup both read that pair. Hit, miss, nested keys,
    /// array-with-string mismatch, optional-suffix skip, and the owned
    /// constructor arm all keep their existing answers.
    #[test]
    fn key_step_reuses_one_located_view_and_kind() {
        let mut scratch = Scratch::new();

        let (product, handle) = located_int_object("a", "1");
        let hit = descend_located(&product, handle, core::slice::from_ref(&key("a", false)), &mut scratch)
            .expect("located object key");
        assert_leaf_int(hit, &mut scratch, 1);

        let miss = descend_located(
            &product,
            handle,
            core::slice::from_ref(&key("missing", false)),
            &mut scratch,
        )
        .expect("missing key is null precedence");
        assert!(matches!(miss, Descended::Leaf(EngineResult::Owned(Value::Null))));

        let (nested_product, nested_handle) = located_nested_int_object("a", "b", "2");
        let nested = descend_located(
            &nested_product,
            nested_handle,
            &[key("a", false), key("b", false)],
            &mut scratch,
        )
        .expect("nested located keys");
        assert_leaf_int(nested, &mut scratch, 2);

        let (array_product, array_handle) = located_int_array(&["1", "2"]);
        let mismatch = descend_located(
            &array_product,
            array_handle,
            core::slice::from_ref(&key("b", false)),
            &mut scratch,
        )
        .expect_err("array-with-string is a hard mismatch");
        assert!(matches!(
            mismatch,
            EngineRunError::TypeMismatch {
                actual_type: ValueKind::Array,
                ..
            }
        ));

        let skipped = descend_located(
            &array_product,
            array_handle,
            core::slice::from_ref(&key("b", true)),
            &mut scratch,
        )
        .expect("optional suffix suppresses the mismatch");
        assert!(matches!(skipped, Descended::Skip));

        let mut object = Object::try_new().expect("object");
        assert!(
            object
                .try_insert_unique(ObjectKey::try_from_str("a").expect("key"), number("3"))
                .expect("insert")
        );
        let owned = descend(
            core::slice::from_ref(&key("a", false)),
            0,
            &[],
            EngineResult::owned(Value::Object(object)),
            0,
            &mut scratch.borrow(),
        )
        .expect("owned object key");
        assert_leaf_int(owned, &mut scratch, 3);
    }
}
