use core::num::NonZeroU32;
use std::sync::mpsc;
use std::thread;

use jqf_resource::task::{TaskChildAccount, TaskGrantLimits, TaskOutputBuffer};
use jqf_resource::{
    ContinueControl, Control, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits, WorkMeter,
};
use jqf_runtime::workers::{MorselByteRange, MorselOutcome};
use jqf_runtime::workers::{
    NativeWorkerControl, NativeWorkerHost, OrderedRecordCoordinator, OrderedRecordCoordinatorError,
    OrderedRecordDescriptorContract, OrderedRecordDispatch, OrderedRecordPoll, OrderedRecordTaskOutput,
    RecordConcurrencyWindow,
};

static CONTINUE: ContinueControl = ContinueControl;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestTerminal {
    Malformed,
}

fn work() -> WorkMeter {
    WorkMeter::try_new_v1(4_096).expect("work meter")
}

fn resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, u64::MAX, 64))
            .expect("request account"),
        &CONTINUE,
        work(),
    )
    .expect("resources")
}

fn grant_limits(output_bytes: u64) -> TaskGrantLimits {
    TaskGrantLimits {
        retained_bytes: RequestAccount::minimum_memory_bytes() + (64 << 10),
        working_bytes: 64 << 10,
        pending_io_bytes: 64 << 10,
        output_bytes,
    }
}

fn window(records: u32) -> RecordConcurrencyWindow {
    RecordConcurrencyWindow::try_new(NonZeroU32::new(records).expect("nonzero records"), 1 << 20, 1 << 20)
        .expect("nonempty window")
}

fn descriptor_contract() -> OrderedRecordDescriptorContract<MorselOutcome, TestTerminal> {
    OrderedRecordDescriptorContract::new(
        |descriptor, buffer_len| descriptor.fits_buffer(buffer_len),
        |terminal, buffer_len| match terminal {
            TestTerminal::Malformed => buffer_len == Some(3),
        },
    )
}

fn record_output(
    ordinal: u64,
    child: TaskChildAccount,
    control: &NativeWorkerControl,
) -> OrderedRecordTaskOutput<MorselOutcome, TestTerminal> {
    let mut child = child.bind(control, work()).expect("child resources");
    let bytes = ordinal.to_string();
    let mut output = TaskOutputBuffer::try_with_capacity(bytes.len(), &mut child).expect("output allocation");
    output
        .try_append_within_capacity(bytes.as_bytes())
        .expect("output bytes");
    let descriptor = MorselOutcome::new(
        MorselByteRange::try_new(0, bytes.len() as u64).expect("output range"),
        1,
        1,
    );
    let detached = child.detach_result(output).expect("detach output");
    OrderedRecordTaskOutput::record(descriptor, detached)
}

fn terminal_output(
    child: TaskChildAccount,
    control: &NativeWorkerControl,
) -> OrderedRecordTaskOutput<MorselOutcome, TestTerminal> {
    let mut child = child.bind(control, work()).expect("child resources");
    let mut output = TaskOutputBuffer::try_with_capacity(3, &mut child).expect("diagnostic buffer");
    output.try_append_within_capacity(b"bad").expect("diagnostic bytes");
    let detached = child.detach_result(output).expect("detach diagnostic");
    OrderedRecordTaskOutput::terminal(TestTerminal::Malformed, Some(detached))
}

fn assert_zero(coordinator: &OrderedRecordCoordinator<'_, '_, MorselOutcome, TestTerminal>) {
    let snapshot = coordinator.snapshot();
    assert_eq!(snapshot.in_flight_records(), 0);
    assert_eq!(snapshot.ready_results(), 0);
}

#[test]
fn concurrency_window_rejects_empty_byte_dimensions() {
    let one = NonZeroU32::new(1).expect("one");
    assert!(RecordConcurrencyWindow::try_new(one, 0, 1).is_none());
    assert!(RecordConcurrencyWindow::try_new(one, 1, 0).is_none());
    assert!(RecordConcurrencyWindow::try_new(one, 2, 3).is_some());
}

#[test]
fn every_window_dimension_stops_dispatch_before_overcommit() {
    let resources = resources();
    let baseline = resources.snapshot();
    let host = NativeWorkerHost::new(3);
    host.scope(|scope| {
        let window =
            RecordConcurrencyWindow::try_new(NonZeroU32::new(2).expect("two records"), 10, 10).expect("window");
        let mut coordinator =
            OrderedRecordCoordinator::try_new(scope, window, descriptor_contract()).expect("coordinator");
        let (release, wait) = mpsc::channel::<()>();

        assert_eq!(
            coordinator
                .try_dispatch(
                    scope,
                    6,
                    grant_limits(6),
                    &resources,
                    move |ordinal, budget, control| {
                        while control.check() == ControlOutcome::Continue && wait.try_recv().is_err() {
                            thread::yield_now();
                        }
                        if control.check() != ControlOutcome::Continue {
                            return OrderedRecordTaskOutput::cancelled();
                        }
                        record_output(ordinal, budget, control)
                    },
                )
                .expect("first dispatch"),
            OrderedRecordDispatch::Started { ordinal: 0 }
        );
        assert_eq!(
            coordinator
                .try_dispatch(scope, 5, grant_limits(1), &resources, record_output,)
                .expect("input bound"),
            OrderedRecordDispatch::WindowFull
        );
        assert_eq!(
            coordinator
                .try_dispatch(scope, 1, grant_limits(5), &resources, record_output,)
                .expect("result bound"),
            OrderedRecordDispatch::WindowFull
        );
        assert_eq!(
            coordinator
                .try_dispatch(scope, 1, grant_limits(1), &resources, record_output,)
                .expect("second dispatch"),
            OrderedRecordDispatch::Started { ordinal: 1 }
        );
        assert_eq!(
            coordinator
                .try_dispatch(scope, 1, grant_limits(1), &resources, record_output,)
                .expect("record bound"),
            OrderedRecordDispatch::WindowFull
        );

        coordinator.cancel_and_drain();
        drop(release);
        assert_zero(&coordinator);
        assert_eq!(scope.available_permits(), 3);
        drop(coordinator);
    });
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
fn descriptor_ranges_are_checked_after_adoption_before_entering_the_ring() {
    let resources = resources();
    let baseline = resources.snapshot();
    NativeWorkerHost::new(1).scope(|scope| {
        let mut coordinator =
            OrderedRecordCoordinator::try_new(scope, window(1), descriptor_contract()).expect("coordinator");
        coordinator
            .try_dispatch(scope, 1, grant_limits(4), &resources, |_ordinal, child, control| {
                let mut child = child.bind(control, work()).expect("child resources");
                let mut output = TaskOutputBuffer::try_with_capacity(1, &mut child).expect("output");
                output.try_append_within_capacity(b"x").expect("output byte");
                let detached = child.detach_result(output).expect("detach");
                let invalid = MorselOutcome::new(MorselByteRange::try_new(0, 2).expect("range"), 1, 1);
                OrderedRecordTaskOutput::record(invalid, detached)
            })
            .expect("dispatch");

        let error = loop {
            match coordinator.poll_next() {
                Ok(OrderedRecordPoll::Pending) => thread::yield_now(),
                Err(error) => break error,
                _ => panic!("invalid descriptor must not enter the ring"),
            }
        };
        assert!(matches!(
            error,
            OrderedRecordCoordinatorError::InternalContract("record descriptor byte range exceeds its adopted buffer")
        ));
        assert_zero(&coordinator);
        assert_eq!(scope.available_permits(), 1);
        assert!(scope.is_cancelled());
        drop(coordinator);
    });
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the jobs differential keeps the shared scheduling proof identical for 1, 2, and 4"
)]
fn jobs_one_two_and_four_release_only_the_contiguous_ordinal_prefix() {
    for jobs in [1_u32, 2, 4] {
        let resources = resources();
        let baseline = resources.snapshot();
        let host = NativeWorkerHost::new(jobs as usize);
        host.scope(|scope| {
            let mut coordinator =
                OrderedRecordCoordinator::try_new(scope, window(jobs), descriptor_contract()).expect("coordinator");
            let (release_zero, wait_zero) = mpsc::channel::<()>();
            let (completed, completion) = mpsc::channel();

            let first = coordinator
                .try_dispatch(
                    scope,
                    8,
                    grant_limits(16),
                    &resources,
                    move |ordinal, budget, control| {
                        assert_eq!(ordinal, 0);
                        wait_zero.recv().expect("release ordinal zero");
                        let output = record_output(ordinal, budget, control);
                        completed.send(ordinal).expect("completion observed");
                        output
                    },
                )
                .expect("dispatch zero");
            assert_eq!(first, OrderedRecordDispatch::Started { ordinal: 0 });

            let (later_completed, later_completion) = mpsc::channel();
            for expected in 1..u64::from(jobs) {
                let sender = later_completed.clone();
                let dispatched = coordinator
                    .try_dispatch(
                        scope,
                        8,
                        grant_limits(16),
                        &resources,
                        move |ordinal, budget, control| {
                            assert_eq!(ordinal, expected);
                            let output = record_output(ordinal, budget, control);
                            sender.send(ordinal).expect("later completion observed");
                            output
                        },
                    )
                    .expect("dispatch later record");
                assert_eq!(dispatched, OrderedRecordDispatch::Started { ordinal: expected });
            }
            drop(later_completed);
            for _ in 1..jobs {
                later_completion.recv().expect("later worker completes");
            }

            if jobs > 1 {
                assert!(matches!(
                    coordinator.poll_next().expect("poll held prefix"),
                    OrderedRecordPoll::Pending
                ));
            }

            release_zero.send(()).expect("release zero");
            completion.recv().expect("zero completes");
            coordinator.finish_dispatch();

            let mut released = Vec::new();
            loop {
                match coordinator.poll_next().expect("ordered poll") {
                    OrderedRecordPoll::Pending => thread::yield_now(),
                    OrderedRecordPoll::Ready(ready) => {
                        let ordinal = ready.ordinal();
                        assert_eq!(ready.buffer().as_slice(), ordinal.to_string().as_bytes());
                        let range = ready.descriptor().bytes();
                        assert_eq!(range.start(), 0);
                        assert_eq!(range.end(), ready.buffer().len() as u64);
                        released.push(ordinal);
                        coordinator.acknowledge_ready(&resources).expect("acknowledge");
                    }
                    OrderedRecordPoll::Terminal(_) => panic!("fixture has no terminal"),
                    OrderedRecordPoll::Complete => break,
                }
            }
            assert_eq!(released, (0..u64::from(jobs)).collect::<Vec<_>>());
            assert_zero(&coordinator);
            assert!(!scope.is_cancelled());
            drop(coordinator);
            assert_eq!(scope.available_permits(), jobs as usize);
            assert!(
                !scope.is_cancelled(),
                "dropping a drained coordinator must not poison the native scope"
            );
        });
        assert_eq!(
            resources.snapshot().memory_current_bytes(),
            baseline.memory_current_bytes()
        );
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the ordered-terminal race keeps setup, held frontier, release, and cleanup in one proof"
)]
fn later_terminal_is_held_until_slow_ordinal_zero_is_acknowledged() {
    let resources = resources();
    let baseline = resources.snapshot();
    let host = NativeWorkerHost::new(2);
    host.scope(|scope| {
        let mut coordinator =
            OrderedRecordCoordinator::try_new(scope, window(2), descriptor_contract()).expect("coordinator");
        let (release_zero, wait_zero) = mpsc::channel();
        let (terminal_done, terminal_completion) = mpsc::channel();

        assert_eq!(
            coordinator
                .try_dispatch(
                    scope,
                    8,
                    grant_limits(16),
                    &resources,
                    move |ordinal, budget, control| {
                        wait_zero.recv().expect("release zero");
                        record_output(ordinal, budget, control)
                    },
                )
                .expect("dispatch zero"),
            OrderedRecordDispatch::Started { ordinal: 0 }
        );
        assert_eq!(
            coordinator
                .try_dispatch(
                    scope,
                    8,
                    grant_limits(16),
                    &resources,
                    move |ordinal, budget, control| {
                        assert_eq!(ordinal, 1);
                        let output = terminal_output(budget, control);
                        terminal_done.send(()).expect("terminal completion");
                        output
                    },
                )
                .expect("dispatch terminal"),
            OrderedRecordDispatch::Started { ordinal: 1 }
        );
        terminal_completion.recv().expect("terminal worker completes");

        loop {
            assert!(matches!(
                coordinator.poll_next().expect("hold terminal"),
                OrderedRecordPoll::Pending
            ));
            if coordinator.snapshot().ready_results() == 1 {
                break;
            }
            thread::yield_now();
        }
        assert_eq!(
            coordinator
                .try_dispatch(scope, 1, grant_limits(1), &resources, record_output,)
                .expect("closed dispatch"),
            OrderedRecordDispatch::Closed
        );
        assert!(!scope.is_cancelled(), "slow earlier work must stay live");

        release_zero.send(()).expect("release zero");
        loop {
            match coordinator.poll_next().expect("zero poll") {
                OrderedRecordPoll::Pending => thread::yield_now(),
                OrderedRecordPoll::Ready(ready) => {
                    assert_eq!(ready.ordinal(), 0);
                    break;
                }
                _ => panic!("terminal passed ordinal zero"),
            }
        }
        coordinator.acknowledge_ready(&resources).expect("acknowledge zero");

        let terminal = loop {
            match coordinator.poll_next().expect("terminal poll") {
                OrderedRecordPoll::Pending => thread::yield_now(),
                OrderedRecordPoll::Terminal(terminal) => break terminal,
                _ => panic!("expected ordered terminal"),
            }
        };
        assert_eq!(terminal.ordinal(), 1);
        assert_eq!(terminal.descriptor(), &TestTerminal::Malformed);
        assert_eq!(terminal.buffer().expect("terminal diagnostic").as_slice(), b"bad");
        drop(terminal);
        assert_zero(&coordinator);
        assert!(scope.is_cancelled());
        assert_eq!(scope.available_permits(), 2);
        drop(coordinator);
    });
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the backpressure race keeps offer, paused join, acknowledgement, and cleanup together"
)]
fn pending_output_offer_pauses_join_and_dispatch_until_acknowledged() {
    let resources = resources();
    let baseline = resources.snapshot();
    let host = NativeWorkerHost::new(2);
    host.scope(|scope| {
        let mut coordinator =
            OrderedRecordCoordinator::try_new(scope, window(2), descriptor_contract()).expect("coordinator");
        let (release_one, wait_one) = mpsc::channel();
        let (zero_done, zero_completion) = mpsc::channel();
        let (one_done, one_completion) = mpsc::channel();

        coordinator
            .try_dispatch(
                scope,
                8,
                grant_limits(16),
                &resources,
                move |ordinal, budget, control| {
                    let output = record_output(ordinal, budget, control);
                    zero_done.send(()).expect("zero done");
                    output
                },
            )
            .expect("dispatch zero");
        coordinator
            .try_dispatch(
                scope,
                8,
                grant_limits(16),
                &resources,
                move |ordinal, budget, control| {
                    wait_one.recv().expect("release one");
                    let output = record_output(ordinal, budget, control);
                    one_done.send(()).expect("one done");
                    output
                },
            )
            .expect("dispatch one");
        zero_completion.recv().expect("zero completes");

        loop {
            match coordinator.poll_next().expect("offer zero") {
                OrderedRecordPoll::Pending => thread::yield_now(),
                OrderedRecordPoll::Ready(ready) => {
                    assert_eq!(ready.ordinal(), 0);
                    break;
                }
                _ => panic!("unexpected poll"),
            }
        }
        release_one.send(()).expect("release one");
        one_completion.recv().expect("one completes");
        assert_eq!(
            scope.available_permits(),
            1,
            "completed ordinal one still owns its permit before join"
        );

        match coordinator.poll_next().expect("repeat offer") {
            OrderedRecordPoll::Ready(ready) => assert_eq!(ready.ordinal(), 0),
            _ => panic!("pending offer must be repeated"),
        }
        assert_eq!(
            scope.available_permits(),
            1,
            "backpressure must pause joining later workers"
        );
        assert_eq!(
            coordinator
                .try_dispatch(scope, 1, grant_limits(1), &resources, record_output,)
                .expect("paused dispatch"),
            OrderedRecordDispatch::PausedForOutput
        );

        coordinator.acknowledge_ready(&resources).expect("acknowledge zero");
        loop {
            match coordinator.poll_next().expect("offer one") {
                OrderedRecordPoll::Pending => thread::yield_now(),
                OrderedRecordPoll::Ready(ready) => {
                    assert_eq!(ready.ordinal(), 1);
                    break;
                }
                _ => panic!("unexpected poll"),
            }
        }
        assert_eq!(scope.available_permits(), 2);
        coordinator.acknowledge_ready(&resources).expect("acknowledge one");
        coordinator.finish_dispatch();
        assert!(matches!(
            coordinator.poll_next().expect("complete"),
            OrderedRecordPoll::Complete
        ));
        assert_zero(&coordinator);
        assert!(!scope.is_cancelled());
        drop(coordinator);
        assert!(!scope.is_cancelled());
    });
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
fn cancellation_before_dispatch_and_while_worker_is_active_drains_every_grant() {
    let resources = resources();
    let baseline = resources.snapshot();

    NativeWorkerHost::new(1).scope(|scope| {
        let mut coordinator =
            OrderedRecordCoordinator::try_new(scope, window(1), descriptor_contract()).expect("coordinator");
        coordinator.cancel_and_drain();
        assert_zero(&coordinator);
        assert_eq!(scope.available_permits(), 1);
        assert_eq!(
            coordinator
                .try_dispatch(scope, 1, grant_limits(1), &resources, record_output,)
                .expect("closed"),
            OrderedRecordDispatch::Closed
        );
        drop(coordinator);
    });

    NativeWorkerHost::new(1).scope(|scope| {
        let mut coordinator =
            OrderedRecordCoordinator::try_new(scope, window(1), descriptor_contract()).expect("coordinator");
        let (started, start) = mpsc::channel();
        coordinator
            .try_dispatch(
                scope,
                8,
                grant_limits(16),
                &resources,
                move |_ordinal, _child, control| {
                    started.send(()).expect("started");
                    while control.check() == ControlOutcome::Continue {
                        thread::yield_now();
                    }
                    OrderedRecordTaskOutput::cancelled()
                },
            )
            .expect("dispatch active worker");
        start.recv().expect("active worker starts");
        coordinator.cancel_and_drain();
        assert_zero(&coordinator);
        assert_eq!(scope.available_permits(), 1);
        drop(coordinator);
    });

    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
fn a_cancelled_worker_panic_stays_observable_on_the_tripwire() {
    let resources = resources();
    let baseline = resources.snapshot();
    NativeWorkerHost::new(1).scope(|scope| {
        let mut coordinator =
            OrderedRecordCoordinator::try_new(scope, window(1), descriptor_contract()).expect("coordinator");
        let (started, start) = mpsc::channel();
        coordinator
            .try_dispatch(
                scope,
                8,
                grant_limits(16),
                &resources,
                move |_ordinal, _child, _control| {
                    started.send(()).expect("started");
                    panic!("worker panics mid-cancel");
                },
            )
            .expect("dispatch panicking worker");
        start.recv().expect("worker starts");
        // The cancel path joins the panicked worker; the join observes the panic (`Err`), which is the tripwire the
        // counter exists for.
        coordinator.cancel_and_drain();
        assert_eq!(
            coordinator.snapshot().cancelled_worker_panics(),
            1,
            "a cancelled worker's panic must stay observable"
        );
        assert_zero(&coordinator);
        assert_eq!(scope.available_permits(), 1);
        drop(coordinator);
    });
    assert_eq!(
        resources.snapshot().memory_current_bytes(),
        baseline.memory_current_bytes()
    );
}

#[test]
fn cancellation_drops_reordered_backpressured_and_terminal_buffers() {
    for stage in ["reordered", "backpressured", "terminal"] {
        let resources = resources();
        let baseline = resources.snapshot();
        NativeWorkerHost::new(2).scope(|scope| {
            let mut coordinator =
                OrderedRecordCoordinator::try_new(scope, window(2), descriptor_contract()).expect("coordinator");
            let (release_zero, wait_zero) = mpsc::channel::<()>();
            let (later_done, later_completion) = mpsc::channel();

            coordinator
                .try_dispatch(
                    scope,
                    8,
                    grant_limits(16),
                    &resources,
                    move |ordinal, budget, control| {
                        if stage != "backpressured" {
                            while control.check() == ControlOutcome::Continue && wait_zero.try_recv().is_err() {
                                thread::yield_now();
                            }
                            if control.check() != ControlOutcome::Continue {
                                return OrderedRecordTaskOutput::cancelled();
                            }
                        }
                        record_output(ordinal, budget, control)
                    },
                )
                .expect("dispatch zero");
            coordinator
                .try_dispatch(
                    scope,
                    8,
                    grant_limits(16),
                    &resources,
                    move |ordinal, budget, control| {
                        let output = if stage == "terminal" {
                            terminal_output(budget, control)
                        } else {
                            record_output(ordinal, budget, control)
                        };
                        later_done.send(()).expect("later complete");
                        output
                    },
                )
                .expect("dispatch later");
            later_completion.recv().expect("later completion");

            if stage == "backpressured" {
                loop {
                    match coordinator.poll_next().expect("offer zero") {
                        OrderedRecordPoll::Pending => thread::yield_now(),
                        OrderedRecordPoll::Ready(ready) => {
                            assert_eq!(ready.ordinal(), 0);
                            break;
                        }
                        _ => panic!("unexpected backpressure poll"),
                    }
                }
            } else {
                loop {
                    assert!(matches!(
                        coordinator.poll_next().expect("hold later"),
                        OrderedRecordPoll::Pending
                    ));
                    if coordinator.snapshot().ready_results() == 1 {
                        break;
                    }
                    thread::yield_now();
                }
            }

            coordinator.cancel_and_drain();
            drop(release_zero);
            assert_zero(&coordinator);
            assert_eq!(scope.available_permits(), 2);
            drop(coordinator);
        });
        assert_eq!(
            resources.snapshot().memory_current_bytes(),
            baseline.memory_current_bytes(),
            "stage {stage}"
        );
    }
}
