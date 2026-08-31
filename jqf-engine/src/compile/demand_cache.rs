/// Regression guard: count, element, keys, and type demands are compile-time
/// constants of the immutable program, so the per-record consultation pattern
/// of the sequence drive must never re-derive them. The derivation counters
/// live at the derivation sites; this test pins the finish-time cache,
/// including the cached `type_path` / [`CompiledProgram::type_demand`] row.
use super::*;
use core::sync::atomic::Ordering;

fn resources() -> ResourceContext<'static> {
    let account = jqf_resource::RequestAccount::try_new(jqf_resource::ResourceLimits::new(
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u64::MAX,
        u32::MAX,
    ))
    .expect("account");
    let work = jqf_resource::WorkMeter::try_new_v1(4096).expect("work");
    jqf_resource::ResourceContext::new(account, &jqf_resource::ContinueControl, work).expect("resources")
}

#[test]
fn demands_are_derived_once_per_program_not_per_record() {
    // The sequence drive's per-record pattern: `count_answer` consults
    // the count demand on every record, `element_answer` consults the
    // element demand when the count declines, and the residual run probes
    // both again when both fast paths decline. A multi-record input
    // therefore consults each accessor several times per record; none of
    // those consultations may re-run the derivation.
    let resources = resources();
    let policy = crate::codec_requirement::CodecRequirementPolicy::new(
        jqf_codec_core::ValidationMode::Strict,
        jqf_codec_core::DiagnosticPolicy::ErrorsOnly,
    );
    let compiled = try_compile_program("[.catalog[] | .id] | length", policy, CompileOptions::new(), &resources)
        .expect("compiles");
    // Hold the derivation lock across the whole snapshot-to-assertion
    // window: a concurrent test's compile must not move the counters
    // mid-assertion (the counters are shared across the test binary).
    let guard = super::DERIVATION_LOCK.lock().expect("derivation lock");
    let count_before = super::COUNT_DEMAND_DERIVATIONS.load(Ordering::Relaxed);
    let element_before = super::ELEMENT_DEMAND_DERIVATIONS.load(Ordering::Relaxed);
    for _ in 0..64 {
        // count_answer's consultation.
        let _ = compiled.count_demand();
        // element_answer's consultation (count declined).
        let _ = compiled.element_demand();
        // finish-cached route rows the explain plan now surfaces.
        let _ = compiled.keys_demand();
        let _ = compiled.type_demand();
        // The residual run's decline probes (both fast paths declined).
        let _ = compiled.count_demand().is_some();
        let _ = compiled.element_demand().is_some();
    }
    assert_eq!(
        super::COUNT_DEMAND_DERIVATIONS.load(Ordering::Relaxed),
        count_before,
        "the count demand re-derived per record consultation"
    );
    assert_eq!(
        super::ELEMENT_DEMAND_DERIVATIONS.load(Ordering::Relaxed),
        element_before,
        "the element demand re-derived per record consultation"
    );
    drop(guard);
}
