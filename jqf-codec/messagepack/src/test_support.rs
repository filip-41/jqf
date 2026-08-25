use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;

pub(crate) fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(1 << 30, 1 << 30, 1 << 30, 1 << 30, 10_000)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(4_096).expect("work"),
    )
    .expect("resources")
}
