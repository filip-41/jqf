//! Integration tests for the json-seq framer's framing law.
//!
//! These pin the RFC 7464 framing law as implemented: RS boundaries, §2.4 truncation, coalescing, the trailing-RS
//! tail, unframed input, and the strict-vs-recovering profile split. Every case is byte-level.

mod common;

use jqf_codec_core::{
    CodecRunContext, DiagnosticPolicy, RecordBatch, RecordBatchLimit, RecordEntry, RecordPoll, RouteSlot,
};
use jqf_codec_json::seq::{JsonSeqDecodeOptions, JsonSeqProfile};
use jqf_source::{ResolvedSource, SourceId, SourceKind, SourceRef};

/// Drives one profile to completion, returning the payloads, the issue codes, and whether the stream ended cleanly.
fn drive(bytes: &[u8], profile: JsonSeqProfile) -> (Vec<Vec<u8>>, Vec<u8>, bool) {
    let mut resources = common::resources();
    let source = ResolvedSource::new(
        SourceRef::new(SourceId::new(1), SourceKind::Input),
        "test.json-seq",
        bytes,
        0,
    );
    let options = JsonSeqDecodeOptions::try_new(None, 1 << 20).expect("ceiling");
    let mut provider = jqf_codec_json::seq::create_record_provider(
        source,
        profile,
        options,
        DiagnosticPolicy::ErrorsOnly,
        profile.validation(),
        &mut resources,
    )
    .expect("provider");
    let mut stream = provider
        .open_record_route(RouteSlot::new(0), &mut resources)
        .expect("route");
    let limit = RecordBatchLimit::new(64, 1 << 20).expect("limit");
    let mut batch = RecordBatch::new();
    let mut payloads = Vec::new();
    let mut codes = Vec::new();
    let mut completed = false;
    loop {
        batch.clear();
        let mut run = CodecRunContext::new(&mut resources);
        let poll = stream.poll(limit, &mut batch, &mut run);
        match poll {
            Ok(RecordPoll::End(_)) => {
                completed = true;
                break;
            }
            Ok(RecordPoll::Pending) => {
                resources.try_begin_next_cooperative_entry(4_096).expect("resume");
                continue;
            }
            Ok(RecordPoll::Filled) => {}
            Err(_) => break,
        }
        for entry in batch.entries() {
            match entry {
                RecordEntry::Record(record) => payloads.push(record.lease().payload().to_vec()),
                RecordEntry::Issue(issue) => codes.push(code_tag(issue.code())),
            }
        }
    }
    (payloads, codes, completed)
}

fn code_tag(code: jqf_codec_core::RecordIssueCode) -> u8 {
    match code {
        jqf_codec_core::RecordIssueCode::TruncatedTopLevelScalar => 1,
        jqf_codec_core::RecordIssueCode::UnframedInput => 2,
        jqf_codec_core::RecordIssueCode::MalformedPayload => 3,
        jqf_codec_core::RecordIssueCode::OversizeRecord => 4,
        _ => 0,
    }
}

const RS: u8 = 0x1e;

fn frame(items: &[&str]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for item in items {
        bytes.push(RS);
        bytes.extend_from_slice(item.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

#[test]
fn a_well_framed_stream_delivers_every_item() {
    let input = frame(&["{\"a\":1}", "{\"b\":2}", "null "]);
    let (payloads, codes, completed) = drive(&input, JsonSeqProfile::Strict);
    assert!(completed);
    assert!(codes.is_empty());
    assert_eq!(
        payloads,
        vec![b"{\"a\":1}\n".to_vec(), b"{\"b\":2}\n".to_vec(), b"null \n".to_vec()]
    );
}

#[test]
fn the_final_item_does_not_need_a_terminating_rs() {
    let (payloads, codes, completed) = drive(b"\x1e{\"a\":1}\n\x1e{\"b\":2}", JsonSeqProfile::Strict);
    assert!(completed);
    assert!(codes.is_empty());
    assert_eq!(payloads, vec![b"{\"a\":1}\n".to_vec(), b"{\"b\":2}".to_vec()]);
}

#[test]
fn the_truncated_scalar_canaries_are_rejected_by_strict_and_issued_by_recovering() {
    // `<RS>123<RS>` and `<RS>true<RS>` are RFC 7464 §2.4 canaries.
    for canary in [b"\x1e123\x1e{\"b\":2}\n".to_vec(), b"\x1etrue\x1e{\"b\":2}\n".to_vec()] {
        let (payloads, _codes, completed) = drive(&canary, JsonSeqProfile::Strict);
        assert!(!completed, "strict must reject {canary:02x?}");
        assert!(payloads.is_empty(), "strict must publish nothing for {canary:02x?}");
        let (payloads, codes, completed) = drive(&canary, JsonSeqProfile::Recovering);
        assert!(completed, "recovering must complete {canary:02x?}");
        assert_eq!(payloads, vec![b"{\"b\":2}\n".to_vec()]);
        assert_eq!(codes, vec![1], "the truncation is a reported issue");
    }
}

#[test]
fn a_scalar_with_trailing_json_whitespace_is_complete() {
    // Space, tab, LF, and CR all satisfy §2.4.
    for item in ["123 ", "123\t", "123\n", "123\r"] {
        let mut input = Vec::new();
        input.push(RS);
        input.extend_from_slice(item.as_bytes());
        input.push(RS);
        input.push(b' ');
        input.extend_from_slice(b"{\"b\":2}");
        let (payloads, codes, completed) = drive(&input, JsonSeqProfile::Strict);
        assert!(completed);
        assert!(codes.is_empty());
        assert_eq!(payloads.len(), 2, "item {item:?} must parse");
    }
}

#[test]
fn a_number_at_eof_without_whitespace_is_truncated() {
    let (payloads, _codes, completed) = drive(b"\x1e123", JsonSeqProfile::Strict);
    assert!(!completed);
    assert!(payloads.is_empty());
    let (payloads, codes, completed) = drive(b"\x1e123", JsonSeqProfile::Recovering);
    assert!(completed);
    assert!(payloads.is_empty());
    assert_eq!(codes, vec![1]);
}

#[test]
fn bare_literals_at_eof_without_whitespace_are_truncated_too() {
    // §2.4 applies UNIFORMLY: a top-level `true`, `false`, or `null` is not self-delimiting any more than a number is,
    // so an EOF with no separating whitespace faults identically — terminal under strict, one advisory under
    // recovering. (the unfinished-at-EOF check is NUMBER-only and accepts these; jqf is deliberately the stricter side,
    // matching the recorded divergence pinned by the mid-stream canaries.)
    for literal in ["true", "false", "null"] {
        let mut input = Vec::new();
        input.push(RS);
        input.extend_from_slice(literal.as_bytes());
        let (payloads, _codes, completed) = drive(&input, JsonSeqProfile::Strict);
        assert!(!completed, "strict must reject {literal} at EOF");
        assert!(payloads.is_empty(), "strict publishes nothing for {literal}");
        let (payloads, codes, completed) = drive(&input, JsonSeqProfile::Recovering);
        assert!(completed, "recovering completes past {literal} at EOF");
        assert!(payloads.is_empty());
        assert_eq!(codes, vec![1], "{literal}'s truncation is a reported issue");
    }
}

#[test]
fn trailing_rs_is_a_strict_failure_and_silent_in_recovering() {
    let input = b"\x1e{\"a\":1}\n\x1e";
    let (payloads, _codes, completed) = drive(input, JsonSeqProfile::Strict);
    assert!(!completed, "trailing RS is an unterminated zero-byte item in strict");
    assert_eq!(payloads, vec![b"{\"a\":1}\n".to_vec()], "the earlier item still stands");
    let (payloads, codes, completed) = drive(input, JsonSeqProfile::Recovering);
    assert!(completed, "the reference accepts a trailing RS silently");
    assert!(codes.is_empty(), "recovering discards the tail without an issue");
    assert_eq!(payloads, vec![b"{\"a\":1}\n".to_vec()]);
}

#[test]
fn an_rs_only_input_is_a_strict_failure_and_silent_in_recovering() {
    for input in [b"\x1e".to_vec(), b"\x1e\x1e".to_vec()] {
        let (_, _, completed) = drive(&input, JsonSeqProfile::Strict);
        assert!(!completed, "RS-only input is not zero-item success in strict");
        let (_, codes, completed) = drive(&input, JsonSeqProfile::Recovering);
        assert!(completed);
        assert!(codes.is_empty());
    }
}

#[test]
fn empty_input_is_a_valid_zero_item_stream_under_both_profiles() {
    for profile in [JsonSeqProfile::Strict, JsonSeqProfile::Recovering] {
        let (payloads, codes, completed) = drive(b"", profile);
        assert!(completed);
        assert!(payloads.is_empty());
        assert!(codes.is_empty());
    }
}

#[test]
fn unframed_input_is_a_strict_failure_and_one_advisory_in_recovering() {
    let (_, _, completed) = drive(b"{\"a\":1}\n", JsonSeqProfile::Strict);
    assert!(!completed);
    let (payloads, codes, completed) = drive(b"{\"a\":1}\n", JsonSeqProfile::Recovering);
    assert!(completed);
    assert!(payloads.is_empty());
    assert_eq!(
        codes,
        vec![2],
        "one unframed-input advisory (the reference's abandoned text)"
    );
}

#[test]
fn bytes_before_the_first_rs_are_dropped() {
    // The reference's resync law: the unframed prefix is skipped, later items parse.
    let (payloads, codes, completed) = drive(b"{\"a\":1}\x1e{\"b\":2}\n", JsonSeqProfile::Strict);
    assert!(completed);
    assert!(codes.is_empty());
    assert_eq!(payloads, vec![b"{\"b\":2}\n".to_vec()]);
}

#[test]
fn consecutive_rs_bytes_coalesce() {
    let (payloads, codes, completed) = drive(b"\x1e\x1e{\"a\":1}\n", JsonSeqProfile::Strict);
    assert!(completed);
    assert!(codes.is_empty());
    assert_eq!(payloads, vec![b"{\"a\":1}\n".to_vec()]);
}

#[test]
fn an_rs_inside_a_string_still_boundaries() {
    // A raw RS is a boundary even mid-string: the framer frames the `"a` unit; the strict JSON ladder (not the framer)
    // later rejects its unterminated string as a malformed payload.
    let (payloads, codes, completed) = drive(b"\x1e\"a\x1e\x1e{\"b\":2}\n", JsonSeqProfile::Recovering);
    assert!(completed);
    assert!(codes.is_empty(), "the framer itself reports no issue for the unit");
    assert_eq!(
        payloads,
        vec![b"\"a".to_vec(), b"{\"b\":2}\n".to_vec()],
        "the mid-string RS still boundaries the unit"
    );
}

#[test]
fn an_rs_inside_a_string_boundaries_under_strict_too() {
    // The strict twin of `an_rs_inside_a_string_still_boundaries`: the framer frames the `"a` unit and the payload
    // ladder rejects it as a terminal strict fault — the published prefix keeps the earlier record.
    let (payloads, codes, completed) = drive(b"\x1e{\"b\":2}\n\x1e\"a\x1e", JsonSeqProfile::Strict);
    assert!(!completed, "the malformed unit is terminal under strict");
    // The earlier record stays published in its batch even though a later unit's payload faulted (the prefix-keep law).
    assert_eq!(payloads, vec![b"{\"b\":2}\n".to_vec(), b"\"a".to_vec()]);
    // The strict fault is TERMINAL (raised on the next poll), not an ordered issue: the batch carries no codes.
    assert!(codes.is_empty());
}

#[test]
fn a_whitespace_only_tail_is_valid() {
    // `{"a":1}` then a whitespace-only final unit: a valid empty possible-JSON.
    let (payloads, codes, completed) = drive(b"\x1e{\"a\":1}\n\x1e  \n\t", JsonSeqProfile::Strict);
    assert!(completed);
    assert!(codes.is_empty());
    assert_eq!(payloads, vec![b"{\"a\":1}\n".to_vec()]);
}

#[test]
fn the_framer_delivers_malformed_units_for_the_payload_ladder() {
    // Payload GRAMMAR is the strict JSON ladder's, never the framer's: a unit whose significant span starts with a
    // self-delimiting character is framed as a record, and the ladder (reached through the SDK) rejects it. The
    // framer's own fault classes are §2.4 truncation and oversize only.
    let input = b"\x1e{bad\n\x1e{\"b\":2}\n\x1e{nope\n\x1e{\"c\":3}\n";
    let (payloads, codes, completed) = drive(input, JsonSeqProfile::Strict);
    assert!(
        completed,
        "the framer frames every unit; the ladder rejects the bad ones"
    );
    assert!(codes.is_empty());
    assert_eq!(
        payloads,
        vec![
            b"{bad\n".to_vec(),
            b"{\"b\":2}\n".to_vec(),
            b"{nope\n".to_vec(),
            b"{\"c\":3}\n".to_vec()
        ]
    );
}
