//! Feed residency, linear push, and pause-not-kill — plus the finalize law: `finish` delivers the held tail as the
//! stream's FINAL record under the profile's own tail law instead of silently dropping it.

use jqf_codec_core::DiagnosticPolicy;
use jqf_codec_json::ndjson::NdjsonProfile;
use jqf_engine::{CodecRequirementPolicy, try_compile_program};
use jqf_resource::{
    ContinueControl, RequestAccount, ResourceContext, ResourceError, ResourceLimit, ResourceLimits, WorkMeter,
    diag::codes,
};
use jqf_runtime::feed::{FeedPoll, ResidentFeed};
use jqf_runtime::records::install_record_catalog;

fn feed() -> ResidentFeed {
    ResidentFeed::new(
        NdjsonProfile::Strict,
        install_record_catalog(
            jqf_codec_json::registration().expect("json"),
            jqf_codec_json::ndjson::registration().expect("ndjson"),
            jqf_codec_json::seq::registration().expect("json-seq"),
            jqf_codec_delimited::registration().expect("csv"),
            jqf_codec_delimited::registration_tsv().expect("tsv"),
            jqf_codec_render::registration().expect("render"),
            jqf_codec_yaml::registration().expect("yaml"),
            jqf_codec_xml::registration().expect("xml"),
            jqf_codec_html::registration().expect("html"),
        ),
        |_| ("blank-record", "message"),
    )
}

/// The infallible push shape the tests read naturally: a no-op on refusal that reports the current retained length. The
/// production surface is [`ResidentFeed::try_push`] alone; this helper lives beside its only readers.
fn push(feed: &mut ResidentFeed, bytes: &[u8]) -> usize {
    match feed.try_push(bytes) {
        Ok(length) => length,
        // Refusal leaves the buffer untouched; a zero-byte push never grows, so it recovers the current length.
        Err(_) => feed.try_push(b"").expect("zero-byte push fits any cap"),
    }
}

#[test]
fn try_push_refuses_past_the_configured_cap() {
    let mut feed = feed().with_max_retained_bytes(8);
    assert_eq!(feed.try_push(b"abcd\n").expect("under cap"), 5);
    let error = feed.try_push(b"more-than-three").expect_err("over cap");
    match error {
        ResourceError::LimitExceeded {
            limit_kind,
            limit,
            current,
            requested_delta,
        } => {
            assert_eq!(limit_kind, ResourceLimit::InputBytes);
            assert_eq!(limit, 8);
            assert_eq!(current, 5);
            assert_eq!(requested_delta, 15);
        }
        other => panic!("expected a limit refusal, got {other:?}"),
    }
    assert_eq!(push(&mut feed, b"zzzzzzzz"), 5, "infallible push is a no-op on refusal");
}

#[test]
fn with_limits_honors_the_tighter_ceiling() {
    let limits = ResourceLimits::new(4, u64::MAX, 16, 0, 8);
    let mut feed = feed().with_limits(limits);
    assert!(feed.try_push(b"12345").is_err());
    assert_eq!(feed.try_push(b"12\n").expect("under the input ceiling"), 3);
}

#[test]
fn push_scans_only_the_appended_piece() {
    let mut feed = feed();
    push(&mut feed, b"{\"a\":1}\n");
    push(&mut feed, b"{\"a\":2}");
    // No LF in the second piece: the complete cut stays at the first record. A whole-buffer rposition would still find
    // the first LF; the law under test is that a piece with no LF does not move the cut.
    assert_eq!(
        push(&mut feed, b" still no lf"),
        "{\"a\":1}\n{\"a\":2} still no lf".len()
    );
    push(&mut feed, b"\n{\"a\":3}\n");
    assert_eq!(
        push(&mut feed, b""),
        "{\"a\":1}\n{\"a\":2} still no lf\n{\"a\":3}\n".len()
    );
}

const CONTROL: ContinueControl = ContinueControl;

fn poll_resources() -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(256 << 20, 256 << 20, 512 << 20, 0, 10_000))
            .expect("request account"),
        &CONTROL,
        WorkMeter::try_new_v1(64).expect("work meter"),
    )
    .expect("resources")
}

fn identity() -> jqf_engine::CompiledProgram {
    let resources = poll_resources();
    try_compile_program(
        ".",
        CodecRequirementPolicy::new(jqf_codec_core::ValidationMode::Strict, DiagnosticPolicy::ErrorsOnly),
        &resources,
    )
    .expect("program compiles")
}

/// Polls once with a generous buffer.
fn poll_once(
    feed: &mut ResidentFeed,
    compiled: &jqf_engine::CompiledProgram,
    resources: &mut ResourceContext<'_>,
) -> FeedPoll {
    let diagnostics = jqf_sdk::Diagnostics::new(DiagnosticPolicy::All).expect("diagnostics");
    let mut out = [0u8; 512];
    feed.poll(compiled, resources, &diagnostics, &mut out)
}

#[test]
fn finish_publishes_a_complete_final_record_without_its_newline() {
    let compiled = identity();
    let mut resources = poll_resources();
    let diagnostics = jqf_sdk::Diagnostics::new(DiagnosticPolicy::All).expect("diagnostics");
    let mut feed = feed();
    // The second record is COMPLETE but has no terminator — the shape the whole-input route accepts under both profiles
    // and the feed used to drop silently, because try_push only advanced the cut on a line feed.
    push(&mut feed, b"{\"a\":1}\n{\"a\":2}");
    let mut out = [0u8; 512];
    match feed.poll(&compiled, &mut resources, &diagnostics, &mut out) {
        FeedPoll::Batch(required) => assert_eq!(&out[..required], b"{\"a\":1}\n"),
        other => panic!("expected the first batch, got {other:?}"),
    }
    // Before finish the unterminated tail stays held: a healthy idle poll.
    assert!(matches!(
        poll_once(&mut feed, &compiled, &mut resources),
        FeedPoll::Empty
    ));
    feed.finish();
    match feed.poll(&compiled, &mut resources, &diagnostics, &mut out) {
        FeedPoll::Batch(required) => assert_eq!(&out[..required], b"{\"a\":2}\n"),
        other => panic!("expected the final tail record, got {other:?}"),
    }
    // Drained: finish is a CLEAN end of stream (Empty, never Failed).
    assert!(matches!(
        poll_once(&mut feed, &compiled, &mut resources),
        FeedPoll::Empty
    ));
}

#[test]
fn finish_over_a_truncated_tail_fails_under_strict() {
    let compiled = identity();
    let mut resources = poll_resources();
    let diagnostics = jqf_sdk::Diagnostics::new(DiagnosticPolicy::All).expect("diagnostics");
    let mut feed = feed();
    push(&mut feed, b"{\"a\":1}\n{\"a\":");
    let mut out = [0u8; 512];
    match feed.poll(&compiled, &mut resources, &diagnostics, &mut out) {
        FeedPoll::Batch(required) => assert_eq!(&out[..required], b"{\"a\":1}\n"),
        other => panic!("expected the first batch, got {other:?}"),
    }
    feed.finish();
    // The truncated final record is the strict profile's terminal fault, exactly the whole-input route's answer over
    // the same bytes.
    assert!(matches!(
        feed.poll(&compiled, &mut resources, &diagnostics, &mut out),
        FeedPoll::Failed
    ));
}

/// Counts the retained route and cost rows on one diagnostics stream.
fn route_and_cost_count(diagnostics: &jqf_sdk::Diagnostics) -> (usize, usize) {
    let records = diagnostics.records();
    let routes = records
        .iter()
        .filter(|record| record.code == codes::ROUTE_SELECTED)
        .count();
    let costs = records
        .iter()
        .filter(|record| record.code == codes::COST_SNAPSHOT)
        .count();
    (routes, costs)
}

/// The idle poll's route/cost pair emits ONCE per drive state, not once per poll: steady-state polling over an
/// unchanged feed formats no diagnostic records at all, while a real transition (a batch driven and published) emits a
/// fresh pair shaped exactly like the first.
#[test]
fn idle_polling_emits_route_and_cost_once_per_state_change() {
    let compiled = identity();
    let mut resources = poll_resources();
    let diagnostics = jqf_sdk::Diagnostics::new(DiagnosticPolicy::All).expect("diagnostics");
    let mut feed = feed();
    let mut out = [0u8; 512];

    // The FIRST idle poll emits the pair.
    assert!(matches!(
        feed.poll(&compiled, &mut resources, &diagnostics, &mut out),
        FeedPoll::Empty
    ));
    assert_eq!(route_and_cost_count(&diagnostics), (1, 1));
    let records = diagnostics.records();
    assert_eq!(
        records
            .iter()
            .find(|record| record.code == codes::ROUTE_SELECTED)
            .and_then(|record| record.operand()),
        Some("record"),
        "the route row keeps its operand"
    );

    // Nine identical idle polls over the unchanged feed emit NOTHING.
    for _ in 0..9 {
        assert!(matches!(
            feed.poll(&compiled, &mut resources, &diagnostics, &mut out),
            FeedPoll::Empty
        ));
    }
    assert_eq!(route_and_cost_count(&diagnostics), (1, 1));

    // A real transition — one complete record driven and published — emits a fresh pair.
    push(&mut feed, b"{\"a\":1}\n");
    match feed.poll(&compiled, &mut resources, &diagnostics, &mut out) {
        FeedPoll::Batch(required) => assert_eq!(&out[..required], b"{\"a\":1}\n"),
        other => panic!("expected the record's batch, got {other:?}"),
    }
    assert_eq!(route_and_cost_count(&diagnostics), (2, 2));

    // Settled again: one settling poll after the delivery may still see a moved state, but the identical idle polls
    // that follow emit nothing.
    assert!(matches!(
        feed.poll(&compiled, &mut resources, &diagnostics, &mut out),
        FeedPoll::Empty
    ));
    let settled = route_and_cost_count(&diagnostics);
    for _ in 0..9 {
        assert!(matches!(
            feed.poll(&compiled, &mut resources, &diagnostics, &mut out),
            FeedPoll::Empty
        ));
    }
    assert_eq!(route_and_cost_count(&diagnostics), settled);
}
