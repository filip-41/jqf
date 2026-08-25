use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;

const GENEROUS_LIMITS: ResourceLimits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, u32::MAX);

/// A generously limited request context for tests that do not probe exhaustion.
pub fn resources() -> ResourceContext<'static> {
    resources_with_work_credits(4096)
}

/// A request context with a specific initial cooperative work budget.
pub fn resources_with_work_credits(credits: u32) -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(GENEROUS_LIMITS).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(credits).expect("work"),
    )
    .expect("context")
}
