//! Walking the request ledger: limits, logical charges, snapshots.
//!
//! Share + `install` is how heap charges join this ledger when a counting allocator is installed. This walk charges
//! what the allocator cannot see: spill-disk, output permits, nesting, and a ceiling refuse.
//!
//! Run with `cargo run -p jqf-resource --example account`.

use jqf_resource::{
    ContinueControl, RequestAccount, ResourceContext, ResourceError, ResourceLimit, ResourceLimits, WorkMeter, install,
};

static CONTROL: ContinueControl = ContinueControl;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // One request policy: input/output bytes, aggregate memory, spill bytes, and nesting levels. The spill-disk ceiling
    // is opt-in via the builder.
    let limits = ResourceLimits::new(u64::MAX, 4096, 1 << 20, u64::MAX, 100);
    let resources = ResourceContext::new(
        RequestAccount::try_new(limits)?,
        &CONTROL,
        WorkMeter::try_new_v1(4096).ok_or("work meter")?,
    )?;
    // The ambient slot needs an owned handle; sharing is how this account becomes the thread's accountant without a
    // second ledger.
    let _scope = install(resources.account().try_share()?);

    // The spill-disk ceiling is a LOGICAL dimension: bytes written to disk, which the allocator can never see. Charging
    // the run's encoded size before the write is the only accounting it gets.
    resources.charge_spill_disk(1024)?;
    assert_eq!(resources.snapshot().spill_disk_bytes(), 1024);

    // Output publication is authorized before bytes are written; committing records the actual published prefix.
    let permit = resources.reserve_output(512)?;
    permit.commit(128)?;
    assert_eq!(resources.snapshot().output_bytes(), 128);
    assert_eq!(resources.snapshot().output_reserved_bytes(), 0);

    // A charge that would pass the ceiling refuses before mutation.
    let refused = resources.reserve_output(4096);
    assert!(matches!(
        refused,
        Err(ResourceError::LimitExceeded {
            limit_kind: ResourceLimit::OutputBytes,
            ..
        })
    ));
    assert_eq!(resources.snapshot().output_bytes(), 128);

    // Structural nesting is bounded by the depth ceiling, released on drop.
    let guard = resources.enter_nesting()?;
    assert_eq!(resources.snapshot().nesting_depth(), 1);
    drop(guard);
    assert_eq!(resources.snapshot().nesting_depth(), 0);

    let final_snapshot = resources.snapshot();
    println!("ledger walk: ok ({final_snapshot:?})");
    Ok(())
}
