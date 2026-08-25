//! Located-versus-owned engine result handoff.

use jqf_codec_core::{
    AccessOutcome, AccessReport, AccessResult, CodecError, ExactSelectionRecord, LocatedOutcome, LocatedProduct,
};
use jqf_data::{Value, ValueKind};

/// One semantic engine result with its remaining authority made explicit.
#[derive(Debug)]
pub enum EngineResult<'source> {
    /// A value still located in its authoritative decoded document.
    Located(LocatedProduct<'source>),
    /// A value produced independently at an explicit semantic construction barrier.
    Owned(Value),
}

impl<'source> EngineResult<'source> {
    /// Wraps one independently owned semantic value.
    #[must_use]
    pub const fn owned(value: Value) -> Self {
        Self::Owned(value)
    }

    /// Returns the retained location when this result is still document-backed.
    #[must_use]
    pub const fn located(&self) -> Option<&LocatedProduct<'source>> {
        match self {
            Self::Located(located) => Some(located),
            Self::Owned(_) => None,
        }
    }

    /// Returns the independent value when this result has crossed a construction barrier.
    #[must_use]
    pub const fn owned_value(&self) -> Option<&Value> {
        match self {
            Self::Located(_) => None,
            Self::Owned(value) => Some(value),
        }
    }

    /// Re-derives a second owner of this result at a choice fork.
    ///
    /// A comma evaluates both members over the SAME input, so the fork needs a second handle. A located value
    /// re-borrows the same authoritative document via [`LocatedProduct::try_clone`] (Arc-cheap; the fork transition is
    /// therefore fallible and gains a [`CodecError`] path — no access is re-run and no input is re-charged).
    ///
    /// An engine-owned value re-derives through [`Value::try_clone`], which is itself a SHARE on every heap-backed
    /// variant — elements, entries, string, bytes, tag payload — rather than a deep copy. There is one mechanism
    /// here, not two: an owned re-derivation is refcount bumps at the handle, and it is sound because every mutator
    /// routes through a `try_*_mut` that detaches a non-unique allocation before writing, so a shared re-derivation is
    /// observationally identical to a detached one.
    ///
    /// The deep copy this replaces was QUADRATIC on the descent. `..` emits every node; once a construction barrier has
    /// promoted a fan-out frame to its owned form, every subsequent emission is owned, and a re-derivation between the
    /// descent and the barrier — the `select` in `[.. | select(f)]`, which re-derives TWICE (once for the predicate,
    /// once for the emission) — copied each node's whole subtree. On the 2 MB deep-tree fixture, `[.. | select(true)]
    /// | length` fell from 381 ms / 508 MB peak RSS to 68 ms / 99 MB, which is exactly the cost of the plain `[..] |
    /// length` beside it.
    pub fn try_clone(&self) -> Result<Self, CodecError> {
        match self {
            Self::Located(located) => Ok(Self::Located(located.try_clone()?)),
            Self::Owned(value) => Ok(Self::Owned(value.clone())),
        }
    }
}

/// Engine-visible semantic interpretation of one codec access outcome.
///
/// Missing and type-mismatch observations retain the complete authoritative product that proved them. Engine semantics
/// can therefore apply optionality or diagnostics without turning an observation into a naked sentinel.
#[derive(Debug)]
pub enum CodecInputOutcome<'source> {
    /// One result ready for ordinary engine evaluation.
    Result(EngineResult<'source>),
    /// The exact static path did not exist.
    Missing {
        /// Authority-owned exact observation.
        authority: LocatedOutcome<'source>,
    },
    /// A static path step addressed the wrong semantic category.
    TypeMismatch {
        /// Authority-owned exact observation.
        authority: LocatedOutcome<'source>,
        /// Zero-based path step.
        step_index: usize,
        /// Actual payload-transparent type.
        actual_type: ValueKind,
        /// The markup accessor hint carried from the codec's locate arm (a missed member step whose name matches an
        /// attribute or the element's own name). `None` for every format-neutral source.
        hint: Option<alloc::string::String>,
    },
}

/// One interpreted engine input plus the exact physical/diagnostic access report.
#[derive(Debug)]
pub struct CodecInputResult<'source> {
    outcome: CodecInputOutcome<'source>,
    report: AccessReport,
}

impl<'source> CodecInputResult<'source> {
    /// Converts a complete codec result without materializing document-backed values.
    pub fn try_from_access(result: AccessResult<'source>) -> Result<Self, CodecError> {
        let (outcome, report) = result.into_parts();
        let outcome = match outcome {
            AccessOutcome::FullDocument(product) => {
                let root = product.document().root_handle();
                CodecInputOutcome::Result(EngineResult::Located(LocatedProduct::try_new(&product, root)?))
            }
            AccessOutcome::Located(located) => CodecInputOutcome::try_from_located(located)?,
        };
        Ok(Self { outcome, report })
    }

    /// Borrows the engine-interpreted outcome.
    #[must_use]
    pub const fn outcome(&self) -> &CodecInputOutcome<'source> {
        &self.outcome
    }

    /// Returns the fixed access report retained across the engine boundary.
    #[must_use]
    pub const fn report(&self) -> AccessReport {
        self.report
    }

    /// Consumes this value into the interpreted outcome and fixed report.
    #[must_use]
    pub fn into_parts(self) -> (CodecInputOutcome<'source>, AccessReport) {
        (self.outcome, self.report)
    }
}

impl<'source> CodecInputOutcome<'source> {
    /// Interprets one exact located observation into an engine input outcome:
    /// a resolved node becomes a located result, a missing path and a step type mismatch retain their complete
    /// authority as negative outcomes.
    ///
    /// This is the container-level negative reading of a located observation:
    /// when the codec cannot stream a container (missing, or a non-iterable scalar at the container path), it yields
    /// the identical [`LocatedOutcome`] the located route would have, so it flows through
    /// [`crate::CompiledProgram::try_run`]'s residual (`[.[], …]`) interpretation unchanged — the pushed-down `.[]`
    /// is read as an iterate mismatch, prefix `?`/missing/mismatch as before.
    pub fn try_from_located(located: LocatedOutcome<'source>) -> Result<Self, CodecError> {
        let (step_index, actual_type, hint) = match located.result() {
            ExactSelectionRecord::Node { node, .. } => {
                return Ok(CodecInputOutcome::Result(EngineResult::Located(
                    LocatedProduct::try_new(located.product(), *node)?,
                )));
            }
            ExactSelectionRecord::Missing { .. } => {
                return Ok(CodecInputOutcome::Missing { authority: located });
            }
            ExactSelectionRecord::TypeMismatch {
                step_index,
                actual_type,
                hint,
                ..
            } => (*step_index, *actual_type, hint.clone()),
        };
        Ok(CodecInputOutcome::TypeMismatch {
            authority: located,
            step_index,
            actual_type,
            hint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{CodecInputOutcome, CodecInputResult};
    use jqf_codec_core::{
        AccessOutcome, AccessResult, DocumentProduct, ExactSelectionRecord, LocatedOutcome, SelectionOrigin,
    };
    use jqf_data::{AccountedDocumentBuilder, AccountedSemanticNode};
    use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn resources() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX))
                .expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4_096).expect("work"),
        )
        .expect("resources")
    }

    #[test]
    fn negative_exact_handoff_retains_authority_and_report() {
        let resources = resources();
        let mut builder = AccountedDocumentBuilder::try_new("test", None).expect("builder");
        let root = builder
            .add_node("test.bool", AccountedSemanticNode::Bool(true), None, &resources)
            .expect("root");
        let product =
            DocumentProduct::try_new(builder.finish(root, &resources).expect("document"), &resources).expect("product");
        let located = LocatedOutcome::try_new(
            &product,
            ExactSelectionRecord::Missing {
                step_index: 0,
                origin: SelectionOrigin::new(0),
            },
        )
        .expect("located");
        let input = AccessResult::from_outcome(AccessOutcome::Located(located));
        let result = CodecInputResult::try_from_access(input).expect("handoff");
        assert!(matches!(
            result.outcome(),
            CodecInputOutcome::Missing { authority }
                if authority.product().document().node_count() == 1
        ));
        assert_eq!(result.report().route(), None);
    }
}
