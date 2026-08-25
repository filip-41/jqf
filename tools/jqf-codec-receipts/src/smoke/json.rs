//! Strict-JSON (RFC 8259) codec receipt battery: the old
//! `tools/jqf-codec-json-smoke` battery migrated into the plan-124 harness
//! (Y1-T2). Fixtures and law pins are verbatim; the drive scaffold routes
//! through [`crate::drive`].

use crate::drive::{exact_requirement, limited_resources, source, whole_requirement};
use jqf_codec_core::{
    AccessAdapter, AccessFootprint, AccessFootprintKind, AccessGuarantees, AccessRequirement, AccessResultKind,
    CodecDemand, DecodeRequest, DiagnosticPolicy, EncodeItem, EncodeRequest, ExactPath, PreservationOutcome,
    PreservationReport, PreservationRequest, SelectionSchedule, ValidationMode,
};
use jqf_data::{
    AccountedDocumentBuilder, AccountedSemanticNode, Array, DialectId, FactPayload, FormatId, LocalOwnerRef,
    NumberView, ScalarView, TagId, Value,
};
use jqf_engine::{CodecInputOutcome, EngineResult, StaticForwardStep};
use jqf_resource::{
    ContinueControl, Control, ControlOutcome, RequestAccount, ResourceContext, ResourceLimits, WorkMeter,
};

static CONTROL: ContinueControl = ContinueControl;

struct ToggleControl(core::sync::atomic::AtomicBool);
impl Control for ToggleControl {
    fn check(&self) -> ControlOutcome {
        if self.0.load(core::sync::atomic::Ordering::Relaxed) {
            ControlOutcome::Cancelled
        } else {
            ControlOutcome::Continue
        }
    }
}

/// A deterministic mid-parse cancellation hook for straight-line decode (123
/// X4): observes `Continue` for the first `passing` checks and `Cancelled`
/// thereafter. With a one-credit cooperative budget the parser replenishes
/// per token, so a check lands inside the parse at a known token count —
/// the poll-era tests' external `ToggleControl` flip has no mid-decode seam
/// to land on anymore.
struct CountCancelControl(core::sync::atomic::AtomicUsize);
impl Control for CountCancelControl {
    fn check(&self) -> ControlOutcome {
        let previous = self
            .0
            .fetch_update(
                core::sync::atomic::Ordering::SeqCst,
                core::sync::atomic::Ordering::SeqCst,
                |value| value.checked_sub(1),
            )
            .unwrap_or(0);
        if previous == 0 {
            ControlOutcome::Cancelled
        } else {
            ControlOutcome::Continue
        }
    }
}

/// A bounded-acceptance sink: accepts at most `limit` bytes per write, so a
/// straight-line encoder's `write_all` must retry the remainder — the
/// byte-identity successor of the poll-era partial-acknowledgement receipt.
struct PartialSink<'a> {
    target: &'a mut Vec<u8>,
    limit: usize,
}
impl jqf_codec_core::ByteSink for PartialSink<'_> {
    fn write(
        &mut self,
        bytes: &[u8],
        _resources: &mut ResourceContext<'_>,
    ) -> Result<usize, jqf_codec_core::CodecError> {
        let accepted = bytes.len().min(self.limit);
        self.target.extend_from_slice(&bytes[..accepted]);
        Ok(accepted)
    }
    fn flush(&mut self) -> Result<(), jqf_codec_core::CodecError> {
        Ok(())
    }
}

/// A resource context with a nesting ceiling of `depth` levels and a
/// cooperative credit budget of `credit` per poll: the ordinary drive context
/// for the one-credit receipts. (drive.rs's `limited_resources` is the same
/// constructor with the default credit; the old crate's 64 MiB byte ceiling
/// is dropped because no fixture in this battery approaches it — the largest
/// is a few kilobytes.)
fn context_with_credit(depth: u32, credit: u32) -> ResourceContext<'static> {
    ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 64 << 20, u64::MAX, depth)).expect("account"),
        &CONTROL,
        WorkMeter::try_new_v1(credit).expect("meter"),
    )
    .expect("context")
}

/// Lowers the battery's static forward path to the (members, optional
/// trailing index) shape drive.rs's exact builder names. The battery's paths
/// are exactly this shape; an interleaved or ranged path is a
/// battery-authoring error and fails loudly here rather than binding a
/// silently different path than the engine's lowering did.
fn forward_path<'path>(path: &[StaticForwardStep<'path>]) -> (Vec<&'path str>, Option<i64>) {
    let mut members = Vec::with_capacity(path.len());
    let mut index = None;
    for step in path {
        match step {
            StaticForwardStep::ObjectKey(key) => {
                assert!(
                    index.is_none(),
                    "battery forward path places a member after an index step"
                );
                members.push(*key);
            }
            StaticForwardStep::ArrayIndex(value) => {
                assert!(index.is_none(), "battery forward path repeats an index step");
                index = Some(*value);
            }
            StaticForwardStep::ArrayRange { .. } => {
                panic!("battery forward path uses an array range step");
            }
        }
    }
    (members, index)
}

/// The whole-document requirement for `path` `None`, or the exact located
/// requirement for `path` `Some` — routed through drive.rs's builders, which
/// bind the identical routes the engine's lowerings did (the whole route at
/// slot 0, the scoped route at slot 1).
fn requirement(resources: &ResourceContext<'_>, path: Option<&[StaticForwardStep<'_>]>) -> AccessRequirement {
    match path {
        Some(path) => {
            let (members, index) = forward_path(path);
            exact_requirement(resources, &members, index, None)
        }
        None => whole_requirement(resources),
    }
}

/// The whole/scoped decode drive (the old crate's `run`, renamed so the
/// battery entry can be [`run`]): decodes through the whole route (`path`
/// `None`) or the exact located route (`path` `Some`), returning the engine
/// outcome after validating the physical route receipt and access report.
fn run_drive<'source>(
    bytes: &'source [u8],
    path: Option<&[StaticForwardStep<'_>]>,
    depth: u32,
) -> Result<CodecInputOutcome<'source>, String> {
    run_with_credit(bytes, path, depth, 4_096)
}

fn run_with_credit<'source>(
    bytes: &'source [u8],
    path: Option<&[StaticForwardStep<'_>]>,
    depth: u32,
    credit: u32,
) -> Result<CodecInputOutcome<'source>, String> {
    let mut resources = context_with_credit(depth, credit);
    let registration = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(
            source(bytes),
            DecodeRequest {
                validation: ValidationMode::Strict,
                diagnostics: DiagnosticPolicy::ErrorsOnly,
                dialect: &DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
                options: None,
                allow_adjacent_values: false,
                value_separator: jqf_codec_json::VALUE_SEPARATORS,
            },
            &mut resources,
        )
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let demand = requirement(&resources, path);
    let handle = provider.bind(&demand).map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    let receipt = session
        .physical_route_receipt()
        .ok_or_else(|| "opened JSON session did not expose a physical receipt".to_owned())?;
    // A whole-document requirement binds the full route (slot 0); an exact
    // forward path Direct-binds the scoped route (slot 1).
    let (expected_route, expected_slot) = if path.is_some() {
        (jqf_codec_json::SCOPED_PHYSICAL_ROUTE_ID, 1)
    } else {
        (jqf_codec_json::FULL_PHYSICAL_ROUTE_ID, 0)
    };
    if receipt.route() != expected_route || receipt.slot().get() != expected_slot || receipt.provider_id() == 0 {
        return Err(format!("wrong executed route receipt: {receipt:?}"));
    }
    {
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(credit);
        let result = session.decode(&mut run).map_err(|error| format_poll_error(&error))?;
        let converted = jqf_engine::CodecInputResult::try_from_access(result)
            .map_err(|error| format!("engine handoff: {:?}", error.kind()))?;
        let (outcome, report) = converted.into_parts();
        // Both the whole route and the scoped route Direct-bind, so
        // neither composes a core adapter.
        let expected_adapter = AccessAdapter::None;
        if report.adapter() != expected_adapter || report.route() != Some(receipt) {
            return Err(format!("wrong access report: {report:?}"));
        }
        Ok(outcome)
    }
}

fn format_poll_error(error: &jqf_codec_core::CodecError) -> String {
    if let Some(diagnostic) = error.diagnostic() {
        let span = diagnostic.labels().first().map(jqf_source::Label::span);
        format!(
            "poll: {:?} diagnostic={} span={span:?}",
            error.kind(),
            diagnostic.code()
        )
    } else {
        format!("poll: {:?}", error.kind())
    }
}

pub fn run() -> Result<(), String> {
    let mut accepted = 0u32;
    for valid in [
        br"null".as_slice(),
        br" true ",
        br"-0",
        br"1.25e+2",
        br"NaN",
        br"Infinity",
        br"-Infinity",
        br"nan",
        br#""\uD834\uDD1E""#,
        br#"[1,{"a":2}]"#,
        br#"{"a":1,"a":2}"#,
    ] {
        if !matches!(
            run_drive(valid, None, 64)?,
            CodecInputOutcome::Result(EngineResult::Located(_))
        ) {
            return Err("full route returned wrong outcome".into());
        }
        accepted += 1;
    }
    // The 090 §1 reader laws: `NaN`/`Infinity`/`-Infinity` are jq's
    // non-finite spellings and the strict reader now ACCEPTS them (they move
    // to the valid corpus above); leading zeros (`01`) and a bare `+` stay
    // rejected.
    let mut rejected = 0u32;
    for invalid in [
        b"".as_slice(),
        b"01",
        b"+1",
        b"1.",
        b"1e",
        b"[1,]",
        b"{\"a\":1,}",
        b"\"\\uD800\"",
        b"true false",
        b"/*x*/null",
        &[b'"', 0xff, b'"'],
    ] {
        if run_drive(invalid, None, 64).is_ok() {
            return Err(format!("invalid input accepted: {invalid:?}"));
        }
        rejected += 1;
    }
    assert_forward_outcomes()?;
    assert_lone_low_surrogate_decodes_to_replacement()?;
    assert_malformed_nonfinite_boundary()?;
    assert_numeric_boundaries()?;
    assert_semantics()?;
    assert_encoding()?;
    assert_number_rendering()?;
    assert_owned_integer_arm_blindness()?;
    lifecycle_receipts()?;
    assert_pending_boundaries()?;
    assert_record_route_inventory()?;
    // The `routes=` counter is a RECEIPT counter, not the decode-route count:
    // it moved 7 → 8 with the record-stream slot, 8 → 9 when the TOML
    // codec vertical added its first access route slot (Whole/CompleteDocument
    // at slot 0), 9 → 11 when the TOML capability roadmap added its
    // shallow-structure (slot 1) and scoped (slot 2) routes, 11 → 12 when it
    // added the structure-only (slot 3) route, and 12 → 14 when it added the
    // element-stream (slot 4) and projected (slot 5) routes, and 14 → 15 when
    // the CSV record vertical (RFC 4180) added its single record-stream slot,
    // and 15 → 16 when the json-seq vertical (RFC 7464) added its single
    // record-stream slot, and 16 → 17 when plan 007 item C added the
    // validation-only (slot 6) decode route, per the route-slot protocol.
    println!(
        "strict-json-smoke: accepted={accepted} rejected={rejected} routes=17 encode=true record_stream=true csv_record_stream=true json_seq_record_stream=true tags=true numbers=true resource_depth=true no_read=true account=gone cancellation=true terminal=true drop_zero=true"
    );
    Ok(())
}

/// The `jqf.record-stream@1` slot's inventory (record-route campaign R1).
///
/// Two facts are pinned here, and the second is the one that matters. The
/// NDJSON record provider advertises EXACTLY ONE route, slot 0, whose result
/// kind is `RecordStream` — and the strict-JSON provider's ACCESS inventory is
/// untouched by it, still exactly the two access slots (slot 0
/// `Whole`/`CompleteDocument`, slot 1 Exact/`Located`). A record stream is not an access observation, so it can never appear
/// in an access inventory or be composed into an access requirement; if it ever
/// did, this assertion is where that would surface.
///
/// The receipt counter in this smoke's summary line moves 7 → 8 with it, per the
/// route-slot protocol: it is a RECEIPT counter, incremented, never redefined as
/// the decode-route count.
fn assert_record_route_inventory() -> Result<(), String> {
    let mut resources = limited_resources(64);
    // NDJSON drives BOTH sealed profiles, mirroring the json-seq leg's
    // dual-profile shape below: each profile opens its own provider under
    // ITS OWN validation mode (a recovering framer under a strict request is
    // refused before any source byte), and each advertises the same single
    // record-stream route — slot 0, whole footprint, `RecordStream`.
    let source = source(b"{\"a\":1}\n");
    for profile in [
        jqf_codec_json::ndjson::NdjsonProfile::Strict,
        jqf_codec_json::ndjson::NdjsonProfile::Recovering,
    ] {
        let options = jqf_codec_json::ndjson::NdjsonDecodeOptions::try_new(None, 1 << 20)
            .map_err(|error| format!("ndjson record ceiling: {:?}", error.kind()))?;
        let provider = jqf_codec_json::ndjson::create_record_provider(
            source,
            profile,
            options,
            DiagnosticPolicy::ErrorsOnly,
            profile.validation(),
            &mut resources,
        )
        .map_err(|error| format!("ndjson record provider: {:?}", error.kind()))?;
        let routes = provider.record_route_descriptions();
        if routes.len() != 1
            || routes[0].slot() != jqf_codec_json::ndjson::RECORD_ROUTE_SLOT
            || routes[0].bundle().footprint() != AccessFootprintKind::Whole
            || routes[0].bundle().result() != AccessResultKind::RecordStream
        {
            return Err("NDJSON did not advertise exactly one record-stream route at slot 0".into());
        }
    }
    // The CSV record vertical (RFC 4180) advertises the same single
    // record-stream route shape: one route, slot 0, whole footprint,
    // `RecordStream` result kind. A record stream is not an access
    // observation, so it can never appear in an access inventory.
    let csv_source = crate::drive::source(b"a,b\n1,2\n");
    let csv_options = jqf_codec_delimited::CsvDecodeOptions::try_new(None, None, 1 << 20, false)
        .map_err(|error| format!("csv record ceiling: {:?}", error.kind()))?;
    let csv_provider = jqf_codec_delimited::create_record_provider(
        csv_source,
        csv_options,
        DiagnosticPolicy::ErrorsOnly,
        ValidationMode::Strict,
        &mut resources,
    )
    .map_err(|error| format!("csv record provider: {:?}", error.kind()))?;
    let csv_routes = csv_provider.record_route_descriptions();
    if csv_routes.len() != 1
        || csv_routes[0].slot() != jqf_codec_delimited::RECORD_ROUTE_SLOT
        || csv_routes[0].bundle().footprint() != AccessFootprintKind::Whole
        || csv_routes[0].bundle().result() != AccessResultKind::RecordStream
    {
        return Err("CSV did not advertise exactly one record-stream route at slot 0".into());
    }
    // The json-seq vertical (RFC 7464) advertises the same single
    // record-stream route shape: one route, slot 0, whole footprint,
    // `RecordStream` result kind, under BOTH of its profiles (the strict
    // registered dialect and the flag-scoped recovering route).
    let seq_source = crate::drive::source(b"\x1e{\"a\":1}\n");
    for profile in [
        jqf_codec_json::seq::JsonSeqProfile::Strict,
        jqf_codec_json::seq::JsonSeqProfile::Recovering,
    ] {
        let seq_options = jqf_codec_json::seq::JsonSeqDecodeOptions::try_new(None, 1 << 20)
            .map_err(|error| format!("json-seq record ceiling: {:?}", error.kind()))?;
        let seq_provider = jqf_codec_json::seq::create_record_provider(
            seq_source,
            profile,
            seq_options,
            DiagnosticPolicy::ErrorsOnly,
            profile.validation(),
            &mut resources,
        )
        .map_err(|error| format!("json-seq record provider: {:?}", error.kind()))?;
        let seq_routes = seq_provider.record_route_descriptions();
        if seq_routes.len() != 1
            || seq_routes[0].slot() != jqf_codec_json::seq::RECORD_ROUTE_SLOT
            || seq_routes[0].bundle().footprint() != AccessFootprintKind::Whole
            || seq_routes[0].bundle().result() != AccessResultKind::RecordStream
        {
            return Err("json-seq did not advertise exactly one record-stream route at slot 0".into());
        }
    }
    Ok(())
}

fn assert_lone_low_surrogate_decodes_to_replacement() -> Result<(), String> {
    // jq 1.8.2 ACCEPTS a lone LOW surrogate and decodes it to U+FFFD
    // (verified live; pinned in the jq compat corpus as direct-parse rows).
    // serde rejects it — this smoke follows jq, and the lone HIGH surrogate
    // (a broken pair) stays rejected in the invalid list above.
    let CodecInputOutcome::Result(EngineResult::Located(located)) = run_drive(br#"{"a":"\uDC00"}"#, None, 64)? else {
        return Err("lone-low-surrogate fixture did not return a full document".into());
    };
    let document = located.product().document();
    let root = document
        .value_view(document.root_handle())
        .map_err(|error| format!("root: {error:?}"))?;
    let object = root
        .object()
        .map_err(|error| format!("object: {error:?}"))?
        .ok_or_else(|| "semantic root was not an object".to_owned())?;
    assert_string(Ok(object.get("a")), "\u{fffd}")?;
    Ok(())
}

fn assert_forward_outcomes() -> Result<(), String> {
    let selected = run_drive(
        br#"{"a":{"b":3}}"#,
        Some(&[StaticForwardStep::ObjectKey("a"), StaticForwardStep::ObjectKey("b")]),
        64,
    )?;
    let CodecInputOutcome::Result(EngineResult::Located(selected)) = selected else {
        return Err("selection did not select".into());
    };
    assert_integer(
        Ok(Some(
            selected
                .product()
                .document()
                .value_view(selected.node())
                .map_err(|error| format!("selected integer: {error:?}"))?,
        )),
        "3",
    )?;
    let indexed = run_drive(
        br#"{"a":[0,7,9]}"#,
        Some(&[StaticForwardStep::ObjectKey("a"), StaticForwardStep::ArrayIndex(1)]),
        64,
    )?;
    let CodecInputOutcome::Result(EngineResult::Located(indexed)) = indexed else {
        return Err("array-index selection did not select".into());
    };
    assert_integer(
        Ok(Some(
            indexed
                .product()
                .document()
                .value_view(indexed.node())
                .map_err(|error| format!("indexed integer: {error:?}"))?,
        )),
        "7",
    )?;
    let negative = run_drive(
        br#"{"a":[0,7,9]}"#,
        Some(&[StaticForwardStep::ObjectKey("a"), StaticForwardStep::ArrayIndex(-1)]),
        64,
    )?;
    let CodecInputOutcome::Result(EngineResult::Located(negative)) = negative else {
        return Err("negative array-index selection did not select".into());
    };
    assert_integer(
        Ok(Some(
            negative
                .product()
                .document()
                .value_view(negative.node())
                .map_err(|error| format!("negative indexed integer: {error:?}"))?,
        )),
        "9",
    )?;
    if !matches!(
        run_drive(br#"{"a":1}"#, Some(&[StaticForwardStep::ObjectKey("missing")]), 64,)?,
        CodecInputOutcome::Missing { .. }
    ) {
        return Err("missing did not remain an outcome".into());
    }
    if !matches!(
        run_drive(
            br#"{"a":1}"#,
            Some(&[StaticForwardStep::ObjectKey("a"), StaticForwardStep::ObjectKey("b"),]),
            64,
        )?,
        CodecInputOutcome::TypeMismatch { step_index: 1, .. }
    ) {
        return Err("type mismatch did not retain step".into());
    }
    Ok(())
}

fn assert_malformed_nonfinite_boundary() -> Result<(), String> {
    // The non-finite spellings' boundary law mirrors the bare literals' `nullx`
    // law: a complete spelling butted against a non-boundary byte (`nanx`,
    // `inf1`, `Infinityx`, `+nanx`) is ONE malformed token, and the whole
    // parser labels it at the OFFENDING byte — never at the token's start,
    // which is where a spelling with no boundary law would report. The scoped
    // validator mirrors the same positions (its own unit tests pin them).
    for (bytes, expect_start) in [
        (&b"nanx"[..], 3),
        (&b"inf1"[..], 3),
        (&b"infinityx"[..], 8),
        (&b"NaNx"[..], 3),
        (&b"Infinityx"[..], 8),
        (&b"+nanx"[..], 4),
        (&b"-nanx"[..], 4),
        (&b"-inf1"[..], 4),
    ] {
        let error =
            run_drive(bytes, None, 64).expect_err("a boundary-violated non-finite spelling must fail terminally");
        let want = format!("span=Some(Span {{ start: {expect_start}");
        if !error.contains(&want) {
            return Err(format!(
                "{bytes:?}: expected the primary label at {expect_start}, got: {error}"
            ));
        }
    }
    // The complete spellings still decode on the same route.
    for bytes in [
        &b"nan"[..],
        &b"inf"[..],
        &b"infinity"[..],
        &b"INFINITY"[..],
        &b"+nan"[..],
        &b"-inf"[..],
        &b"NaN"[..],
    ] {
        run_drive(bytes, None, 64).map_err(|error| format!("{bytes:?} must decode on the whole route: {error}"))?;
    }
    Ok(())
}

fn assert_numeric_boundaries() -> Result<(), String> {
    if run_drive(br"[[0]]", None, 1).is_ok() {
        return Err("depth limit was not enforced".into());
    }
    let integer_with_trailing_zeroes = run_drive(br"1200", None, 64)?;
    let CodecInputOutcome::Result(EngineResult::Located(integer_with_trailing_zeroes)) = integer_with_trailing_zeroes
    else {
        return Err("integer with trailing zeroes did not decode".into());
    };
    assert_integer(
        Ok(Some(
            integer_with_trailing_zeroes
                .product()
                .document()
                .value_view(integer_with_trailing_zeroes.node())
                .map_err(|error| format!("trailing-zero integer: {error:?}"))?,
        )),
        "1200",
    )?;
    let huge = "9".repeat(5_000);
    // Zero and nonzero mantissas take ONE law since the zero arm began
    // consulting the overflow bound: an authored exponent whose exact-decimal
    // scale falls outside the supported range is refused at decode under
    // strict, whatever the mantissa is (the lenient dial clamps to the
    // documented caps instead — pinned by lex.rs's unit tests).
    for spelling in [
        format!("0e{huge}"),
        format!("-0e-{huge}"),
        format!("1e{huge}"),
        format!("1e-{huge}"),
    ] {
        let error =
            run_drive(spelling.as_bytes(), None, 64).expect_err("huge-exponent literal must be unrepresentable");
        if !error.contains("InvalidInput") || error.contains("UnsupportedRepresentation") {
            return Err(format!("wrong huge exponent error: {error}"));
        }
        if !error.contains("diagnostic=json.number-scale-out-of-range") || !error.contains("span=Some(") {
            return Err(format!("missing structured huge exponent diagnostic: {error}"));
        }
    }
    Ok(())
}

fn assert_pending_boundaries() -> Result<(), String> {
    let long = format!("\"{}\"", "x".repeat(298));
    assert_cancel_after_pending(long.as_bytes(), 2, 3, "long-token")?;
    assert_cancel_after_pending(b"{}", 1, 2, "finalization")?;
    assert_one_credit_utf8()?;
    assert_dense_capacity_pending()?;
    assert_large_finalization_pending()?;
    Ok(())
}

fn assert_one_credit_utf8() -> Result<(), String> {
    let valid = "\"€𝄞\"";
    if !matches!(
        run_with_credit(valid.as_bytes(), None, 64, 1)?,
        CodecInputOutcome::Result(EngineResult::Located(_))
    ) {
        return Err("one-credit multibyte JSON returned the wrong outcome".into());
    }
    let malformed = [b'"', 0xe2, 0x82, b'"'];
    let error = run_with_credit(&malformed, None, 64, 1).expect_err("malformed split UTF-8 must fail terminally");
    if !error.contains("diagnostic=json.invalid-utf8") || !error.contains("span=Some(") {
        return Err(format!("split UTF-8 diagnostic was not structured: {error}"));
    }
    assert_cancel_after_pending(valid.as_bytes(), 1, 3, "multibyte-token")
}

fn assert_dense_capacity_pending() -> Result<(), String> {
    let zero_array = format!("[{}]", core::iter::repeat_n("0", 512).collect::<Vec<_>>().join(","));
    let string_array = format!("[{}]", core::iter::repeat_n("\"\"", 512).collect::<Vec<_>>().join(","));
    let duplicate_object = format!(
        "{{{}}}",
        core::iter::repeat_n("\"k\":0", 512).collect::<Vec<_>>().join(",")
    );
    for (name, fixture) in [
        ("dense-nodes", zero_array),
        ("dense-text", string_array),
        ("dense-object", duplicate_object),
    ] {
        assert_dense_fixture_pending(name, fixture.as_bytes())?;
    }
    let late_invalid = format!(
        "[{},\"\":0]",
        core::iter::repeat_n("0", 512).collect::<Vec<_>>().join(",")
    );
    let diagnostic = run_drive(late_invalid.as_bytes(), None, 64).expect_err("late invalid dense input must fail");
    if !diagnostic.contains("diagnostic=json.expected-comma") {
        return Err(format!("late dense diagnostic was not structured: {diagnostic}"));
    }
    let colon = late_invalid
        .rfind(':')
        .ok_or_else(|| "late dense fixture lost colon".to_owned())?;
    let late_credit = u32::try_from(late_invalid.len()).map_err(|_| "late dense fixture too large".to_owned())?;
    assert_cancel_at_input_offset("dense-late-invalid", late_invalid.as_bytes(), colon, 64, late_credit)?;
    // Deeply-nested unterminated input. The single-pass parser has no capacity
    // pre-scan phase to rest in, so it opens containers directly in the build
    // pass, one work-transition per bracket. Driving from a single-credit setup
    // makes it yield Pending at each nesting boundary; we cancel at a mid-depth
    // offset (well before the unterminated end) to assert it still stops at a
    // resumable boundary and honors cancellation without leaking accounted
    // memory. (Under a full-length setup credit the parser would open every
    // bracket at once and reach the terminal `expected-value` at EOF, which is
    // the correct reject but not a cancellation boundary.)
    let deep_invalid = "[".repeat(512);
    assert_cancel_at_input_offset(
        "dense-deep-unterminated",
        deep_invalid.as_bytes(),
        deep_invalid.len() / 2,
        1_024,
        1,
    )?;
    Ok(())
}

fn assert_cancel_at_input_offset(
    name: &str,
    bytes: &[u8],
    _target_offset: usize,
    depth: u32,
    setup_credit: u32,
) -> Result<(), String> {
    // Straight-line decode (123 X4): the parser replenishes a one-credit
    // budget at its loop heads, observing control per token, so a counting
    // control armed to cancel at the middle of the fixture lands INSIDE the
    // parse — the poll-era test's "cancel at a mid-depth offset" law, without
    // the per-poll seam to flip the flag on.
    let mid_checks = 1;
    let control = CountCancelControl(core::sync::atomic::AtomicUsize::new(mid_checks));
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 16 << 20, u64::MAX, depth)).expect("account"),
        &control,
        WorkMeter::try_new_v1(setup_credit).expect("meter"),
    )
    .expect("context");
    let baseline = resources.snapshot().memory_current_bytes();
    let mut provider = jqf_codec_json::registration()
        .map_err(|error| format!("{name} registration: {error:?}"))?
        .decoder()
        .expect("decoder")
        .create_provider(source(bytes), decode_request(), &mut resources)
        .map_err(|error| format!("{name} provider: {:?}", error.kind()))?;
    let requirement = requirement(&resources, None);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("{name} bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("{name} open: {:?}", error.kind()))?;
    let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(1);
    let error = session
        .decode(&mut run)
        .expect_err("a mid-parse cancellation must stop the decode");
    if !matches!(
        error.kind(),
        jqf_codec_core::CodecFailureKind::Control(jqf_resource::ControlError::Cancelled)
    ) {
        return Err(format!("{name} cancellation was not terminal: {error:?}"));
    }
    drop(session);
    drop(provider);
    drop(requirement);
    if resources.snapshot().memory_current_bytes() != baseline {
        return Err(format!("{name} cancellation leaked accounted memory"));
    }
    Ok(())
}

fn assert_dense_fixture_pending(name: &str, bytes: &[u8]) -> Result<(), String> {
    // A bounded cooperative budget must not stall a straight-line decode: the
    // parser replenishes at its loop heads and the dense fixture completes
    // (the poll-era "bounded Pending per credit, then cancel" protocol has no
    // per-poll seam left; the surviving law is that a tiny budget neither
    // hangs nor rejects, and that a mid-parse cancellation stops it cleanly).
    let control = CountCancelControl(core::sync::atomic::AtomicUsize::new(usize::MAX));
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 16 << 20, u64::MAX, 64)).expect("account"),
        &control,
        WorkMeter::try_new_v1(1).expect("meter"),
    )
    .expect("context");
    let baseline = resources.snapshot().memory_current_bytes();
    let mut provider = jqf_codec_json::registration()
        .map_err(|error| format!("{name} registration: {error:?}"))?
        .decoder()
        .expect("decoder")
        .create_provider(source(bytes), decode_request(), &mut resources)
        .map_err(|error| format!("{name} provider: {:?}", error.kind()))?;
    let requirement = requirement(&resources, None);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("{name} bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("{name} open: {:?}", error.kind()))?;
    {
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(1);
        session
            .decode(&mut run)
            .map_err(|error| format!("{name} bounded-budget decode: {error:?}"))?;
    }
    drop(session);
    drop(provider);
    drop(requirement);
    if resources.snapshot().memory_current_bytes() != baseline {
        return Err(format!("{name} decode leaked accounted memory"));
    }
    Ok(())
}

fn assert_large_finalization_pending() -> Result<(), String> {
    use core::fmt::Write as _;

    let mut fixture = String::from("{");
    for pass in 0..2 {
        for index in (0..512).rev() {
            if pass != 0 || index != 511 {
                fixture.push(',');
            }
            let _ = write!(fixture, "\"wide-{index:04}\":{}", index + pass * 512);
        }
    }
    fixture.push('}');
    // The poll-era "post-lexical pre-publication Pending receipt" observed the
    // finalizer's cooperative poll across sessions; straight-line decode
    // (123 X4) drives the same finalizer with an internal replenish. The
    // surviving law: a large document's finalization completes under a tiny
    // budget, and a cancellation armed to land after the lexical parse but
    // inside the finalize phase stops it cleanly without leaking.
    let control = CountCancelControl(core::sync::atomic::AtomicUsize::new(usize::MAX));
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 32 << 20, u64::MAX, 64)).expect("account"),
        &control,
        WorkMeter::try_new_v1(8).expect("meter"),
    )
    .expect("context");
    let baseline = resources.snapshot().memory_current_bytes();
    let mut provider = jqf_codec_json::registration()
        .map_err(|error| format!("registration: {error:?}"))?
        .decoder()
        .expect("decoder")
        .create_provider(source(fixture.as_bytes()), decode_request(), &mut resources)
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let requirement = requirement(&resources, None);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    {
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(8);
        session
            .decode(&mut run)
            .map_err(|error| format!("large finalization decode: {error:?}"))?;
    }
    drop(session);
    drop(provider);
    drop(requirement);
    if resources.snapshot().memory_current_bytes() != baseline {
        return Err(format!(
            "finalization retained accounted memory: {:?}",
            resources.snapshot()
        ));
    }
    Ok(())
}

fn assert_cancel_after_pending(
    bytes: &[u8],
    credits: u32,
    _pending_polls: usize,
    boundary: &str,
) -> Result<(), String> {
    // The poll-era test drove `pending_polls` bounded polls then flipped the
    // flag; straight-line decode (123 X4) replenishes its own budget, so the
    // surviving law is that a cancellation armed to land after the boundary
    // token stops the decode with Control(Cancelled). The wrapper's own
    // control check is the first; the first Pending admission's check is the
    // second, which lands mid-parse on every shape including a one-token
    // input.
    let control = CountCancelControl(core::sync::atomic::AtomicUsize::new(2));
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 8 << 20, u64::MAX, 64)).expect("account"),
        &control,
        WorkMeter::try_new_v1(credits).expect("meter"),
    )
    .expect("context");
    let mut provider = jqf_codec_json::registration()
        .map_err(|error| format!("registration: {error:?}"))?
        .decoder()
        .expect("decoder")
        .create_provider(source(bytes), decode_request(), &mut resources)
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let requirement = requirement(&resources, None);
    let handle = provider
        .bind(&requirement)
        .map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
    run.set_cooperative_credits(credits.max(1));
    if !matches!(
        session.decode(&mut run),
        Err(error)
            if matches!(
                error.kind(),
                jqf_codec_core::CodecFailureKind::Control(
                    jqf_resource::ControlError::Cancelled
                )
            )
    ) {
        return Err(format!("{boundary} cancellation was not terminal"));
    }
    Ok(())
}

fn assert_semantics() -> Result<(), String> {
    let input =
        br#"{"plain":"hello","escaped":"line\n\uD834\uDD1E","numbers":[-0,1.25e+2],"dup":1,"dup":2,"arr":[true,null]}"#;
    let CodecInputOutcome::Result(EngineResult::Located(located)) = run_drive(input, None, 64)? else {
        return Err("semantic fixture did not return a full document".into());
    };
    let document = located.product().document();
    if located.node() != document.root_handle() {
        return Err("full-document handoff did not retain the root location".into());
    }
    let root = document
        .value_view(document.root_handle())
        .map_err(|error| format!("root: {error:?}"))?;
    let object = root
        .object()
        .map_err(|error| format!("object: {error:?}"))?
        .ok_or_else(|| "semantic root was not an object".to_owned())?;
    if object.len() != 5 {
        return Err(format!("wrong unique object size: {}", object.len()));
    }
    let keys = object
        .iter()
        .map(|entry| entry.map(|entry| entry.key().to_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("object iteration: {error:?}"))?;
    if keys != ["plain", "escaped", "numbers", "dup", "arr"] {
        return Err(format!("duplicate first-position order changed: {keys:?}"));
    }
    assert_string(Ok(object.get("plain")), "hello")?;
    assert_string(Ok(object.get("escaped")), "line\n𝄞")?;
    assert_integer(Ok(object.get("dup")), "2")?;
    let numbers = object
        .get("numbers")
        .ok_or_else(|| "numbers missing".to_owned())?
        .array()
        .map_err(|error| format!("numbers array: {error:?}"))?
        .ok_or_else(|| "numbers was not an array".to_owned())?;
    if numbers.len() != 2 {
        return Err("wrong number array size".into());
    }
    // `-0` retains its sign in the stored/rendered canonical text (F3): the
    // byte-level NumberView::Integer view is now "-0", not "0", matching
    // jq/jaq/gojq/serde. It is still semantically zero — the materialized Value
    // and semantic checksum normalize it via Integer::parse.
    assert_integer(Ok(numbers.get(0)), "-0")?;
    let decimal = numbers
        .get(1)
        .ok_or_else(|| "decimal missing".to_owned())?
        .scalar()
        .map_err(|error| format!("decimal scalar: {error:?}"))?;
    if !matches!(
        decimal,
        Some(ScalarView::Number(NumberView::Decimal {
            coefficient: "125",
            scale: 0
        }))
    ) {
        return Err(format!("wrong exact decimal: {decimal:?}"));
    }
    let array = object
        .get("arr")
        .ok_or_else(|| "arr missing".to_owned())?
        .array()
        .map_err(|error| format!("arr array: {error:?}"))?
        .ok_or_else(|| "arr was not an array".to_owned())?;
    if array.len() != 2 {
        return Err("wrong representative array size".into());
    }
    Ok(())
}

fn assert_number_rendering() -> Result<(), String> {
    // A decoded number renders byte-for-byte like jq's decNumber
    // to-scientific-string, independent of whether `jq` is installed: integers
    // pass through verbatim (`1`, `-0`); fractions keep their source trailing
    // zeroes (`1.000`, `10.250`); exponents reformat (`1e2` -> `1E+2`,
    // `1e-2` -> `0.01`, `12345e2` -> `1.2345E+6`); zeros keep their exponent
    // placement (`0.00`, `0e5` -> `0E+5`, `-0.0`); precision and past-f64-range
    // literals are preserved (`3e408` -> `3E+408`).
    let input = br"[1,-0,0.1,1.000,10.250,1e2,1e-2,12345e2,0.00,0e5,-0.0,3e408,0.30000000000000004]";
    let expected = br"[1,-0,0.1,1.000,10.250,1E+2,0.01,1.2345E+6,0.00,0E+5,-0.0,3E+408,0.30000000000000004]";
    // Decode and encode under one resource context so the located product's
    // arena and the encoder share an account.
    let mut resources = limited_resources(64);
    let registration = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(source(input), decode_request(), &mut resources)
        .map_err(|error| format!("number provider: {:?}", error.kind()))?;
    let demand = requirement(&resources, None);
    let handle = provider
        .bind(&demand)
        .map_err(|error| format!("number bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("number open: {:?}", error.kind()))?;
    let outcome = {
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4096);
        session
            .decode(&mut run)
            .map_err(|error| format!("number decode: {:?}", error.kind()))?
    };
    let (outcome, _report) = outcome.into_parts();
    let jqf_codec_core::AccessOutcome::FullDocument(product) = outcome else {
        return Err("number fixture did not return a document".into());
    };
    let root = product.document().root_handle();
    let format = product.document().format();
    let dialect = product
        .document()
        .dialect()
        .ok_or_else(|| "number fixture lacked dialect".to_owned())?;
    let item =
        EncodeItem::try_located(&product, root).map_err(|error| format!("number encode item: {:?}", error.kind()))?;
    let encoded = encode_item(item, format, dialect, &mut resources)?;
    if encoded != expected {
        return Err(format!("wrong number rendering: {}", String::from_utf8_lossy(&encoded)));
    }
    Ok(())
}

/// Blindness consumer 1: the ENCODER, over an OWNED integer in both storage
/// arms.
///
/// `jqf-data`'s `Integer` keeps a value in `i64` range inline (the machine arm)
/// and everything else on the heap. The encoder never asks which — it writes
/// the borrowed retained spelling verbatim — so this probe pins the published
/// bytes across the arm boundary: the ±10^k digit boundaries, 2^53±1, both
/// `i64` extremes, one step past each, the 19-versus-20-digit fits/doesn't-fit
/// pair, a magnitude far past `i64`, and the retained `-0` the machine arm
/// refuses precisely so those bytes survive.
///
/// It also covers the RESUMED-CURSOR write for free: `encode_item` accepts at
/// most 7 bytes per offer, so every spelling longer than that is published
/// across several resumed offers off the same borrowed buffer.
fn assert_owned_integer_arm_blindness() -> Result<(), String> {
    let mut resources = limited_resources(64);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| format!("format: {error:?}"))?;
    let dialect =
        DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| format!("dialect: {error:?}"))?;
    for spelling in [
        "0",
        "1",
        "-1",
        "10",
        "-10",
        "100",
        "1000",
        "9007199254740991",
        "9007199254740992",
        "9007199254740993",
        "999999999999999999",
        "1000000000000000000",
        "9223372036854775807",
        "-9223372036854775808",
        "9223372036854775808",
        "-9223372036854775809",
        "99999999999999999999",
        "12345678901234567890123456789",
    ] {
        let value = Value::Number(
            jqf_data::Number::try_json_literal(spelling)
                .map_err(|error| format!("owned integer {spelling}: {error:?}"))?,
        );
        let encoded = encode_item(EncodeItem::owned(&value), &format, &dialect, &mut resources)?;
        if encoded != spelling.as_bytes() {
            return Err(format!(
                "owned integer {spelling} published as {:?}",
                String::from_utf8_lossy(&encoded)
            ));
        }
    }
    let retained_negative_zero = Value::Number(jqf_data::Number::integer(
        jqf_data::Integer::from_canonical("-0".to_owned()).map_err(|error| format!("retained -0: {error:?}"))?,
    ));
    let encoded = encode_item(
        EncodeItem::owned(&retained_negative_zero),
        &format,
        &dialect,
        &mut resources,
    )?;
    if encoded != b"-0" {
        return Err(format!(
            "retained -0 published as {:?}",
            String::from_utf8_lossy(&encoded)
        ));
    }
    Ok(())
}

fn assert_encoding() -> Result<(), String> {
    let mut resources = limited_resources(64);
    let format = FormatId::try_new(jqf_codec_json::FORMAT_ID).map_err(|error| format!("format: {error:?}"))?;
    let dialect =
        DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).map_err(|error| format!("dialect: {error:?}"))?;
    let text = Value::try_string("line\nquote\"slash\\music𝄞").map_err(|error| format!("text value: {error:?}"))?;
    let encoded = encode_item(EncodeItem::owned(&text), &format, &dialect, &mut resources)?;
    if encoded != "\"line\\nquote\\\"slash\\\\music𝄞\"".as_bytes() {
        return Err(format!(
            "wrong owned JSON encoding: {:?}",
            String::from_utf8_lossy(&encoded)
        ));
    }
    let tagged = Value::try_tagged(
        TagId::try_new_unaccounted("!money").map_err(|error| format!("tag: {error:?}"))?,
        Value::Null,
    )
    .map_err(|error| format!("tagged value: {error:?}"))?;
    // The cross-format encode policy REPLACED this case's old law. It used to
    // require `InvalidTag` here — JSON refusing to encode a tagged value at all
    // — and the D1 ruling decided the other way: encode PUBLISHES the payload
    // (yq, RFC 8949), because a tag jq cannot spell is not a reason to refuse a
    // value jq can. The case is rewritten to the decided behaviour rather than
    // deleted: the surface it guards (a tagged owned value reaching the JSON
    // encoder) is exactly the one the ruling moved.
    let published = encode_item(EncodeItem::owned(&tagged), &format, &dialect, &mut resources)?;
    if published != b"null" {
        return Err(format!(
            "tagged value did not publish its payload: {:?}",
            String::from_utf8_lossy(&published)
        ));
    }
    let validator = jqf_codec_json::registration()
        .map_err(|error| format!("registration: {error:?}"))?
        .tag_validator()
        .expect("tag validator")
        .create_validator(encode_request(&format, &dialect), &mut resources)
        .map_err(|error| format!("validator: {:?}", error.kind()))?;
    if !matches!(validator.validate(&[], &resources), Ok(()))
        || !matches!(validator.validate(&[tagged.tag().expect("tag")], &resources), Err(error) if error.kind() == jqf_codec_core::CodecFailureKind::InvalidTag)
    {
        return Err("strict JSON target validator did not reject exact tags".into());
    }
    assert_deep_encode_cancellation(&format, &dialect)?;
    assert_empty_container_depth(&format, &dialect)?;
    assert_fact_preservation(&format, &dialect)?;
    Ok(())
}

fn assert_fact_preservation(format: &FormatId, dialect: &DialectId) -> Result<(), String> {
    let mut resources = limited_resources(64);
    let mut builder =
        AccountedDocumentBuilder::try_new(jqf_codec_json::FORMAT_ID, Some(jqf_codec_json::RFC8259_DIALECT_ID))
            .map_err(|error| format!("fact builder: {error:?}"))?;
    let root = builder
        .add_node("json.value", AccountedSemanticNode::Null, None, &resources)
        .map_err(|error| format!("fact root: {error:?}"))?;
    builder
        .add_fact(
            LocalOwnerRef::Node(root),
            "leading",
            "json.comment",
            1,
            &FactPayload::Text("comment".to_owned()),
            &resources,
        )
        .map_err(|error| format!("fact: {error:?}"))?;
    let document = builder
        .finish(root, &resources)
        .map_err(|error| format!("fact document: {error:?}"))?;
    let product = jqf_codec_core::DocumentProduct::try_new(document, &resources)
        .map_err(|error| format!("fact product: {:?}", error.kind()))?;
    let (_, report) = encode_item_with_report(
        EncodeItem::try_located(&product, product.document().root_handle())
            .map_err(|error| format!("fact item: {:?}", error.kind()))?,
        format,
        dialect,
        &mut resources,
    )?;
    if report.tags_and_facts() != PreservationOutcome::Omitted {
        return Err(format!("wrong root fact preservation: {report:?}"));
    }

    assert_selected_fact_preservation(format, dialect)?;
    let mut owned_resources = limited_resources(64);
    let owned = Value::Null;
    let (_, owned_report) = encode_item_with_report(EncodeItem::owned(&owned), format, dialect, &mut owned_resources)?;
    if owned_report.tags_and_facts() != PreservationOutcome::Exact {
        return Err(format!("wrong fact-free preservation: {owned_report:?}"));
    }
    Ok(())
}

fn assert_selected_fact_preservation(format: &FormatId, dialect: &DialectId) -> Result<(), String> {
    let mut selected_resources = limited_resources(64);
    let mut selected_builder =
        AccountedDocumentBuilder::try_new(jqf_codec_json::FORMAT_ID, Some(jqf_codec_json::RFC8259_DIALECT_ID))
            .map_err(|error| format!("selected builder: {error:?}"))?;
    let selected_root = selected_builder
        .add_node(
            "json.value",
            AccountedSemanticNode::Array { item_role: "json.item" },
            None,
            &selected_resources,
        )
        .map_err(|error| format!("selected root: {error:?}"))?;
    let selected_child = selected_builder
        .add_node("json.value", AccountedSemanticNode::Null, None, &selected_resources)
        .map_err(|error| format!("selected child: {error:?}"))?;
    selected_builder
        .add_occurrence(
            LocalOwnerRef::Node(selected_root),
            "json.item",
            None,
            selected_child,
            &selected_resources,
        )
        .map_err(|error| format!("selected occurrence: {error:?}"))?;
    selected_builder
        .add_fact(
            LocalOwnerRef::Node(selected_root),
            "leading",
            "json.comment",
            1,
            &FactPayload::Text("comment".to_owned()),
            &selected_resources,
        )
        .map_err(|error| format!("selected fact: {error:?}"))?;
    let selected_document = selected_builder
        .finish(selected_root, &selected_resources)
        .map_err(|error| format!("selected document: {error:?}"))?;
    let selected_handle = selected_document
        .node_handle(selected_child)
        .map_err(|error| format!("selected handle: {error:?}"))?;
    let selected_product = jqf_codec_core::DocumentProduct::try_new(selected_document, &selected_resources)
        .map_err(|error| format!("selected product: {:?}", error.kind()))?;
    let (_, selected_report) = encode_item_with_report(
        EncodeItem::try_located(&selected_product, selected_handle)
            .map_err(|error| format!("selected item: {:?}", error.kind()))?,
        format,
        dialect,
        &mut selected_resources,
    )?;
    if selected_report.tags_and_facts() != PreservationOutcome::Indeterminate {
        return Err(format!("wrong selected fact preservation: {selected_report:?}"));
    }
    Ok(())
}

fn assert_empty_container_depth(format: &FormatId, dialect: &DialectId) -> Result<(), String> {
    let mut exact_resources = limited_resources(1);
    let empty = Value::Array(Array::try_from_vec(Vec::new()).map_err(|error| format!("empty array: {error:?}"))?);
    let bytes = encode_item(EncodeItem::owned(&empty), format, dialect, &mut exact_resources)?;
    if bytes != b"[]" {
        return Err(format!("empty depth exact mismatch: {bytes:?}"));
    }

    let mut nested_resources = limited_resources(1);
    let nested =
        Value::Array(Array::try_from_vec(vec![empty]).map_err(|error| format!("nested empty array: {error:?}"))?);
    let error = encode_item(EncodeItem::owned(&nested), format, dialect, &mut nested_resources)
        .expect_err("nested empty array must consume both depth levels");
    if !error.contains("NestingDepth") {
        return Err(format!("wrong empty-container depth failure: {error}"));
    }
    Ok(())
}

fn assert_deep_encode_cancellation(format: &FormatId, dialect: &DialectId) -> Result<(), String> {
    // Straight-line encode (123 X4): the one-credit meter forces the encoder's
    // first admission to exhaust it, the wrapper's own check passes, and the
    // first replenish's control check lands Cancelled AFTER the encoder staged
    // its first quantum — the poll-era "drive to an outstanding offer, then
    // cancel" law without the offer seam.
    let control = CountCancelControl(core::sync::atomic::AtomicUsize::new(2));
    let mut resources = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 8 << 20, u64::MAX, 256))
            .map_err(|error| format!("deep account: {error:?}"))?,
        &control,
        WorkMeter::try_new_v1(1).ok_or_else(|| "deep meter overflow".to_owned())?,
    )
    .map_err(|error| format!("deep context: {error:?}"))?;
    let factory = jqf_codec_json::registration()
        .map_err(|error| format!("deep registration: {error:?}"))?
        .encoder()
        .expect("encoder")
        .create_factory(encode_request(format, dialect), &mut resources)
        .map_err(|error| format!("deep factory: {:?}", error.kind()))?;
    let mut value = Value::Null;
    for _ in 0..255 {
        value = Value::Array(Array::try_from_vec(vec![value]).map_err(|error| format!("deep array: {error:?}"))?);
    }
    // The baseline is read AFTER the value exists: its 255 nested arrays each
    // hold their own ledger residency for as long as the value lives, and this
    // check is about what the CANCELLED ENCODER retains, not about the input.
    let baseline = resources.snapshot().memory_current_bytes();
    let mut session = factory
        .start(EncodeItem::owned(&value), PreservationRequest::Report, &mut resources)
        .map_err(|error| format!("deep session: {:?}", error.kind()))?;
    let mut discarded = Vec::new();
    let kind = {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut discarded);
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session
            .encode(&mut sink, &mut run)
            .expect_err("cancel deep encode")
            .kind()
    };
    if !matches!(
        kind,
        jqf_codec_core::CodecFailureKind::Control(jqf_resource::ControlError::Cancelled)
    ) {
        return Err(format!("deep cancellation was not the armed cancel: {kind:?}"));
    }
    // The failure is terminal-stable: a retry re-raises the same kind.
    let retry = {
        let mut sink = jqf_codec_core::VecByteSink::new(&mut discarded);
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4_096);
        session
            .encode(&mut sink, &mut run)
            .expect_err("retry deep cancellation")
            .kind()
    };
    if retry != kind {
        return Err("deep cancellation was not terminal-stable".into());
    }
    drop(session);
    if resources.snapshot().memory_current_bytes() != baseline {
        return Err("deep cancelled encoder retained request memory".into());
    }
    Ok(())
}

fn encode_request<'a>(format: &'a FormatId, dialect: &'a DialectId) -> EncodeRequest<'a, 'static> {
    EncodeRequest {
        format,
        dialect,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        preservation: PreservationRequest::Report,
        options: None,
    }
}

fn encode_item(
    item: EncodeItem<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    resources: &mut ResourceContext<'_>,
) -> Result<Vec<u8>, String> {
    encode_item_with_report(item, format, dialect, resources).map(|(bytes, _)| bytes)
}

fn encode_item_with_report(
    item: EncodeItem<'_, '_>,
    format: &FormatId,
    dialect: &DialectId,
    resources: &mut ResourceContext<'_>,
) -> Result<(Vec<u8>, PreservationReport), String> {
    let registration = jqf_codec_json::registration().map_err(|error| format!("registration: {error:?}"))?;
    let factory = registration
        .encoder()
        .expect("encoder")
        .create_factory(encode_request(format, dialect), resources)
        .map_err(|error| format!("encoder factory: {:?}", error.kind()))?;
    let mut session = factory
        .start(item, PreservationRequest::Report, resources)
        .map_err(|error| format!("encoder start: {:?}", error.kind()))?;
    let mut output = Vec::new();
    // The bounded-acceptance sink (7 bytes per write) forces the encoder's
    // `write_all` to retry partial writes — the byte-identity successor of the
    // poll-era partial-acknowledgement receipt.
    let report = {
        let mut sink = PartialSink {
            target: &mut output,
            limit: 7,
        };
        let mut run = jqf_codec_core::CodecRunContext::new(resources);
        run.set_cooperative_credits(4096);
        session
            .encode(&mut sink, &mut run)
            .map_err(|error| format!("encode: {:?}", error.kind()))?
    };
    Ok((output, report))
}

fn assert_string(
    value: Result<Option<jqf_data::ValueView<'_, '_>>, jqf_data::DataError>,
    expected: &str,
) -> Result<(), String> {
    let scalar = value
        .map_err(|error| format!("string lookup: {error:?}"))?
        .ok_or_else(|| "string missing".to_owned())?
        .scalar()
        .map_err(|error| format!("string scalar: {error:?}"))?;
    if !matches!(scalar, Some(ScalarView::String(value)) if value == expected) {
        return Err(format!("wrong string value: {scalar:?}"));
    }
    Ok(())
}

fn assert_integer(value: Result<Option<jqf_data::ValueView<'_, '_>>, String>, expected: &str) -> Result<(), String> {
    let scalar = value?
        .ok_or_else(|| "integer missing".to_owned())?
        .scalar()
        .map_err(|error| format!("integer scalar: {error:?}"))?;
    if !matches!(scalar, Some(ScalarView::Number(NumberView::Integer(value))) if value == expected) {
        return Err(format!("wrong integer value: {scalar:?}"));
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "one lifecycle receipt keeps drop order and shared ledger visible"
)]
fn lifecycle_receipts() -> Result<(), String> {
    let registration = jqf_codec_json::registration().map_err(|error| format!("{error:?}"))?;
    let mut resources = limited_resources(64);
    let baseline = resources.snapshot().memory_current_bytes();
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(source(b"+1"), decode_request(), &mut resources)
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let routes = provider.route_descriptions();
    if routes.len() != 2
        || routes[0].slot().get() != 0
        || routes[0].bundle().footprint() != AccessFootprintKind::Whole
        || routes[0].bundle().result() != AccessResultKind::CompleteDocument
        || routes[1].slot().get() != 1
        || routes[1].bundle().footprint() != AccessFootprintKind::Exact
        || routes[1].bundle().result() != AccessResultKind::Located
    {
        return Err("strict JSON did not advertise full at slot 0, scoped at slot 1".into());
    }
    assert_invalid_forward_shapes(&resources)?;
    let demand = requirement(&resources, None);
    let handle = provider.bind(&demand).map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open inspected source: {:?}", error.kind()))?;
    let first = {
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        session.decode(&mut run).expect_err("invalid source must fail")
    };
    let second = {
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        session.decode(&mut run).expect_err("terminal failure must be stable")
    };
    if first.kind() != second.kind() || first.diagnostic().is_none() {
        return Err("terminal error classification or diagnostic was not stable".into());
    }
    drop(first);
    drop(second);
    drop(session);
    drop(provider);
    drop(demand);
    if resources.snapshot().memory_current_bytes() != baseline {
        return Err(format!(
            "accounted memory remained after provider and session drop: {:?}",
            resources.snapshot()
        ));
    }

    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(source(br#"{"ok":[1,2,3]}"#), decode_request(), &mut resources)
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let demand = requirement(&resources, None);
    let handle = provider.bind(&demand).map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut resources)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    let outcome = {
        let mut run = jqf_codec_core::CodecRunContext::new(&mut resources);
        run.set_cooperative_credits(4096);
        session
            .decode(&mut run)
            .map_err(|error| format!("successful decode: {:?}", error.kind()))?
    };
    let (outcome, report) = outcome.into_parts();
    if report.adapter() != AccessAdapter::None || report.route().is_none() {
        return Err("successful full route report was not sealed".into());
    }
    let jqf_codec_core::AccessOutcome::FullDocument(product) = outcome else {
        return Err("successful full route did not return a document".into());
    };
    let root = product.document().root_handle();
    let located =
        EncodeItem::try_located(&product, root).map_err(|error| format!("located encode item: {:?}", error.kind()))?;
    let format = product.document().format();
    let dialect = product
        .document()
        .dialect()
        .ok_or_else(|| "strict JSON document lacked dialect".to_owned())?;
    let encoded = encode_item(located, format, dialect, &mut resources)?;
    if encoded != br#"{"ok":[1,2,3]}"# {
        return Err(format!(
            "wrong located JSON encoding: {:?}",
            String::from_utf8_lossy(&encoded)
        ));
    }
    drop(product);
    drop(session);
    drop(provider);
    drop(demand);
    if resources.snapshot().memory_current_bytes() != baseline {
        return Err(format!(
            "successful product did not return ledger memory after final owner drop: {:?}",
            resources.snapshot()
        ));
    }

    let mut owner = limited_resources(64);
    let mut foreign = limited_resources(64);
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(source(b"null"), decode_request(), &mut owner)
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let demand = requirement(&owner, None);
    let handle = provider.bind(&demand).map_err(|error| format!("bind: {error:?}"))?;
    // The account anchor is gone (122 W2-T5): the ambient allocator charged
    // the provider's bytes at allocation time, so an access handle is not
    // account-bound and opening it under another request's context succeeds.
    provider
        .open(&handle, &mut foreign)
        .map_err(|error| format!("cross-context open: {error:?}"))?;

    let control = ToggleControl(core::sync::atomic::AtomicBool::new(false));
    let mut cancelled = ResourceContext::new(
        RequestAccount::try_new(ResourceLimits::new(u64::MAX, u64::MAX, 1 << 20, u64::MAX, 64)).expect("account"),
        &control,
        WorkMeter::try_new_v1(4096).expect("meter"),
    )
    .expect("context");
    let mut provider = registration
        .decoder()
        .expect("decoder")
        .create_provider(source(b"null"), decode_request(), &mut cancelled)
        .map_err(|error| format!("provider: {:?}", error.kind()))?;
    let demand = requirement(&cancelled, None);
    let handle = provider.bind(&demand).map_err(|error| format!("bind: {error:?}"))?;
    let mut session = provider
        .open(&handle, &mut cancelled)
        .map_err(|error| format!("open: {:?}", error.kind()))?;
    control.0.store(true, core::sync::atomic::Ordering::Relaxed);
    let mut run = jqf_codec_core::CodecRunContext::new(&mut cancelled);
    if !matches!(session.decode(&mut run), Err(error) if matches!(error.kind(), jqf_codec_core::CodecFailureKind::Control(jqf_resource::ControlError::Cancelled)))
    {
        return Err("cancellation was not terminal before parser work".into());
    }
    Ok(())
}

fn assert_invalid_forward_shapes(resources: &ResourceContext<'_>) -> Result<(), String> {
    // The illegal structural pairings are UNEXPRESSIBLE through the named
    // constructors; the one runtime-rejected shape left is the exact
    // constructor handed a whole footprint.
    let whole = AccessFootprint::try_whole(resources);
    if AccessRequirement::try_exact(
        whole,
        CodecDemand::try_new(resources),
        AccessGuarantees::strict(DiagnosticPolicy::ErrorsOnly),
        resources,
    )
    .is_ok()
    {
        return Err("the exact constructor accepted a whole footprint".into());
    }
    let exact = AccessFootprint::try_exact(ExactPath::try_new(resources), resources);
    if SelectionSchedule::try_empty_complete(&exact, resources).is_ok() {
        return Err("an exact footprint accepted a complete schedule".into());
    }
    Ok(())
}

fn decode_request() -> DecodeRequest<'static> {
    let dialect: &'static DialectId = Box::leak(Box::new(
        DialectId::try_new(jqf_codec_json::RFC8259_DIALECT_ID).expect("dialect"),
    ));
    DecodeRequest {
        validation: ValidationMode::Strict,
        diagnostics: DiagnosticPolicy::ErrorsOnly,
        dialect,
        options: None,
        allow_adjacent_values: false,
        value_separator: jqf_codec_json::VALUE_SEPARATORS,
    }
}
