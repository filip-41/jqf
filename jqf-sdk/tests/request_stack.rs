//! The documented nesting refusal must land as an error on a sized thread,
//! never as a process abort. [`Request`](jqf_sdk::Request) is `!Send`, so
//! this test spawns first and compiles on that thread.

use jqf_sdk::{
    ContinueControl, DiagnosticPolicy, RequestAccount, ResourceContext, ResourceLimits, ValidationMode, WorkMeter,
    try_compile_program,
};

#[test]
fn deep_nesting_refuses_on_a_sized_thread() {
    let stack = jqf_sdk::request_stack_bytes().expect("request stack size");
    std::thread::Builder::new()
        .name("jqf-request-stack-test".into())
        .stack_size(stack)
        .spawn(|| {
            let control = ContinueControl;
            let resources = ResourceContext::new(
                RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, 0, u32::MAX))
                    .expect("account allocates"),
                &control,
                WorkMeter::try_new_v1(64).expect("work meter starts"),
            )
            .expect("resources start");
            let policy = jqf_sdk::CodecRequirementPolicy::new(ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly);
            let deep = format!("{} .{}", "(".repeat(2000), ")".repeat(2000));
            try_compile_program(&deep, policy, &resources).expect("2000 groups must compile on the sized thread");

            let past = format!("{} .{}", "(".repeat(10_000), ")".repeat(10_000));
            let error = try_compile_program(&past, policy, &resources)
                .expect_err("10_000 groups must refuse at the nesting ceiling");
            let text = format!("{error}");
            assert!(
                text.contains("nesting depth limit exceeded"),
                "expected the nesting-depth refusal, got {text}"
            );
        })
        .expect("sized thread starts")
        .join()
        .expect("sized thread must not abort");
}
