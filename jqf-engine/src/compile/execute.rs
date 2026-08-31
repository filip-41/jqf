//! Thin job match: committed shortcut, then the residual graph.
//!
//! [`CompiledProgram::execute`] tries the oracle, then the graph.
//! Document oracle bodies live in [`crate::exec::oracles`]. Exact miss that
//! cannot tell Exact from Whole returns [`EngineRun::ReboundWhole`]; YAML/HTML
//! `CompleteDocumentExact` fallback relocates to the document root and runs
//! Whole. `node == root` is not Exact vs Whole.
//!
//! [`crate::HostIo::Echo`] and [`crate::HostIo::SpanCut`] stay host I/O.
//! Identity echo needs the source window execute does not hold. Range-locate
//! needs the codec session and byte window. Execute's Identity / `RangeLocate`
//! arms are the graph floor after that I/O declines. `-n`/`-s` keep
//! [`CompiledProgram::try_run_whole_value`] because their input is synthesized,
//! never Exact-located.
//!
//! Lenient-only. Decline is byte-identical to the graph.

#[allow(clippy::wildcard_imports)]
use super::*;

use jqf_codec_core::LocatedProduct;

impl CompiledProgram {
    /// Runs this program on one codec access outcome: shortcut, then the graph.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when seeding the executor fails. Typed program-error
    /// arms are `Ok` variants of [`EngineRun`].
    pub fn execute<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        if let Some(run) = self.try_shortcut(&outcome, resources)? {
            return Ok(run);
        }
        self.graph_after_decline(outcome, resources)
    }

    fn try_shortcut<'source>(
        &self,
        outcome: &CodecInputOutcome<'source>,
        resources: &mut ResourceContext<'_>,
    ) -> Result<Option<EngineRun<'_, 'source>>, CodecError> {
        if resources.mismatch_policy() != MismatchPolicy::Lenient {
            return Ok(None);
        }
        match &self.shortcut {
            Shortcut::Count(demand) => Ok(self.count_answer(outcome, demand, resources)?),
            Shortcut::Keys(path) => Ok(self.keys_answer(outcome, path, resources)),
            Shortcut::Type(path) => Ok(self.type_answer(outcome, path, resources)),
            Shortcut::Has(demand) => Ok(self.has_answer(outcome, demand, resources)),
            Shortcut::AnyAll(demand) => Ok(self.any_all_answer(outcome, demand, resources)),
            Shortcut::MinMax(demand) => Ok(self.min_max_answer(outcome, demand, resources)),
            Shortcut::Element {
                demand,
                construct,
                collect,
            } => Ok(self.element_answer(outcome, demand, construct.as_deref(), *collect, resources)),
            // HostIo::Echo / SpanCut: execute does not hold the source window.
            // The SDK matches host_io() for retained-byte echo and span cut;
            // this is the graph floor when that I/O declines.
            Shortcut::None | Shortcut::Identity | Shortcut::RangeLocate => Ok(None),
        }
    }

    fn graph_after_decline<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        resources: &ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        if self.may_rebind_whole() {
            return self.exact_miss_graph(outcome, resources);
        }
        self.graph_run(outcome, resources)
    }

    /// Exact miss must not run the graph on the Exact node.
    ///
    /// YAML/HTML Exact names a child in the full graph: relocate to the document
    /// root and run Whole (`prefix_len = 0` then matches `demand.path`). JSON
    /// Exact republishes the selection as root (`node == root`), so Whole on
    /// this outcome would walk PATH again. The host rebinds Whole. Count and
    /// element Exact share this path; `node == root` is not how Exact is told
    /// from Whole.
    fn exact_miss_graph<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        resources: &ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        if let CodecInputOutcome::Result(EngineResult::Located(located)) = &outcome {
            let document = located.product().document();
            if located.node() != document.root_handle() {
                let relocated = LocatedProduct::try_new(located.product(), document.root_handle())
                    .map_err(|_| CodecError::new(CodecFailureKind::ProviderRouteMismatch))?;
                return self
                    .try_run_whole_value(CodecInputOutcome::Result(EngineResult::Located(relocated)), resources);
            }
            return Ok(EngineRun::ReboundWhole);
        }
        self.try_run_whole_value(outcome, resources)
    }

    fn graph_run<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        resources: &ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        let whole = matches!(&self.shortcut, Shortcut::Count(_) | Shortcut::Element { .. })
            || resources.mismatch_policy() != MismatchPolicy::Lenient;
        if whole {
            return self.try_run_whole_value(outcome, resources);
        }
        try_run_program(&self.program, self.program.split().prefix_len(), outcome, None)
    }

    /// Runs this program over one SYNTHESIZED value with no codec pushdown:
    /// every static step the split handed to the codec is applied by the
    /// engine instead.
    ///
    /// [`Self::execute`] assumes the codec already resolved the pushed-down
    /// prefix, which is true for every drive whose input came OUT of a decode.
    /// The single-run drives (`-n`, `-s`, and their record siblings) build
    /// their input themselves — `null`, or the array of every decoded input —
    /// so nothing resolved the prefix and the whole program must run
    /// (`-s '.[0]'` answers the first value, and running the residual alone
    /// answers the whole array). Do not call [`Self::execute`]: an Exact
    /// access plus a Whole decode would count from the wrong node.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when seeding the executor fails.
    pub fn try_run_whole_value<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        _resources: &ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        try_run_program(&self.program, 0, outcome, None)
    }

    /// Runs this program over one value with `$index` BOUND to the item
    /// counter — the split-destination run. Whole
    /// semantics exactly like [`Self::try_run_whole_value`] (nothing resolved
    /// the split program's prefix; the whole program runs), with the
    /// difference that `$index`'s compile-time slot is seeded with `index`
    /// before the first poll.
    ///
    /// The item counter binds as an exact integer. The seed is a no-op when
    /// the expression never references `$index` (the slot is `None`).
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] when seeding the executor fails.
    pub fn try_run_split<'source>(
        &self,
        outcome: CodecInputOutcome<'source>,
        index: u64,
        _resources: &ResourceContext<'_>,
    ) -> Result<EngineRun<'_, 'source>, CodecError> {
        let runtime_index = self.runtime_index_slot().map(|slot| {
            (
                slot,
                jqf_data::Value::Number(jqf_data::Number::integer(jqf_data::Integer::from_i64(
                    i64::try_from(index).unwrap_or(i64::MAX),
                ))),
            )
        });
        try_run_program(&self.program, 0, outcome, runtime_index)
    }
}
