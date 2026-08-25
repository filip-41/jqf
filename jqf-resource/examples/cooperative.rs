//! Cooperative work slices and one linear worker grant, end to end.
//!
//! Run with `cargo run -p jqf-resource --example cooperative`.

use jqf_resource::task::{TaskGrantLimits, TaskOutputBuffer};
use jqf_resource::{ContinueControl, RequestAccount, ResourceContext, ResourceLimits, WorkAdmission, WorkMeter};

static CONTROL: ContinueControl = ContinueControl;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let limits = ResourceLimits::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX, 64);
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(limits)?,
        &CONTROL,
        WorkMeter::try_new_v1(4096).ok_or("work meter")?,
    )?;

    // One cooperative entry admits bounded quanta; exhaustion is `Pending`, never a resource failure, and the next
    // entry starts a fresh budget.
    resources.try_begin_next_cooperative_entry(8)?;
    let mut processed = 0_u64;
    while let WorkAdmission::Granted(count) = resources.admit_work_transitions(usize::MAX)? {
        processed += count as u64;
    }
    assert_eq!(processed, 8);

    // One linear worker grant: the budget carries only numbers to a child ledger, and the child detaches exactly one
    // result allocation.
    let envelope = TaskGrantLimits {
        retained_bytes: RequestAccount::minimum_memory_bytes() + 128,
        working_bytes: 256,
        pending_io_bytes: 128,
        output_bytes: 128,
    };
    let (reservation, budget) = resources.reserve_task_grant(envelope)?;
    let child = budget.open_child()?;
    let mut child = child.bind(&CONTROL, WorkMeter::try_new_v1(4096).ok_or("work meter")?)?;
    let mut output = TaskOutputBuffer::try_with_capacity(5, &mut child)?;
    output.try_append_within_capacity(b"hello")?;
    let detached = child.detach_result(output)?;

    // Adoption moves the worker's bytes into a parent-owned handle.
    let adopted = reservation.adopt(detached)?;
    assert_eq!(adopted.as_slice(), b"hello");
    drop(adopted);
    println!("cooperative walk: ok");
    Ok(())
}
