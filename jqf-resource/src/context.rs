//! The handle a request holds: its ledger, cancellation, and work budget.
//!
//! Call this for limits, output permits, nesting, diagnostics, and the small knobs a run needs ([`MismatchPolicy`],
//! [`StrictnessPolicy`], and so on). Those knobs live in [`crate::policy`]; this file just carries them.

use crate::policy::{MISMATCH_CELL_COUNT, MismatchPolicy, ProjectionKind, StrictnessPolicy};
use crate::{
    Control, ControlError, ControlOutcome, DepthGuard, OutputPermit, OwnedDepthGuard, RequestAccount, ResourceError,
    ResourceLimits, UsageSnapshot, WorkAdmission, WorkMeter,
};
use core::cell::Cell;
use std::boxed::Box;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;

#[inline]
const fn mismatch_cell_index(cell: usize) -> usize {
    if cell < MISMATCH_CELL_COUNT {
        cell
    } else {
        MISMATCH_CELL_COUNT - 1
    }
}

/// Environment, working directory, and module search paths for one request.
///
/// This is host data the program reads (`env`, `get_prog_origin`, …). It is not charged to the ledger. Leave it unset
/// and those builtins answer empty (`env` → `{}`, no origin).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvironmentSnapshot {
    inner: Arc<EnvironmentInner>,
}

impl Default for EnvironmentSnapshot {
    fn default() -> Self {
        Self {
            inner: Arc::new(EnvironmentInner::default()),
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct EnvironmentInner {
    vars: Vec<(String, String)>,
    cwd: Option<String>,
    search_list: Vec<String>,
    jq_origin: Option<String>,
}

impl EnvironmentSnapshot {
    /// Build a snapshot.
    #[must_use]
    pub fn new(
        vars: Vec<(String, String)>,
        cwd: Option<String>,
        search_list: Vec<String>,
        jq_origin: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(EnvironmentInner {
                vars,
                cwd,
                search_list,
                jq_origin,
            }),
        }
    }

    /// `NAME=value` pairs, in the order they were given.
    #[must_use]
    pub fn vars(&self) -> &[(String, String)] {
        &self.inner.vars
    }

    /// Process working directory, if known.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.inner.cwd.as_deref()
    }

    /// The module search list.
    #[must_use]
    pub fn search_list(&self) -> &[String] {
        &self.inner.search_list
    }

    /// Directory of the running binary, if known.
    #[must_use]
    pub fn jq_origin(&self) -> Option<&str> {
        self.inner.jq_origin.as_deref()
    }
}

/// Where `stderr/0` writes.
///
/// The builtin still yields the value as its result. This sink is only the write to stderr. If the write fails, that is
/// a machine error — `try` cannot catch it.
pub trait StderrSink {
    /// Write one already-rendered line to stderr.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the host write fails.
    fn write_compact(&self, bytes: &[u8]) -> Result<(), ResourceError>;
}

/// Host data on a request: sinks, knobs, and counters that are not charged.
struct HostState<'a> {
    environment: Option<EnvironmentSnapshot>,
    stderr: Option<&'a dyn StderrSink>,
    diagnostics: Option<&'a dyn crate::diag::DiagnosticSink>,
    spill_store: Option<&'a dyn crate::spill::SpillStore>,
    extension: Option<Box<dyn core::any::Any>>,
    /// Times a calculation had to fall back from exact numbers to `f64`.
    precision_boundary_events: Cell<u64>,
    /// How many document subtrees decode left unbuilt.
    lazy_deferred_spans: Cell<u64>,
    /// How many of those unbuilt subtrees were later built for real.
    lazy_materialized_spans: Cell<u64>,
    /// How many values had to be rewritten to fit the output format, counted by [`ProjectionKind`].
    projection_events: [Cell<u64>; ProjectionKind::COUNT],
    /// Seed for `rand` and friends. `None` means use the system RNG.
    rand_seed_state: Cell<Option<u64>>,
    /// What to do on a missing key, a bad index, or a null operand.
    mismatch_policy: MismatchPolicy,
    /// If true, an edit through a YAML alias rewrites the shared anchor instead of refusing.
    edit_expand_alias: bool,
    /// True if an edit actually used that alias rewrite this run.
    edit_alias_expanded: Cell<bool>,
    /// If true, dates and times read as text, not their native kind.
    types_as_strings: bool,
    /// What to do with warning-severity diagnostics.
    strictness: StrictnessPolicy,
    /// How many of those warnings fired. Under `Strict`, any nonzero count fails the run.
    strictness_warnings: Cell<u64>,
    /// Nesting depth of `//` / `try` regions. Mismatch events stay quiet while this is above zero.
    mismatch_suppression: Cell<u32>,
    /// Per-kind mismatch counts for the warn report.
    mismatch_report: [Cell<u64>; MISMATCH_CELL_COUNT],
}

/// One request's ledger, cancellation, work budget, and the knobs that go with them.
///
/// Nested work borrows this. It does not get its own ledger or its own work budget.
pub struct ResourceContext<'a> {
    pub(crate) account: RequestAccount,
    control: &'a dyn Control,
    work: WorkMeter,
    host: HostState<'a>,
}

impl<'a> ResourceContext<'a> {
    /// Binds an empty or resumed request account to host control and a fresh cooperative work slice.
    ///
    /// # Errors
    ///
    /// Returns a control error if cancellation, the deadline, or the physical-memory ceiling is already observable.
    pub fn new(account: RequestAccount, control: &'a dyn Control, work: WorkMeter) -> Result<Self, ControlError> {
        check_control(control)?;
        Ok(Self {
            account,
            control,
            work,
            host: HostState {
                environment: None,
                stderr: None,
                diagnostics: None,
                spill_store: None,
                extension: None,
                precision_boundary_events: Cell::new(0),
                lazy_deferred_spans: Cell::new(0),
                lazy_materialized_spans: Cell::new(0),
                projection_events: [const { Cell::new(0) }; ProjectionKind::COUNT],
                rand_seed_state: Cell::new(None),
                mismatch_policy: MismatchPolicy::Lenient,
                edit_expand_alias: false,
                edit_alias_expanded: Cell::new(false),
                types_as_strings: false,
                strictness: StrictnessPolicy::Error,
                strictness_warnings: Cell::new(0),
                mismatch_suppression: Cell::new(0),
                mismatch_report: [const { Cell::new(0) }; MISMATCH_CELL_COUNT],
            },
        })
    }

    /// Attach environment, cwd, and search paths.
    ///
    /// Without this, `env` is `{}` and the origin builtins return null.
    #[must_use]
    pub fn with_environment(mut self, environment: EnvironmentSnapshot) -> Self {
        self.host.environment = Some(environment);
        self
    }

    /// The attached environment snapshot, if any.
    #[must_use]
    pub fn environment(&self) -> Option<&EnvironmentSnapshot> {
        self.host.environment.as_ref()
    }

    /// Attach stderr.
    ///
    /// Without this, `stderr/0` still yields the value but writes nothing.
    #[must_use]
    pub fn with_stderr(mut self, stderr: &'a dyn StderrSink) -> Self {
        self.host.stderr = Some(stderr);
        self
    }

    /// Attach a diagnostic sink.
    ///
    /// Without this, [`Self::record_diagnostic`] does nothing.
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: &'a dyn crate::diag::DiagnosticSink) -> Self {
        self.host.diagnostics = Some(diagnostics);
        self
    }

    /// The attached diagnostic sink, if any.
    #[must_use]
    pub fn diagnostics_sink(&self) -> Option<&'a dyn crate::diag::DiagnosticSink> {
        self.host.diagnostics
    }

    /// Same as [`Self::with_diagnostics`], for a context that is already built.
    ///
    /// The sink must outlive the context.
    pub fn set_diagnostics(&mut self, diagnostics: &'a dyn crate::diag::DiagnosticSink) {
        self.host.diagnostics = Some(diagnostics);
    }

    /// Send one diagnostic. Does nothing if no sink is attached.
    ///
    /// Never fails. The sink owns the accounting category of storage it retains; [`crate::diag::DiagnosticBuffer`] uses
    /// Diagnostic memory.
    pub fn record_diagnostic(&self, record: crate::diag::DiagnosticRecord<'_>) {
        if let Some(sink) = self.host.diagnostics {
            sink.record(record);
        }
    }

    /// Where overflow sort runs go on disk.
    #[must_use]
    pub fn with_spill_store(mut self, store: &'a dyn crate::spill::SpillStore) -> Self {
        self.host.spill_store = Some(store);
        self
    }

    /// The spill store, if the host attached one. Without it, sort stays in memory.
    #[must_use]
    pub fn spill_store(&self) -> Option<&'a dyn crate::spill::SpillStore> {
        self.host.spill_store
    }

    /// The attached stderr sink, if any.
    #[must_use]
    pub fn stderr_sink(&self) -> Option<&'a dyn StderrSink> {
        self.host.stderr
    }

    /// Stash one extra host object on the request (for example an input cursor).
    ///
    /// Recover it later with [`Self::host_extension`] and an `Any` downcast. This crate does not know the concrete
    /// type.
    #[must_use]
    pub fn with_host_extension(mut self, extension: Box<dyn core::any::Any>) -> Self {
        self.host.extension = Some(extension);
        self
    }

    /// Replace the extra host object.
    pub fn set_host_extension(&mut self, extension: Box<dyn core::any::Any>) {
        self.host.extension = Some(extension);
    }

    /// Take the extra host object, if any.
    pub fn take_host_extension(&mut self) -> Option<Box<dyn core::any::Any>> {
        self.host.extension.take()
    }

    /// Count one fallback from exact arithmetic to `f64`.
    ///
    /// If a sink is attached, also emit a diagnostic for that site. The count does not change the answer.
    pub fn note_precision_boundary(&self) {
        self.host
            .precision_boundary_events
            .set(self.host.precision_boundary_events.get().saturating_add(1));
        // Build the record only when a sink is installed: the registry lookup in `DiagnosticRecord::new` would
        // otherwise run on every crossing when no diagnostic sink is attached.
        if self.host.diagnostics.is_some() {
            let record = crate::diag::DiagnosticRecord::new_registered(crate::diag::codes::PRECISION_BOUNDARY);
            self.record_diagnostic(record);
        }
    }

    /// How many exact-to-`f64` fallbacks this run recorded.
    pub const fn precision_boundary_events(&self) -> u64 {
        self.host.precision_boundary_events.get()
    }

    /// How many document subtrees decode left unbuilt.
    pub const fn lazy_deferred_spans(&self) -> u64 {
        self.host.lazy_deferred_spans.get()
    }

    /// How many of those unbuilt subtrees were later built.
    pub const fn lazy_materialized_spans(&self) -> u64 {
        self.host.lazy_materialized_spans.get()
    }

    /// Record how many subtrees decode left unbuilt. A later call replaces the earlier one.
    pub fn set_lazy_deferred_spans(&self, deferred: u32) {
        self.host.lazy_deferred_spans.set(u64::from(deferred));
    }

    /// Clear the per-run counters.
    ///
    /// Call this when you reuse a context for another run, or the previous run's numbers leak through.
    pub fn reset_run_diagnostics(&self) {
        self.host.precision_boundary_events.set(0);
        self.host.lazy_deferred_spans.set(0);
        self.host.lazy_materialized_spans.set(0);
        for slot in &self.host.projection_events {
            slot.set(0);
        }
        self.host.edit_alias_expanded.set(false);
        self.host.strictness_warnings.set(0);
        self.host.mismatch_suppression.set(0);
        for slot in &self.host.mismatch_report {
            slot.set(0);
        }
    }

    /// Count one unbuilt subtree that was just built.
    pub fn bump_lazy_materialized_spans(&self) {
        self.host
            .lazy_materialized_spans
            .set(self.host.lazy_materialized_spans.get().saturating_add(1));
    }

    /// Count one value that had to be rewritten to fit the output format.
    ///
    /// If a sink is attached, also emit a diagnostic. Call this from the one place that does the rewrite, so nothing
    /// can rewrite silently.
    pub fn note_projection(&self, kind: ProjectionKind) {
        let slot = &self.host.projection_events[kind.index()];
        slot.set(slot.get().saturating_add(1));
        // Same lazy gate as `note_precision_boundary`: no sink, no record.
        if self.host.diagnostics.is_some() {
            let record = crate::diag::DiagnosticRecord::new_registered(kind.diagnostic_code());
            self.record_diagnostic(record);
        }
    }

    /// The kinds that actually fired, with counts. Empty if nothing was rewritten.
    pub fn projection_event_summary(&self) -> impl Iterator<Item = (ProjectionKind, u64)> + '_ {
        ProjectionKind::ALL
            .into_iter()
            .map(|kind| (kind, self.host.projection_events[kind.index()].get()))
            .filter(|(_, count)| *count > 0)
    }

    /// Seed `rand`, `randint`, `choice`, `sample`, and `shuffle`.
    ///
    /// A second run with the same seed produces the same numbers. Leave this unset and those builtins use the system
    /// RNG.
    #[must_use]
    pub fn with_rand_seed(mut self, seed: u64) -> Self {
        self.host.rand_seed_state = Cell::new(Some(seed));
        self
    }

    /// What to do on a missing key, a bad index, or a null operand.
    #[must_use]
    pub fn with_mismatch_policy(mut self, policy: MismatchPolicy) -> Self {
        self.host.mismatch_policy = policy;
        self
    }

    /// Current mismatch policy. Defaults to [`MismatchPolicy::Lenient`].
    #[must_use]
    #[inline]
    pub fn mismatch_policy(&self) -> MismatchPolicy {
        self.host.mismatch_policy
    }

    /// Allow an edit through a YAML alias to rewrite the shared anchor instead of refusing.
    #[must_use]
    pub fn with_edit_expand_alias(mut self, expand: bool) -> Self {
        self.host.edit_expand_alias = expand;
        self
    }

    /// Whether alias rewrites are allowed.
    #[must_use]
    #[inline]
    pub fn edit_expand_alias(&self) -> bool {
        self.host.edit_expand_alias
    }

    /// Mark that an alias rewrite actually happened.
    ///
    /// Safe to call more than once. The host warns once per request, not once per node.
    pub fn note_edit_alias_expansion(&self) {
        self.host.edit_alias_expanded.set(true);
    }

    /// Whether an alias rewrite happened this run.
    #[must_use]
    #[inline]
    pub fn edit_alias_expansion_engaged(&self) -> bool {
        self.host.edit_alias_expanded.get()
    }

    /// Read dates and times as text instead of their native kinds.
    #[must_use]
    pub fn with_types_as_strings(mut self, enabled: bool) -> Self {
        self.host.types_as_strings = enabled;
        self
    }

    /// Whether dates and times read as text.
    #[must_use]
    #[inline]
    pub fn types_as_strings(&self) -> bool {
        self.host.types_as_strings
    }

    /// What to do with warning-severity diagnostics.
    #[must_use]
    pub fn with_strictness(mut self, policy: StrictnessPolicy) -> Self {
        self.host.strictness = policy;
        self
    }

    /// Current strictness policy. Defaults to [`StrictnessPolicy::Error`].
    #[must_use]
    #[inline]
    pub fn strictness(&self) -> StrictnessPolicy {
        self.host.strictness
    }

    /// True when decode should accept the looser number spellings ([`StrictnessPolicy::Lenient`]).
    #[must_use]
    #[inline]
    pub fn decode_lenient(&self) -> bool {
        self.host.strictness == StrictnessPolicy::Lenient
    }

    /// Add `count` warnings that `Strict` can promote to a failed run.
    pub fn note_strictness_warnings(&self, count: u64) {
        self.host
            .strictness_warnings
            .set(self.host.strictness_warnings.get().saturating_add(count));
    }

    /// How many of those warnings fired.
    #[must_use]
    #[inline]
    pub fn strictness_warnings(&self) -> u64 {
        self.host.strictness_warnings.get()
    }

    /// About to evaluate a `//` operand or a `try` body: mismatch events inside should stay quiet. Pair with
    /// [`Self::exit_mismatch_suppression`].
    #[inline]
    pub fn enter_mismatch_suppression(&self) {
        self.host
            .mismatch_suppression
            .set(self.host.mismatch_suppression.get().saturating_add(1));
    }

    /// Leave one `//` / `try` region. Going below zero is a bug and stays at zero.
    #[inline]
    pub fn exit_mismatch_suppression(&self) {
        self.host
            .mismatch_suppression
            .set(self.host.mismatch_suppression.get().saturating_sub(1));
    }

    /// True while inside a `//` operand or `try` body.
    #[must_use]
    #[inline]
    pub fn mismatch_suppressed(&self) -> bool {
        self.host.mismatch_suppression.get() > 0
    }

    /// Count one mismatch for the warn report.
    ///
    /// `cell` is a row in [`crate::policy::MISMATCH_CELL_NAMES`]. The count will not wrap. An out-of-range row is a
    /// mapping bug: debug builds panic and release builds count it in the final row so the event remains visible.
    #[inline]
    pub fn note_mismatch(&self, cell: usize) {
        debug_assert!(
            cell < MISMATCH_CELL_COUNT,
            "mismatch cell {cell} outside the frozen table"
        );
        let slot = &self.host.mismatch_report[mismatch_cell_index(cell)];
        slot.set(slot.get().saturating_add(1));
    }

    /// Read the warn counts and zero them.
    ///
    /// Safe to call again on a reused context.
    #[must_use]
    pub fn take_mismatch_report(&self) -> [u64; MISMATCH_CELL_COUNT] {
        let mut taken = [0; MISMATCH_CELL_COUNT];
        for (slot, out) in self.host.mismatch_report.iter().zip(taken.iter_mut()) {
            *out = slot.get();
            slot.set(0);
        }
        taken
    }

    /// Take the current `rand` seed out.
    ///
    /// Put it back with [`Self::put_rand_seed_state`] after you use it. `None` means this request was never seeded —
    /// use the system RNG.
    pub fn take_rand_seed_state(&self) -> Option<u64> {
        self.host.rand_seed_state.take()
    }

    /// Put the `rand` seed back after a draw.
    pub fn put_rand_seed_state(&self, material: u64) {
        self.host.rand_seed_state.set(Some(material));
    }

    /// The extra host object, if any.
    #[must_use]
    pub fn host_extension(&self) -> Option<&dyn core::any::Any> {
        self.host.extension.as_deref()
    }

    /// The request's limits.
    #[must_use]
    #[inline]
    pub fn limits(&self) -> ResourceLimits {
        self.account.limits()
    }

    /// A copy of the counters right now.
    #[must_use]
    #[inline]
    pub fn snapshot(&self) -> UsageSnapshot {
        self.account.snapshot()
    }

    /// The ledger this context owns.
    ///
    /// Share it into [`crate::install`] so heap charges and grant holds land on the same cells.
    #[must_use]
    pub fn account(&self) -> &RequestAccount {
        &self.account
    }

    /// Reserve `bytes` of output before you write them.
    ///
    /// The reservation is not yet published. [`OutputPermit::commit`] keeps the prefix that went out; drop gives it all
    /// back.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the output limit or checked arithmetic rejects the proposed write.
    #[inline]
    pub fn reserve_output(&self, bytes: u64) -> Result<OutputPermit<'_>, ResourceError> {
        crate::account::AccountState::reserve_output(&self.account.state, bytes)
    }

    /// Count `bytes` of input the host just read.
    ///
    /// Charge this where the bytes are acquired (a file, a pipe), not in every drive that later borrows the same slice.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the input ceiling is exhausted.
    #[inline]
    pub fn charge_input(&self, bytes: u64) -> Result<(), ResourceError> {
        self.account.charge_input(bytes)
    }

    /// Count spill-file bytes, just before the write.
    ///
    /// A `0` ceiling (the default) means no cap; the charge still records.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the disk ceiling is set and exhausted.
    #[inline]
    pub fn charge_spill_disk(&self, bytes: u64) -> Result<(), ResourceError> {
        self.account.charge_spill_disk(bytes)
    }

    /// Enter one nesting level. Drop the guard to leave it.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the nesting-depth limit is exhausted.
    #[inline]
    pub fn enter_nesting(&self) -> Result<DepthGuard<'_>, ResourceError> {
        crate::account::AccountState::enter_depth(&self.account.state)
    }

    /// Enter one nesting level with a guard that can live on stored frames.
    ///
    /// # Errors
    ///
    /// Returns a resource error when the nesting-depth limit is exhausted or the guard's account handle cannot be
    /// retained.
    #[inline]
    pub fn enter_nesting_owned(&self) -> Result<OwnedDepthGuard, ResourceError> {
        crate::account::AccountState::enter_depth_owned(&self.account.state)
    }

    /// Ask the host whether this request may keep running.
    ///
    /// # Errors
    ///
    /// Returns a control error when the host observes cancellation, the deadline, or the host memory ceiling.
    #[inline]
    pub fn check_control(&self) -> Result<(), ControlError> {
        check_control(self.control)
    }

    /// Ask for up to `remaining` bytes of work on this slice.
    ///
    /// # Errors
    ///
    /// Returns a control error when the host observes cancellation, the deadline, or the host memory ceiling.
    #[inline]
    pub fn admit_work_bytes(&mut self, remaining: usize) -> Result<WorkAdmission, ControlError> {
        let admission = self.work.admit_linear_items(remaining);
        self.after_admission(admission)
    }

    /// Ask for one step (token, node, record, …).
    ///
    /// # Errors
    ///
    /// Returns a control error when the host observes cancellation, the deadline, or the host memory ceiling.
    #[inline]
    pub fn admit_work_transition(&mut self) -> Result<WorkAdmission, ControlError> {
        let admission = self.work.admit_transition();
        self.after_admission(admission)
    }

    /// Ask for up to `remaining` steps.
    ///
    /// # Errors
    ///
    /// Returns a control error when the host observes cancellation, the deadline, or the host memory ceiling.
    #[inline]
    pub fn admit_work_transitions(&mut self, remaining: usize) -> Result<WorkAdmission, ControlError> {
        let admission = self.work.admit_transitions(remaining);
        self.after_admission(admission)
    }

    /// Work budget left in this slice.
    #[must_use]
    #[inline]
    pub const fn remaining_work(&self) -> u32 {
        self.work.remaining()
    }

    /// Puts unused budget from an early-finished grant back on the meter.
    #[inline]
    pub fn refund_work(&mut self, unused: u32) {
        self.work.refund(unused);
    }

    /// If the meter said `Pending`, check cancel / deadline / memory.
    #[inline]
    fn after_admission(&self, admission: WorkAdmission) -> Result<WorkAdmission, ControlError> {
        if admission == WorkAdmission::Pending {
            check_control(self.control)?;
        }
        Ok(admission)
    }

    /// Start the next work slice with a fresh budget.
    ///
    /// Leftover budget from an early return is replaced, not added. Checks cancellation first, then whether a memory
    /// charge already tripped. Returns `Ok(false)` if `budget` is 0 or more than 4096.
    ///
    /// # Errors
    ///
    /// Returns a control error when the host observes cancellation, the deadline, or the physical-memory ceiling, or
    /// the latched charge failure when the ambient trip latch is set.
    #[inline]
    pub fn try_begin_next_cooperative_entry(&mut self, budget: u32) -> Result<bool, CooperativeError> {
        self.check_control().map_err(CooperativeError::Control)?;
        crate::ambient::cooperative_refusal().map_err(CooperativeError::Memory)?;
        Ok(self.work.try_replenish(budget))
    }
}

/// Why a work-slice pause stopped: the host said stop, or memory already tripped.
///
/// Check this instead of allocating again — the next `Vec::push` would abort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CooperativeError {
    /// Cancelled, past the deadline, or over the host memory ceiling.
    Control(ControlError),
    /// A heap charge already hit the request memory ceiling.
    Memory(ResourceError),
}

impl core::fmt::Display for CooperativeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Control(error) => core::fmt::Display::fmt(error, formatter),
            Self::Memory(error) => core::fmt::Display::fmt(error, formatter),
        }
    }
}

impl core::error::Error for CooperativeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Control(error) => Some(error),
            Self::Memory(error) => Some(error),
        }
    }
}

#[inline]
fn check_control(control: &dyn Control) -> Result<(), ControlError> {
    match control.check() {
        ControlOutcome::Continue => Ok(()),
        ControlOutcome::Cancelled => Err(ControlError::Cancelled),
        ControlOutcome::DeadlineExceeded => Err(ControlError::DeadlineExceeded),
        ControlOutcome::MemoryExceeded => Err(ControlError::MemoryExceeded),
    }
}

#[cfg(test)]
mod tests {
    use super::{ResourceContext, mismatch_cell_index};
    use crate::policy::ProjectionKind;
    use crate::{ContinueControl, RequestAccount, ResourceLimits, WorkMeter};

    static CONTROL: ContinueControl = ContinueControl;

    fn context() -> ResourceContext<'static> {
        ResourceContext::new(
            RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64)).expect("account"),
            &CONTROL,
            WorkMeter::try_new_v1(4096).expect("work"),
        )
        .expect("context")
    }

    #[test]
    fn set_lazy_deferred_spans_is_last_write() {
        let resources = context();
        resources.set_lazy_deferred_spans(10);
        resources.set_lazy_deferred_spans(3);
        assert_eq!(resources.lazy_deferred_spans(), 3);
    }

    #[test]
    fn reset_run_diagnostics_clears_the_per_run_counters() {
        let resources = context();
        resources.set_lazy_deferred_spans(4);
        resources.bump_lazy_materialized_spans();
        resources.note_precision_boundary();
        for kind in ProjectionKind::ALL {
            resources.note_projection(kind);
        }
        resources.note_edit_alias_expansion();
        resources.note_strictness_warnings(2);
        resources.enter_mismatch_suppression();
        resources.note_mismatch(0);
        resources.reset_run_diagnostics();
        assert_eq!(resources.lazy_deferred_spans(), 0);
        assert_eq!(resources.lazy_materialized_spans(), 0);
        assert_eq!(resources.precision_boundary_events(), 0);
        assert!(resources.projection_event_summary().next().is_none());
        assert!(!resources.edit_alias_expansion_engaged());
        assert_eq!(resources.strictness_warnings(), 0);
        assert!(!resources.mismatch_suppressed());
        assert!(resources.take_mismatch_report().iter().all(|count| *count == 0));
    }

    #[test]
    fn mismatch_suppression_exit_at_zero_stays_at_zero() {
        let resources = context();
        resources.exit_mismatch_suppression();
        assert!(!resources.mismatch_suppressed());
    }

    #[test]
    fn an_out_of_range_mismatch_cell_maps_to_the_final_row() {
        assert_eq!(mismatch_cell_index(usize::MAX), crate::policy::MISMATCH_CELL_COUNT - 1);
    }

    /// Pins [`ResourceContext::note_mismatch`]'s debug out-of-range contract.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "outside the frozen table")]
    fn an_out_of_range_mismatch_cell_trips_debug() {
        let resources = context();
        resources.note_mismatch(usize::MAX);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn an_out_of_range_mismatch_cell_counts_in_the_final_row_in_release() {
        let resources = context();
        resources.note_mismatch(usize::MAX);
        let report = resources.take_mismatch_report();
        assert_eq!(report[crate::policy::MISMATCH_CELL_COUNT - 1], 1);
    }
}
