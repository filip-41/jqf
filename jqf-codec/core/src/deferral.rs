//! Container-subtree deferral: validate first, materialize a container only when touched.
//!
//! `jqf-data` owns the span-backed node. This module owns depth and committed- span accounting. Each codec owns reading
//! its own span text back into a value. Nested materialization failures map through [`crate::error`].

use core::sync::atomic::{AtomicU64, Ordering};

/// How many container spans this process has committed, across every codec.
static COMMITTED_SPANS: AtomicU64 = AtomicU64::new(0);

/// How many decodes DECLINED deferral after asking for it, across every codec.
static DECLINED_DECODES: AtomicU64 = AtomicU64::new(0);

// The container-span frontier is not a process global: it travels with each access requirement (the engine's root
// lowering names it from the program class or the policy override), so concurrent requests cannot race a shared knob
// and library callers get the lazy default without touching this module. Only the observability counters remain.

/// Returns how many container spans this process has PUBLISHED so far.
///
/// Dark-launch instrumentation, not a product signal: it exists so the standing force-lazy differential can prove the
/// lazy arm ACTUALLY took the lazy path rather than passing by doing nothing at all.
#[doc(hidden)]
#[must_use]
pub fn committed_container_spans() -> u64 {
    COMMITTED_SPANS.load(Ordering::Relaxed)
}

/// Records the container spans one PUBLISHED document holds.
///
/// Counted at publication rather than at each commit, so a decoder that DECLINES deferral part-way and re-decodes
/// eagerly does not leave the spans of the abandoned attempt in the count. The number therefore always describes
/// documents a reader can actually meet, which is what makes it usable as a differential's engagement evidence.
#[doc(hidden)]
pub fn record_published_spans(count: u64) {
    if count > 0 {
        COMMITTED_SPANS.fetch_add(count, Ordering::Relaxed);
    }
}

/// Returns how many decodes DECLINED deferral after being asked for it.
///
/// The counterpart to [`committed_container_spans`], and the more important of the two for judging the mechanism: spans
/// say the frontier engaged somewhere, declines say how often a real document defeated it. A decoder that declines on
/// most inputs is a mechanism that does not generalize, and that is a fact about documents rather than about the code,
/// so it has to be VERIFIED over a population rather than argued.
#[doc(hidden)]
#[must_use]
pub fn declined_deferrals() -> u64 {
    DECLINED_DECODES.load(Ordering::Relaxed)
}

/// Records one decode that asked for deferral and gave it up.
#[doc(hidden)]
pub fn record_declined_deferral() {
    DECLINED_DECODES.fetch_add(1, Ordering::Relaxed);
}
