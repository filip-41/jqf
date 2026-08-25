//! Integration tests for the C ABI contract itself: the surface `bindings/`
//! consumes. These call the exported `unsafe extern "C"` entry points exactly
//! as a binding does (a handle per test, `jqf_run`/`jqf_run_sequence` over a
//! caller buffer, `jqf_diag_count`/`jqf_free` for lifecycle) rather than
//! reaching for any private helper, so a break here is a break a binding
//! would actually observe.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt::Write;
use std::ptr::{self, from_mut};

use jqf_sdk_ffi::{
    codes, jqf_abi_version, jqf_diag_count, jqf_diag_dropped, jqf_diag_free_text, jqf_diag_get, jqf_free, jqf_new,
    jqf_new_limited, jqf_run, jqf_run_sequence,
};

/// The ASCII severity tag as the FFI's `c_char`. Neither `as` nor
/// `cast_signed()` is portable: `b'E' as c_char` is clippy-flagged where
/// `c_char` is signed (the `cast_possible_wrap` lint), and `cast_signed()`
/// is the WRONG type where `c_char` is unsigned (aarch64-linux). The byte is
/// < 0x80 so it fits every `c_char`; `try_from` is the one spelling no clippy
/// cast lint sees, and the unwrap is provably unreachable.
fn severity_tag() -> c_char {
    c_char::try_from(b'E').expect("ASCII severity tag fits c_char")
}

/// The ASCII warning tag, same portability law as [`severity_tag`].
fn warning_tag() -> c_char {
    c_char::try_from(b'W').expect("ASCII severity tag fits c_char")
}

/// One retained diagnostic record read through `jqf_diag_get`, with its text
/// allocations freed on drop (a test that forgets to read a field cannot
/// leak; a test that reads a field gets the owned text).
struct Diag {
    code: u16,
    revision: u16,
    class: c_char,
    severity: c_char,
    catchable: u8,
    caught: u32,
    step_index: u32,
    input_ordinal: u64,
    byte_offset: u64,
    halt_status: i32,
    kind: *mut c_char,
    operand: *mut c_char,
    payload: *mut c_char,
}

impl Diag {
    fn payload(&self) -> Option<String> {
        text(self.payload)
    }

    fn kind(&self) -> Option<String> {
        text(self.kind)
    }
}

fn text(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        // SAFETY: the pointer is a `jqf_diag_get`-produced C string, live
        // until the owning `Diag` drops.
        Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
    }
}

impl Drop for Diag {
    fn drop(&mut self) {
        // SAFETY: each pointer is a `CString::into_raw` from `jqf_diag_get`,
        // freed exactly once here (the `jqf_diag_free_text` contract).
        unsafe {
            jqf_diag_free_text(self.kind);
            jqf_diag_free_text(self.operand);
            jqf_diag_free_text(self.payload);
        }
    }
}

/// Reads one retained record into every field the ABI exposes (the locators
/// and the halt status ride `jqf_diag_get` now).
fn read_diag(handle: *mut c_void, index: u32) -> Diag {
    let mut diag = Diag {
        code: 0,
        revision: 0,
        class: 0 as c_char,
        severity: 0 as c_char,
        catchable: 0,
        caught: 0,
        step_index: 0,
        input_ordinal: 0,
        byte_offset: 0,
        halt_status: 0,
        kind: ptr::null_mut(),
        operand: ptr::null_mut(),
        payload: ptr::null_mut(),
    };
    // SAFETY: every out-parameter is a valid, aligned, writable local slot of
    // its declared type; `handle` is live.
    let rc = unsafe {
        jqf_diag_get(
            handle,
            index,
            from_mut(&mut diag.code),
            from_mut(&mut diag.revision),
            from_mut(&mut diag.class),
            from_mut(&mut diag.severity),
            from_mut(&mut diag.catchable),
            from_mut(&mut diag.caught),
            from_mut(&mut diag.step_index),
            from_mut(&mut diag.input_ordinal),
            from_mut(&mut diag.byte_offset),
            from_mut(&mut diag.halt_status),
            from_mut(&mut diag.kind),
            from_mut(&mut diag.operand),
            from_mut(&mut diag.payload),
        )
    };
    assert_eq!(rc, 0, "jqf_diag_get({index}) failed");
    diag
}

// --- the resident feed surface ---------------------------------------------
//
// The feed is the record route fed incrementally (pull-buffer): push input in
// pieces, poll bounded batches of output with the `snprintf` required-size
// convention. The feed id is a handle-LOCAL `u32` table id with the same
// never-reused, every-misuse-is-a-defined-`-1` law as the program table. The
// tests below pin the four laws the module doc promises: byte identity with
// the per-record compiled path, the id laws, the setup-failure law (a strict
// fault is `-1` with the record retained), and the batch-bound law.

use jqf_sdk_ffi::{
    JQF_FEED_PROFILE_RECOVERING, JQF_FEED_PROFILE_STRICT, RECORD_BATCH_ENTRIES, jqf_feed_close, jqf_feed_finish,
    jqf_feed_open, jqf_feed_poll, jqf_feed_push,
};

fn open_feed(handle: *mut c_void, program_id: u32, profile: i32) -> u32 {
    let mut id = 0u32;
    // SAFETY: `handle` is live, `program_id` is validated by the ABI, and
    // `id` is a live local slot — exactly `jqf_feed_open`'s `# Safety`
    // contract.
    let rc = unsafe { jqf_feed_open(handle, program_id, profile, from_mut(&mut id)) };
    assert_eq!(rc, 0, "jqf_feed_open failed");
    id
}

fn feed_push(handle: *mut c_void, id: u32, input: &[u8]) -> i64 {
    // SAFETY: `handle` is live, `id` is validated by the ABI, and `input` is
    // readable for its length.
    unsafe { jqf_feed_push(handle, id, input.as_ptr(), input.len()) }
}

fn feed_poll(handle: *mut c_void, id: u32, out: &mut [u8]) -> i64 {
    // SAFETY: `handle` is live, `id` is validated by the ABI, and `out` is
    // valid for its length.
    unsafe { jqf_feed_poll(handle, id, out.as_mut_ptr(), out.len()) }
}

/// Drains one feed's output into a Vec by polling until 0 (or -1, which the
/// caller's test then asserts on).
fn drain_feed(handle: *mut c_void, id: u32) -> (Vec<u8>, i64) {
    let mut out = vec![0u8; 65536];
    let mut bytes = Vec::new();
    loop {
        let written = feed_poll(handle, id, &mut out);
        if written <= 0 {
            return (bytes, written);
        }
        bytes.extend_from_slice(&out[..usize::try_from(written).unwrap()]);
    }
}

/// Counts the newline-terminated items in published bytes: the line count a
/// batch-bound test asserts on.
#[expect(
    clippy::naive_bytecount,
    reason = "a bytecount dependency for one test helper is not warranted"
)]
fn count_lines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&byte| byte == b'\n').count()
}

/// A feed over well-formed NDJSON publishes EXACTLY what the compiled
/// per-record path publishes for the same records — the additive-surface law
/// for the feed, in the same shape `compiled_runs_match_per_call_runs` pins
/// for the compiled path.
#[test]
fn a_feed_matches_the_per_record_compiled_path_byte_for_byte() {
    let handle = new_handle();
    let id = compile(handle, ".a");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);

    let stream = b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n";
    let pushed = feed_push(handle, feed, stream);
    assert_eq!(
        pushed,
        i64::try_from(stream.len()).unwrap(),
        "push returns the retained count"
    );

    let (output, written) = drain_feed(handle, feed);
    assert_eq!(written, 0, "a well-formed strict feed completes with a 0 poll");
    assert_eq!(output, b"1\n2\n3\n", "feed output must match the record route");

    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// The held tail law — a partial record pushed, polled, completed by a
/// later push, and published only after its terminator arrives (the record
/// route's exact cut: a record is complete only after its physical
/// terminator).
#[test]
fn a_partial_record_is_held_until_its_terminator_arrives() {
    let handle = new_handle();
    let id = compile(handle, ".a");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    let mut out = vec![0u8; 4096];

    // `{"a":1}\n` is complete; `{"a":` is a held partial record.
    let pushed = feed_push(handle, feed, b"{\"a\":1}\n{\"a\":");
    assert_eq!(pushed, 13, "retained count after the first push");
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, 2, "the complete record publishes");
    assert_eq!(&out[..2], b"1\n");
    // The partial tail is held: polling with nothing new publishes nothing.
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, 0, "the held tail must not frame or publish");
    // The tail completes only when its terminator arrives.
    assert!(feed_push(handle, feed, b"2}") >= 0);
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, 0, "still no terminator: the tail is still held");
    assert!(feed_push(handle, feed, b"\n") >= 0);
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, 2, "the completed record publishes");
    assert_eq!(&out[..2], b"2\n");

    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// The finish law — `jqf_feed_finish` marks the clean end of the stream:
/// the held partial record becomes the stream's FINAL record and is
/// delivered by the next poll (a complete final value without its
/// terminator is accepted, JSON Lines' own law), later polls drain to 0 —
/// never `-1` — and pushes after the finish are accepted-and-ignored.
#[test]
fn a_finished_feed_delivers_the_held_tail_and_drains_to_zero() {
    let handle = new_handle();
    let id = compile(handle, ".a");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    let mut out = vec![0u8; 4096];

    // A complete final record with NO terminator: held while the stream is
    // still open.
    assert!(feed_push(handle, feed, b"{\"a\":7}") >= 0);
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, 0, "the unterminated tail stays held before finish");

    // SAFETY: `handle` is live and `feed` names it.
    let rc = unsafe { jqf_feed_finish(handle, feed) };
    assert_eq!(rc, 0, "finish succeeds on a live feed");
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, 2, "the held tail is delivered as the FINAL record");
    assert_eq!(&out[..2], b"7\n");
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, 0, "after the tail the feed drains to 0, never -1");

    // A push after the finish is accepted (it returns the RETAINED count,
    // which stays empty) and ignored: the poll count stays at 0.
    assert_eq!(feed_push(handle, feed, b"{\"a\":8}\n"), 0);
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, 0);

    // Finishing a dead id is a defined -1.
    // SAFETY: `handle` is live; 9999 names no feed on it.
    let rc = unsafe { jqf_feed_finish(handle, 9999) };
    assert_eq!(rc, -1);

    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// The setup-failure law — a strict PAYLOAD fault is the terminal `-1`
/// with the failure's diagnostic record retained (`MACHINE_INPUT`), and the
/// records before the faulting one are delivered first (the framer's
/// batch-returned, fault-raised-next law).
#[test]
fn a_strict_payload_fault_is_minus_one_with_the_failure_record_retained() {
    let handle = new_handle();
    let id = compile(handle, ".a");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    let mut out = vec![0u8; 4096];

    // Record 2's payload `{"a":2` is not one complete strict-JSON text.
    assert!(feed_push(handle, feed, b"{\"a\":1}\n{\"a\":2\n") >= 0);
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(
        written, 2,
        "record 1 publishes before the failing record's batch is delivered"
    );
    assert_eq!(&out[..2], b"1\n");
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, -1, "the strict fault is the terminal -1");

    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert!(
        count >= 1,
        "the death poll must retain the failure record (got {count})"
    );
    // The failure record is the LAST retained record: the record drive emits
    // its own validate-phase record when the payload fails, and the feed
    // appends the terminal failure after it.
    let last = read_diag(handle, (count - 1) as u32);
    assert_eq!(
        last.code,
        codes::MACHINE_INPUT,
        "a payload fault retains the codec-input failure code"
    );
    assert_eq!(last.severity, severity_tag(), "a payload fault is an error");

    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// The same setup-failure law for a strict FRAMING fault — a bare
/// carriage return not followed by a line feed is a framing fault, reported
/// as the terminal `-1` with the faulting record retained (never a silent
/// continuation, never an empty-stream sentinel).
#[test]
fn a_strict_framing_fault_is_minus_one_with_the_failure_record_retained() {
    let handle = new_handle();
    let id = compile(handle, ".");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    let mut out = vec![0u8; 4096];

    // `\rX` — a carriage return not followed by a line feed — is a framing
    // fault inside the completed range.
    assert!(feed_push(handle, feed, b"{\"a\":1}\rX\n") >= 0);
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, -1, "a strict framing fault is the terminal -1");

    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(count, 1, "the framing fault must retain its failure record");
    // The retained record IS the fault: the codec-input family at error
    // severity, exactly as a payload fault retains (the count alone would
    // pass for any unrelated whisper).
    let failure = read_diag(handle, 0);
    assert_eq!(
        failure.code,
        codes::MACHINE_INPUT,
        "a framing fault retains the codec-input failure code"
    );
    assert_eq!(failure.severity, severity_tag(), "a framing fault is an error");

    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// The recovering profile continues past faults as ordered issues — a
/// blank record (advisory) and a malformed payload (error-severity issue)
/// both leave the feed ALIVE and its output byte-identical to the whole-input
/// recovering run's: the good records' bytes, in order.
#[test]
fn a_recovering_feed_continues_past_faults_as_ordered_issues() {
    let handle = new_handle();
    let id = compile(handle, ".a");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_RECOVERING);

    // Record 2 is blank; record 4's payload `{bad` is malformed JSON.
    let stream = b"{\"a\":1}\n\n{\"a\":3}\n{bad\n{\"a\":5}\n";
    assert!(feed_push(handle, feed, stream) >= 0);
    // One poll carries the whole small batch: the good records' bytes, in
    // order, on a SUCCESSFUL (never -1) poll.
    let mut out = vec![0u8; 4096];
    let written = feed_poll(handle, feed, &mut out);
    let published = usize::try_from(written).unwrap();
    assert_eq!(
        &out[..published],
        b"1\n3\n5\n",
        "the recovering feed publishes the good records' bytes in order"
    );

    // The ordered issues are IDENTIFIED, not merely counted: each retains
    // the codec-input family with its own framing kind, severity, and
    // record ordinal — the blank record's advisory first, then the
    // malformed payload's error, in encounter order.
    let count = unsafe { jqf_diag_count(handle) };
    let mut issues = Vec::new();
    for i in 0..count {
        let diag = read_diag(handle, i);
        if diag.code == codes::MACHINE_INPUT {
            issues.push((diag.kind(), diag.severity, diag.input_ordinal));
        }
    }
    assert_eq!(
        issues.len(),
        2,
        "both faults must surface as ordered issues (got {issues:?})"
    );
    assert_eq!(issues[0].0.as_deref(), Some("blank-record"));
    assert_eq!(issues[0].1, warning_tag(), "a blank record is an advisory issue");
    assert_eq!(issues[0].2, 1, "the blank record is ordinal 1");
    assert_eq!(issues[1].0.as_deref(), Some("malformed-record-payload"));
    assert_eq!(
        issues[1].1,
        severity_tag(),
        "a malformed payload is an error-severity issue"
    );
    assert_eq!(issues[1].2, 3, "the malformed payload is ordinal 3");

    // Draining to completion stays clean.
    let (rest, written) = drain_feed(handle, feed);
    assert_eq!(written, 0, "a recovering feed completes, never -1");
    assert!(rest.is_empty(), "the one poll carried the whole small batch");

    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// The batch-bound law — ONE poll publishes at most ONE record batch
/// (the record route's own `RECORD_BATCH_ENTRIES`/`RECORD_BATCH_TARGET_BYTES`
/// pair, never a new number). The entries half pins a batch of small records;
/// the byte half pins a batch of large ones.
#[test]
fn one_poll_publishes_at_most_one_record_batch() {
    let handle = new_handle();
    let id = compile(handle, ".n");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    let mut out = vec![0u8; 65536];

    // 300 small records: the ENTRIES half of the bound (256) engages first.
    let mut stream = Vec::new();
    for k in 0..300 {
        stream.extend_from_slice(format!("{{\"n\":{k}}}\n").as_bytes());
    }
    assert!(feed_push(handle, feed, &stream) >= 0);

    let written = feed_poll(handle, feed, &mut out);
    let first = usize::try_from(written).unwrap();
    assert_eq!(
        count_lines(&out[..first]),
        usize::try_from(RECORD_BATCH_ENTRIES).unwrap(),
        "the entries half bounds one poll at exactly {RECORD_BATCH_ENTRIES} records"
    );
    let mut expected_first = String::new();
    for k in 0..RECORD_BATCH_ENTRIES {
        writeln!(expected_first, "{k}").unwrap();
    }
    assert_eq!(
        &out[..first],
        expected_first.as_bytes(),
        "the first batch is exactly records 0..{RECORD_BATCH_ENTRIES}, in order"
    );

    let (rest, written) = drain_feed(handle, feed);
    assert_eq!(written, 0, "the feed completes");
    let mut expected_rest = String::new();
    for k in RECORD_BATCH_ENTRIES..300 {
        writeln!(expected_rest, "{k}").unwrap();
    }
    assert_eq!(
        rest,
        expected_rest.as_bytes(),
        "the remaining records publish on the next poll"
    );
    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };

    // The BYTE half: records of 1100 payload bytes cross the 256 KiB target
    // after 238 records, so the first poll must hold exactly 239 of them.
    let handle = new_handle();
    let id = compile(handle, ".p | length");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    let mut stream = Vec::new();
    for _ in 0..300 {
        // `{"p":"…"}` with 1091 interior chars is exactly 1100 payload bytes
        // of one valid strict-JSON text.
        stream.extend_from_slice(b"{\"p\":\"");
        stream.extend_from_slice(&vec![b'x'; 1091]);
        stream.extend_from_slice(b"\"}\n");
    }
    assert!(feed_push(handle, feed, &stream) >= 0);
    let written = feed_poll(handle, feed, &mut out);
    let first = usize::try_from(written).unwrap();
    // 239 records × 1100 payload bytes = 262900 >= RECORD_BATCH_TARGET_BYTES
    // (262144), the first payload that crosses the target.
    assert_eq!(
        count_lines(&out[..first]),
        239,
        "the byte half bounds one poll at the first record past the 256 KiB target"
    );
    let mut drained = Vec::new();
    drained.extend_from_slice(&out[..first]);
    let (rest, written) = drain_feed(handle, feed);
    assert_eq!(written, 0);
    drained.extend_from_slice(&rest);
    assert_eq!(count_lines(&drained), 300, "all 300 records publish across polls");
    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// The id laws — a dead feed id is a DEFINED `-1` (never a dangling
/// dereference), for every entry point, with the stream saying so for the
/// entry points that record.
#[test]
fn a_dead_feed_id_is_a_defined_failure_for_every_entry_point() {
    let handle = new_handle();
    let id = compile(handle, ".");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    let mut out = vec![0u8; 4096];
    assert!(feed_push(handle, feed, b"1\n") >= 0);
    assert!(feed_poll(handle, feed, &mut out) >= 0);

    // SAFETY: `handle` is live; `feed` names a live feed.
    let rc = unsafe { jqf_feed_close(handle, feed) };
    assert_eq!(rc, 0, "closing a live feed succeeds");
    // A closed feed id is dead: push and poll are defined -1 with a recorded
    // setup record; close is a plain -1 (the `jqf_program_free` law).
    let written = feed_push(handle, feed, b"2\n");
    assert_eq!(written, -1, "push on a dead feed id is a defined -1");
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, -1, "poll on a dead feed id is a defined -1");
    // SAFETY: `handle` is live; the id is stale but the ABI validates it.
    let rc = unsafe { jqf_feed_close(handle, feed) };
    assert_eq!(rc, -1, "closing a dead feed id is a defined -1");
    let rc = unsafe { jqf_feed_finish(handle, feed) };
    assert_eq!(rc, -1, "finish on a dead feed id is a defined -1");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    // Poll clears then records; finish appends. Close records nothing.
    assert_eq!(count, 2, "dead-id poll plus finish each leave a setup record");
    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// A feed cannot be opened over a dead program — the open is a DEFINED
/// failure with the setup record; and a program freed under a LIVE feed makes
/// the feed's polls defined `-1` (the program id is re-validated every poll).
#[test]
fn a_feed_over_a_dead_program_is_a_defined_failure() {
    let handle = new_handle();
    let id = compile(handle, ".");
    // SAFETY: `handle` is live; `id` names a live program.
    let rc = unsafe { jqf_program_free(handle, id) };
    assert_eq!(rc, 0);

    let mut feed_id = 0u32;
    // SAFETY: `handle` is live; `id` is a stale program id, validated by the
    // ABI; `feed_id` is a live local slot.
    let rc = unsafe { jqf_feed_open(handle, id, JQF_FEED_PROFILE_STRICT, from_mut(&mut feed_id)) };
    assert_eq!(rc, -1, "opening a feed over a freed program must be a defined -1");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(count, 1, "the failed open must retain exactly its setup record");

    // The live-feed-under-freed-program shape: the poll re-validates the id.
    let live_program = compile(handle, ".");
    let feed = open_feed(handle, live_program, JQF_FEED_PROFILE_STRICT);
    // SAFETY: `handle` is live; `live_program` names a live program.
    let rc = unsafe { jqf_program_free(handle, live_program) };
    assert_eq!(rc, 0);
    let mut out = vec![0u8; 4096];
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, -1, "a poll on a feed whose program was freed is a defined -1");
    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

/// F8: a feed cannot outlive its engine — `jqf_free` drops every live feed
/// with the handle, releasing each feed's retained-input residency while the
/// account is still alive (the cannot-outlive-the-engine law, in the same
/// shape `freeing_the_engine_drops_its_live_programs_cleanly` pins for
/// programs).
#[test]
fn freeing_the_engine_drops_its_live_feeds_cleanly() {
    let handle = new_handle();
    let program = compile(handle, ".a");
    let feeds = [
        open_feed(handle, program, JQF_FEED_PROFILE_STRICT),
        open_feed(handle, program, JQF_FEED_PROFILE_RECOVERING),
        open_feed(handle, program, JQF_FEED_PROFILE_STRICT),
    ];
    for (index, feed) in feeds.iter().enumerate() {
        assert!(feed_push(handle, *feed, b"{\"a\":1}\n") >= 0);
        let mut out = vec![0u8; 4096];
        assert!(feed_poll(handle, *feed, &mut out) >= 0, "feed {index} polled");
    }
    // SAFETY: `handle` is live; every feed is still live — `jqf_free` must
    // drop them in the handle's Drop, releasing their retained input before
    // the account itself dies.
    unsafe { jqf_free(handle) };
}

/// The `snprintf` convention travels to the feed — a batch whose output
/// does not fit the caller's buffer is reported by its required size, and
/// re-polling with a bigger buffer re-delivers the SAME batch, never the
/// next one (a feed must never skip ahead because a buffer was too small).
#[test]
fn an_oversized_batch_reports_its_required_length_and_redelivers() {
    let handle = new_handle();
    let id = compile(handle, "[range(5000)]");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    assert!(feed_push(handle, feed, b"null\n") >= 0);

    let mut tiny = vec![0u8; 16];
    let written = feed_poll(handle, feed, &mut tiny);
    assert!(
        written > 0,
        "a large batch must report a positive required length, got {written}"
    );
    let required = usize::try_from(written).unwrap();
    assert!(required > tiny.len(), "the batch cannot fit 16 bytes");
    // The `snprintf` convention writes up to `out_cap` bytes even when the
    // required size is larger — the REQUIRED count is what signals
    // truncation, never the written prefix.
    assert_eq!(
        &tiny[..],
        b"[0,1,2,3,4,5,6,7",
        "a too-small buffer still receives the first out_cap bytes"
    );

    // Re-poll with the required size: the SAME batch is re-delivered.
    let mut big = vec![0u8; required];
    let written_again = feed_poll(handle, feed, &mut big);
    assert_eq!(written_again, written, "the re-poll returns the same batch");
    assert_eq!(
        &big[..16],
        b"[0,1,2,3,4,5,6,7",
        "the re-poll delivers the same batch prefix"
    );
    assert_eq!(
        &big[required - 7..],
        b",4999]\n",
        "the re-poll delivers the whole oversized batch"
    );

    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };
}

fn new_handle() -> *mut c_void {
    let mut handle: *mut c_void = ptr::null_mut();
    // SAFETY: `handle` is a live local slot for exactly one handle pointer,
    // which is `jqf_new`'s whole `# Safety` contract.
    let rc = unsafe { jqf_new(from_mut(&mut handle)) };
    assert_eq!(rc, 0, "jqf_new failed");
    handle
}

fn run(handle: *mut c_void, program: &str, input: &[u8], out: &mut [u8]) -> i64 {
    // SAFETY: `handle` is a live pointer from `new_handle`, `program` is
    // readable for `program.len()` bytes, and `input`/`out` are valid for
    // their declared lengths — exactly `jqf_run`'s `# Safety` contract.
    unsafe {
        jqf_run(
            handle,
            program.as_ptr(),
            program.len(),
            input.as_ptr(),
            input.len(),
            out.as_mut_ptr(),
            out.len(),
        )
    }
}

/// A program whose published output does not fit the caller's buffer
/// must be reported as such — the return value must be usable to detect
/// truncation, not just clamped to `out_cap` and handed back as if it were
/// an exact fit.
#[test]
fn oversized_output_reports_the_required_length_not_the_truncated_count() {
    let handle = new_handle();
    let mut tiny = vec![0u8; 8];
    let written = run(handle, "[range(200000)]", b"null", &mut tiny);

    assert!(written > 0, "expected a positive required length, got {written}");
    let required = usize::try_from(written).expect("a non-negative byte count");
    assert!(
        required > tiny.len(),
        "the program's real output cannot fit an 8-byte buffer; the ABI must \
         say so via a required length larger than the capacity offered, got {required}"
    );

    // The C convention this ABI follows (`snprintf`-shaped): re-call with a
    // buffer sized to the reported requirement, and the full output fits.
    let mut big = vec![0u8; required];
    let written_again = run(handle, "[range(200000)]", b"null", &mut big);
    assert_eq!(
        written_again, written,
        "the required length must be stable across an identical re-call"
    );

    // SAFETY: `handle` is a live pointer from `jqf_new`, not yet freed.
    unsafe { jqf_free(handle) };
}

/// The exact/over-capacity case must not be reported as truncated: the
/// required length is at most the buffer's own size, and the bytes actually
/// written are the real output.
#[test]
fn output_that_fits_is_not_reported_as_truncated() {
    let handle = new_handle();
    let mut out = vec![0u8; 65536];
    let written = run(handle, ".", b"1", &mut out);
    assert!(written >= 0, "run failed");
    let required = usize::try_from(written).unwrap();
    assert!(required <= out.len(), "a fitting output must not exceed out_cap");
    assert_eq!(&out[..required], b"1\n");

    // SAFETY: same as above.
    unsafe { jqf_free(handle) };
}

/// `jqf_diag_count`'s doc comment says it reads the LAST run's records.
/// A failing run followed by a clean run on the SAME handle must produce the
/// same diagnostic count a handle that only ever ran the clean program
/// would — proving the failing run's records did not survive into the next
/// run's count.
#[test]
fn diagnostics_do_not_accumulate_across_runs_on_a_reused_handle() {
    let reused = new_handle();
    let mut out = vec![0u8; 4096];

    // `jqf_run`'s numeric return now reports the -1 failure sentinel for this
    // uncaught runtime error (see
    // `uncaught_pipeline_failure_reports_the_failure_sentinel_not_a_required_length`
    // below) — the diagnostic stream is exercised here regardless, since this
    // test is about record accumulation, not the return value.
    let _failing = run(reused, "1/0", b"null", &mut out);
    // SAFETY: `reused` is a live handle.
    let after_fail = unsafe { jqf_diag_count(reused) };
    assert!(
        after_fail > 0,
        "a failing run must retain at least one diagnostic record"
    );

    let clean = run(reused, ".", b"1", &mut out);
    assert!(clean >= 0, "the second run on the reused handle must succeed");
    // SAFETY: `reused` is a live handle.
    let reused_count = unsafe { jqf_diag_count(reused) };

    let fresh = new_handle();
    let baseline = run(fresh, ".", b"1", &mut out);
    assert!(baseline >= 0);
    // SAFETY: `fresh` is a live handle.
    let fresh_count = unsafe { jqf_diag_count(fresh) };

    assert_eq!(
        reused_count, fresh_count,
        "a clean run's diagnostic count on a REUSED handle must match a handle \
         that only ever ran the clean program — the prior failing run's records \
         must not leak into it (reused={reused_count}, baseline={fresh_count})"
    );

    // SAFETY: both handles are live, each freed exactly once.
    unsafe {
        jqf_free(reused);
        jqf_free(fresh);
    }
}

/// The module doc promises `-1` "on
/// failure", but before this fix `run_inner` swallowed every PIPELINE
/// failure (an uncaught runtime error, as opposed to an early setup failure
/// like a bad program) into `Ok(())`, so `jqf_run` reported the required
/// length of whatever partial output existed — `0` for `1/0`, which a caller
/// cannot tell apart from a legitimate empty-output success. Every uncaught
/// error class must trip the `-1` sentinel.
#[test]
fn uncaught_pipeline_failure_reports_the_failure_sentinel_not_a_required_length() {
    let programs = [
        ("1/0", "divide by zero"),
        ("1 + \"a\"", "type mismatch (arithmetic)"),
        ("1 | .[]", "iterate over a non-iterable"),
        ("\"abc\"[1:\"x\"]", "non-integer slice index"),
        ("error(\"boom\")", "explicit error(...) call"),
    ];
    for (program, label) in programs {
        let handle = new_handle();
        let mut out = vec![0u8; 4096];
        let written = run(handle, program, b"null", &mut out);
        assert_eq!(
            written, -1,
            "{label} ({program:?}) must return the -1 failure sentinel, not a \
             required length (got {written}, which a caller would read as success)"
        );
        // SAFETY: handle is live.
        unsafe { jqf_free(handle) };
    }
}

/// Inverse of the above: a program that CATCHES its own error is not a
/// pipeline failure — `try`/`catch` absorbing the error must still report
/// success, with the caught value as the published output.
#[test]
fn an_error_caught_inside_the_program_still_reports_success() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, "try (1/0) catch \"x\"", b"null", &mut out);
    assert!(
        written >= 0,
        "an error caught by the program's own try/catch must not trip the \
         failure sentinel, got {written}"
    );
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"\"x\"\n");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The other inverse: a program with legitimately empty output (`empty`)
/// must report a required length of `0`, not the failure sentinel — `0` is
/// only ambiguous with failure if failure is ALSO allowed to report `0`,
/// which is exactly the bug the sentinel fix above closes.
#[test]
fn legitimately_empty_output_is_not_a_failure() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, "empty", b"null", &mut out);
    assert_eq!(
        written, 0,
        "empty output is a legitimate success (required length 0), not a failure"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// `jqf_run_sequence` shares the same sentinel contract as `jqf_run` (see
/// the module doc). Both entry points now drive the same input-sequence
/// route , so they must keep agreeing on what `-1` means.
#[test]
fn run_sequence_shares_the_same_failure_sentinel_contract() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run_sequence(handle, "1/0", b"1\n", &mut out);
    assert_eq!(
        written, -1,
        "an uncaught runtime error over a sequence input must also report the \
         -1 failure sentinel, got {written}"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// A setup failure — a program that never even compiles — is a DIFFERENT
/// case from an uncaught pipeline failure (the tests above): it happens
/// before `execute` ever runs, so `execute`'s own diagnostic recording
/// never fires. Without a record of its own, a caller reading the `-1`
/// sentinel off a freshly cleared, EMPTY diagnostic stream cannot tell a
/// real failure apart from a transport-level bug — the same hazard class as
/// the truncation and predicate bugs already fixed in this ABI. `jqf_run`
/// must both report `-1` AND retain a record explaining why.
#[test]
fn a_program_that_fails_to_compile_still_retains_a_diagnostic_record() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, "this is not valid jq (((", b"null", &mut out);
    assert_eq!(written, -1, "a program that cannot compile must not run");

    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(
        count, 1,
        "a compile failure must retain exactly one diagnostic record, not \
         leave the stream empty"
    );

    let diag = read_diag(handle, 0);
    assert_eq!(diag.code, codes::MACHINE_SETUP, "wrong code for a setup failure");
    assert_eq!(diag.severity, severity_tag(), "a setup failure is an error");
    assert_eq!(
        diag.caught,
        u32::MAX,
        "a setup failure happens before any `try` could run, so it is never caught"
    );

    // The payload is the worded parse rejection (the `Display` law),
    // never a `Parse(ParseRejection { … })` Debug struct literal. The message
    // names the byte span and the parser's own sentence; the Rust type names
    // stay out of the text a binding can surface.
    if let Some(payload) = diag.payload() {
        assert!(
            payload.contains("cannot parse program at bytes 5..7"),
            "the setup payload must word the parse rejection, got: {payload}"
        );
        assert!(
            !payload.contains("ParseRejection"),
            "the setup payload must not leak Debug struct text, got: {payload}"
        );
    }

    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

// --- the compiled-program surface ------------------------------------------
//
// The program handle is a handle-LOCAL `u32` table id, deliberately not a
// pointer: every misuse — a freed id, a double free, an id past the table —
// is a DEFINED `-1`, never undefined behavior. The tests below pin the four
// lifetime hazards the module doc promises are defined: the normal
// free-after-use lifecycle, double-free, use-after-free, and programs
// outliving nothing — they die WITH their engine, releasing their ledger
// residencies while the account is still alive.

use jqf_sdk_ffi::{jqf_compile, jqf_program_free, jqf_run_compiled, jqf_run_sequence_compiled};

fn compile(handle: *mut c_void, program: &str) -> u32 {
    let mut id = 0u32;
    // SAFETY: `handle` is a live pointer from `new_handle`, `program` is
    // readable for `program.len()` bytes, and `id` is a live local slot for
    // one `u32` — exactly `jqf_compile`'s `# Safety` contract.
    let rc = unsafe { jqf_compile(handle, program.as_ptr(), program.len(), from_mut(&mut id)) };
    assert_eq!(rc, 0, "jqf_compile failed for {program:?}");
    id
}

fn run_compiled(handle: *mut c_void, id: u32, input: &[u8], out: &mut [u8]) -> i64 {
    // SAFETY: `handle` is live, `id` is validated by the ABI (any `u32` is
    // legal to pass), and `input`/`out` are valid for their lengths.
    unsafe { jqf_run_compiled(handle, id, input.as_ptr(), input.len(), out.as_mut_ptr(), out.len()) }
}

fn run_sequence(handle: *mut c_void, program: &str, input: &[u8], out: &mut [u8]) -> i64 {
    // SAFETY: `handle` is live, `program` is readable for its length, and
    // `input`/`out` are valid for their lengths.
    unsafe {
        jqf_run_sequence(
            handle,
            program.as_ptr(),
            program.len(),
            input.as_ptr(),
            input.len(),
            out.as_mut_ptr(),
            out.len(),
        )
    }
}

fn run_sequence_compiled(handle: *mut c_void, id: u32, input: &[u8], out: &mut [u8]) -> i64 {
    // SAFETY: `handle` is live, `id` is validated by the ABI, and
    // `input`/`out` are valid for their lengths.
    unsafe { jqf_run_sequence_compiled(handle, id, input.as_ptr(), input.len(), out.as_mut_ptr(), out.len()) }
}

/// C1: the compiled path is byte-identical to the per-call path for the same
/// program and input — output bytes, diagnostic count, and the success
/// sentinel all agree. This is the "additive surface, not a rewrite" law.
#[test]
fn compiled_runs_match_per_call_runs_byte_for_byte() {
    let programs = [".a", ".a | tostring", "[.[]] | length", "empty", "1/0"];
    for program in programs {
        let handle = new_handle();
        let id = compile(handle, program);
        let mut out = vec![0u8; 65536];

        let per_call = run(handle, program, b"{\"a\":1}", &mut out);
        let compiled = run_compiled(handle, id, b"{\"a\":1}", &mut out);

        assert_eq!(
            compiled, per_call,
            "{program:?}: the compiled run's byte count must equal the per-call \
             run's (compiled={compiled}, per-call={per_call})"
        );
        let expected = match program {
            ".a" | "[.[]] | length" => Some(b"1\n".as_slice()),
            ".a | tostring" => Some(b"\"1\"\n".as_slice()),
            "empty" => Some(b"".as_slice()),
            // `1/0` is the -1 sentinel case; the count equality above is the
            // whole assertion for it.
            _ => None,
        };
        if let Some(expected) = expected {
            let required = usize::try_from(per_call).unwrap();
            assert_eq!(
                &out[..required],
                expected,
                "{program:?}: byte-identical output for the same program/input"
            );
        }
        // SAFETY: `handle` is live, freed exactly once.
        unsafe { jqf_free(handle) };
    }
}

/// C2: the compiled path retains the same diagnostic stream contract as the
/// per-call path — a failing compiled run retains its records, and the next
/// operation on the same handle clears them.
#[test]
fn compiled_runs_share_the_diagnostic_stream_contract() {
    let handle = new_handle();
    let id = compile(handle, "1/0");
    let mut out = vec![0u8; 4096];
    let written = run_compiled(handle, id, b"null", &mut out);
    assert_eq!(written, -1, "an uncaught error must report the -1 sentinel");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert!(count > 0, "a failing compiled run must retain diagnostic records");

    // The next operation clears the stream: a clean compiled run retains the
    // same count a fresh handle's clean run does.
    let clean = compile(handle, ".");
    let written = run_compiled(handle, clean, b"1", &mut out);
    assert!(written >= 0);
    // SAFETY: `handle` is live.
    let after_clean = unsafe { jqf_diag_count(handle) };
    let fresh = new_handle();
    let fresh_program = compile(fresh, ".");
    let _ = run_compiled(fresh, fresh_program, b"1", &mut out);
    // SAFETY: `fresh` is live.
    let baseline = unsafe { jqf_diag_count(fresh) };
    assert_eq!(
        after_clean, baseline,
        "a clean compiled run must not retain a prior failing run's records"
    );

    // SAFETY: both handles are live, each freed exactly once.
    unsafe {
        jqf_free(handle);
        jqf_free(fresh);
    }
}

/// A program nested past the default-thread abort (and past the documented
/// `10_000` ceiling) must return a nesting-depth error, never abort. The
/// handle's request thread is sized like the CLI (`JQF_REQUEST_STACK_BYTES`,
/// default 256 MiB).
#[test]
fn deep_nesting_returns_a_depth_error_instead_of_aborting() {
    let handle = new_handle();
    let deep = format!("{} .{}", "(".repeat(2000), ")".repeat(2000));
    let mut id = 0u32;
    // SAFETY: `handle` is live, `deep` is readable for its length, `id` is a
    // live local slot — exactly `jqf_compile`'s `# Safety` contract.
    let rc = unsafe { jqf_compile(handle, deep.as_ptr(), deep.len(), from_mut(&mut id)) };
    assert_eq!(
        rc, 0,
        "2000 groups must compile on the request thread instead of aborting"
    );

    let past = format!("{} .{}", "(".repeat(10_000), ")".repeat(10_000));
    let rc = unsafe { jqf_compile(handle, past.as_ptr(), past.len(), from_mut(&mut id)) };
    assert_eq!(rc, -1, "10_000 groups must refuse at the nesting ceiling");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(count, 1, "the refusal retains exactly one setup record");
    let diag = read_diag(handle, 0);
    let payload = diag.payload().unwrap_or_default();
    assert!(
        payload.contains("nesting depth limit exceeded"),
        "expected the nesting-depth refusal, got {payload:?}"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// C3: `jqf_compile` clears the stream and retains EXACTLY ONE setup-failure
/// record when the program cannot compile — the same law `jqf_run` keeps for
/// its own setup failures, so the `-1` sentinel off a compile is never
/// ambiguous.
#[test]
fn a_compile_failure_retains_exactly_one_setup_record() {
    let handle = new_handle();
    let program = "this is not valid jq (((";
    let mut id = 0u32;
    // SAFETY: `handle` is live, `program` is readable for its length, and
    // `id` is a live local slot — exactly `jqf_compile`'s `# Safety`
    // contract.
    let rc = unsafe { jqf_compile(handle, program.as_ptr(), program.len(), from_mut(&mut id)) };
    assert_eq!(rc, -1, "a program that cannot compile must not compile");

    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(
        count, 1,
        "a failed compile must retain exactly one diagnostic record, not \
         leave the stream empty or accumulate prior records"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// C4: a successful compile does not leave a runnable program behind on a
/// stream it cleared — and the stream after a failed compile is exactly the
/// setup record, never a mix with a prior run's records.
#[test]
fn a_compile_failure_does_not_mix_prior_runs_records_into_the_stream() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    // A run that retains records...
    let _ = run(handle, "1/0", b"null", &mut out);
    // ...followed by a failed compile must show exactly the setup record.
    let program = "this is not valid jq (((";
    let mut id = 0u32;
    // SAFETY: see `a_compile_failure_retains_exactly_one_setup_record`.
    let rc = unsafe { jqf_compile(handle, program.as_ptr(), program.len(), from_mut(&mut id)) };
    assert_eq!(rc, -1);
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(
        count, 1,
        "a failed compile must clear prior runs' records and retain exactly \
         its own setup record (got {count})"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// C5: free-after-use — the normal lifecycle. A program compiled, run, and
/// freed releases cleanly, and a second program compiled after the free gets
/// its own distinct id (the table never reuses slots).
#[test]
fn free_after_use_releases_the_program_and_never_reuses_its_id() {
    let handle = new_handle();
    let first = compile(handle, ".a");
    let mut out = vec![0u8; 4096];
    assert!(run_compiled(handle, first, b"{\"a\":1}", &mut out) >= 0);
    // SAFETY: `handle` is live; `first` names a live program.
    let rc = unsafe { jqf_program_free(handle, first) };
    assert_eq!(rc, 0, "freeing a live program must succeed");

    // A later compile must not reuse the freed id: the id is a table index
    // that stays dead forever, so no stale id can ever alias a new program.
    let second = compile(handle, ".b");
    assert_ne!(
        second, first,
        "the program table must never reuse a freed slot (stale ids must \
         stay dead)"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// C6: double-free is a DEFINED error — the second free of the same id
/// returns -1 and frees nothing — never undefined behavior. (A pointer-based
/// handle could not make this promise; the table id is why this ABI can.)
#[test]
fn double_free_is_a_defined_error_not_undefined_behavior() {
    let handle = new_handle();
    let id = compile(handle, ".");
    // SAFETY: `handle` is live; `id` names a live program.
    let first = unsafe { jqf_program_free(handle, id) };
    assert_eq!(first, 0);
    // SAFETY: same handle; the id is stale but the ABI validates it.
    let second = unsafe { jqf_program_free(handle, id) };
    assert_eq!(
        second, -1,
        "freeing an already-freed id must report -1, never touch freed memory"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// C7: use-after-free is a DEFINED failure — running a freed id reports the
/// `-1` sentinel AND retains a `MACHINE_SETUP` record naming the dead id,
/// exactly the setup-failure law: a caller reading the sentinel off the
/// stream can tell the cause.
#[test]
fn use_after_free_is_a_defined_failure_with_a_recorded_cause() {
    let handle = new_handle();
    let id = compile(handle, ".");
    // SAFETY: `handle` is live; `id` names a live program.
    let rc = unsafe { jqf_program_free(handle, id) };
    assert_eq!(rc, 0);

    let mut out = vec![0u8; 4096];
    let written = run_compiled(handle, id, b"1", &mut out);
    assert_eq!(
        written, -1,
        "running a freed program id must report the -1 failure sentinel, not \
         dereference freed memory"
    );
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(
        count, 1,
        "a use-after-free must retain exactly one setup record explaining it"
    );
    let diag = read_diag(handle, 0);
    assert_eq!(diag.code, codes::MACHINE_SETUP, "wrong code for a dead-id failure");
    assert_eq!(
        diag.severity,
        severity_tag(),
        "a dead-id failure is an error, not a transport whisper"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// C8: programs cannot outlive their engine — `jqf_free` drops every live
/// program with the handle, releasing each compiled arena's ledger residency
/// while the account is still alive. This is the defined outcome of the
/// "program outliving its engine" scenario: there is no such state; the
/// engine free is the program free, and it must be clean (a bug here would
/// be a crash or a leaked-residency double-release, which is exactly what
/// the Residency/account interplay can do wrong).
#[test]
fn freeing_the_engine_drops_its_live_programs_cleanly() {
    let handle = new_handle();
    let ids = [compile(handle, "."), compile(handle, ".a"), compile(handle, "[.[]]")];
    let mut out = vec![0u8; 4096];
    for id in ids {
        assert!(run_compiled(handle, id, b"{\"a\":1}", &mut out) >= 0);
    }
    // SAFETY: `handle` is live; every program is still live — `jqf_free`
    // must drop them in the handle's Drop, releasing the compiled arenas
    // and their ledger residencies before the account itself dies.
    unsafe { jqf_free(handle) };
}

/// C9: an id from a DIFFERENT handle is not a live program on this one —
/// the table is handle-local, so cross-handle ids are defined `-1`, never a
/// borrow into another handle's memory.
#[test]
fn a_program_id_is_handle_local_and_foreign_ids_are_defined_failures() {
    let first = new_handle();
    let second = new_handle();
    let id = compile(first, ".");
    let mut out = vec![0u8; 4096];
    let written = run_compiled(second, id, b"1", &mut out);
    assert_eq!(written, -1, "an id compiled on another handle must not run on this one");
    // SAFETY: `second` is live — the foreign-id failure must have retained a
    // setup record, not garbage.
    let count = unsafe { jqf_diag_count(second) };
    assert_eq!(count, 1, "the foreign-id failure must be recorded");
    // SAFETY: both handles are live, each freed exactly once.
    unsafe {
        jqf_free(first);
        jqf_free(second);
    }
}

/// C10: the sequence route's compiled twin keeps the same contract as
/// `jqf_run_sequence` — multi-value output, per-value errors through the
/// retained error channel, and the shared failure sentinel.
#[test]
fn run_sequence_compiled_matches_run_sequence_byte_for_byte() {
    let handle = new_handle();
    let id = compile(handle, ".");
    let mut out = vec![0u8; 65536];

    let per_call = run_sequence(handle, ".", b"1\n2\n", &mut out);
    let per_call_bytes = if per_call >= 0 {
        out[..usize::try_from(per_call).unwrap()].to_vec()
    } else {
        Vec::new()
    };
    let compiled = run_sequence_compiled(handle, id, b"1\n2\n", &mut out);

    assert_eq!(
        compiled, per_call,
        "the compiled sequence run must report the same byte count (compiled={compiled}, \
         per-call={per_call})"
    );
    if per_call >= 0 {
        let required = usize::try_from(compiled).unwrap();
        assert_eq!(&out[..required], per_call_bytes, "byte-identical sequence output");
    }

    let boom = compile(handle, r#"error("boom")"#);
    let mut err_out = vec![0u8; 64];
    let per_call_err = run_sequence(handle, r#"error("boom")"#, b"1\n2\n", &mut err_out);
    assert_eq!(per_call_err, -1);
    let per_call_n = unsafe { jqf_run_errors_count(handle) };
    let compiled_err = run_sequence_compiled(handle, boom, b"1\n2\n", &mut err_out);
    assert_eq!(compiled_err, -1);
    let compiled_n = unsafe { jqf_run_errors_count(handle) };
    assert_eq!(compiled_n, per_call_n);
    assert!(per_call_n >= 1, "error/1 retains per-value errors");

    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

// --- the correct core ------------------------------------------------
//
// The 083 scope ruling: bindings, input source, environment, encode options,
// and the input-sequence route. Every test below pins one acceptance row —
// the exact deltas that used to answer silently wrong.

use jqf_sdk_ffi::{JQF_ABI_VERSION, jqf_compile_args, jqf_run_error_get, jqf_run_errors_count};

/// The first silent-wrong-answer row: `[inputs]` under the FFI must answer as
/// it does under the CLI — the shared input cursor is installed by the
/// input-sequence drive, so the previous `[]` is gone. The reference's
/// semantics: the first value is the program's input (dot), so `[inputs]`
/// collects the REST.
#[test]
fn inputs_answers_like_the_cli() {
    let handle = new_handle();
    let mut out = vec![0u8; 65536];
    let written = run(handle, "[inputs]", b"1\n2\n3\n", &mut out);
    assert!(written >= 0, "`[inputs]` must not fail");
    let required = usize::try_from(written).unwrap();
    assert_eq!(
        &out[..required],
        b"[2,3]\n",
        "`[inputs]` must collect the values after the first, like the reference"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// `input` reads the NEXT value from the shared cursor (the reference's
/// input-family semantics), instead of the previous `"break"` raise.
#[test]
fn input_reads_the_next_value() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, "input", b"1\n2\n", &mut out);
    assert!(written >= 0, "`input` must not raise `break`");
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"2\n");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// A3 (the plan's second silent-wrong-answer row): `$ENV`/`env` must answer
/// the host's variables — never the pre-083 empty object. A cargo-test
/// process always has variables (PATH, PWD, CARGO_*), so `length > 0` is a
/// real assertion, not a tautology.
#[test]
fn env_is_the_host_snapshot_not_an_empty_object() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, "($ENV | length) > 0", b"null", &mut out);
    assert!(written >= 0, "`$ENV` must resolve");
    let required = usize::try_from(written).unwrap();
    assert_eq!(
        &out[..required],
        b"true\n",
        "the FFI handle must install the host environment snapshot"
    );
    // The `env` builtin reads the same snapshot.
    let written = run(handle, "env | type", b"null", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"\"object\"\n");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// A4: host data reaches a program through the binding API — `$name` is a
/// compile-time constant parsed from JSON, never spliced into the source —
/// and `$ARGS` resolves to the CLI's shape.
#[test]
fn bindings_reach_the_program_and_args_resolves() {
    let handle = new_handle();
    let mut id = 0u32;
    let program = b"$x + 1";
    // A single binding `x` = the JSON value 41.
    let names = [c"x".as_ptr().cast()];
    let values = [b"41".as_ptr()];
    let lengths = [2usize];
    // SAFETY: `handle` is live, `program` is readable for its length, the
    // parallel arrays have one entry each (a live C string name and a
    // readable value pair), and `id` is a live local slot.
    let rc = unsafe {
        jqf_compile_args(
            handle,
            program.as_ptr(),
            program.len(),
            1,
            names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            from_mut(&mut id),
        )
    };
    assert_eq!(rc, 0, "jqf_compile_args failed");
    let mut out = vec![0u8; 4096];
    let written = run_compiled(handle, id, b"null", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"42\n");

    // `$ARGS` is bound on every compile: named-only bindings answer the reference's
    // `{"positional": [], "named": {"x": 41}}`.
    let mut id = 0u32;
    let program = b"$ARGS";
    let rc = unsafe {
        jqf_compile_args(
            handle,
            program.as_ptr(),
            program.len(),
            1,
            names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            from_mut(&mut id),
        )
    };
    assert_eq!(rc, 0);
    let written = run_compiled(handle, id, b"null", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(
        &out[..required],
        b"{\"positional\":[],\"named\":{\"x\":41}}\n",
        "a binding answers the CLI's $ARGS shape"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The reference's duplicate-name law: when `jqf_compile_args` receives the
/// same binding name twice, the FIRST value wins and every later duplicate is
/// dropped — for the `$name` constant AND for `$ARGS.named` (the binding
/// API's compile-time constants, never a last-write-wins splice into the
/// source).
#[test]
fn compile_args_duplicate_names_first_wins() {
    let handle = new_handle();
    let mut id = 0u32;
    let program = b"$x";
    // Two bindings of the same name: `x` = 41 (first) and `x` = 1 (dropped).
    let names = [c"x".as_ptr().cast(), c"x".as_ptr().cast()];
    let values = [b"41".as_ptr(), b"1".as_ptr()];
    let lengths = [2usize, 1usize];
    // SAFETY: `handle` is live, `program` is readable for its length, the
    // parallel arrays have two entries each (live C string names and
    // readable value pairs), and `id` is a live local slot.
    let rc = unsafe {
        jqf_compile_args(
            handle,
            program.as_ptr(),
            program.len(),
            2,
            names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            from_mut(&mut id),
        )
    };
    assert_eq!(rc, 0, "jqf_compile_args failed");
    let mut out = vec![0u8; 4096];
    let written = run_compiled(handle, id, b"null", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(
        &out[..required],
        b"41\n",
        "the FIRST duplicate binding must win, not the last"
    );

    // `$ARGS.named` agrees: the dropped duplicate never reaches it.
    let program = b"$ARGS";
    let mut id = 0u32;
    // SAFETY: same contracts; `program` is readable for its length.
    let rc = unsafe {
        jqf_compile_args(
            handle,
            program.as_ptr(),
            program.len(),
            2,
            names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            from_mut(&mut id),
        )
    };
    assert_eq!(rc, 0);
    let written = run_compiled(handle, id, b"null", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(
        &out[..required],
        b"{\"positional\":[],\"named\":{\"x\":41}}\n",
        "the first-wins law holds for $ARGS.named too"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

#[test]
fn a_null_binding_name_is_a_defined_setup_failure() {
    let handle = new_handle();
    let mut id = 0u32;
    let names = [std::ptr::null()];
    let values = [b"1".as_ptr()];
    let lengths = [1usize];
    let rc = unsafe {
        jqf_compile_args(
            handle,
            b"$x".as_ptr(),
            2,
            1,
            names.as_ptr(),
            values.as_ptr(),
            lengths.as_ptr(),
            from_mut(&mut id),
        )
    };
    assert_eq!(rc, -1);
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(count, 1, "NULL name retains exactly one setup record");
    unsafe { jqf_free(handle) };
}

#[test]
fn null_binding_arrays_are_a_defined_setup_failure() {
    let handle = new_handle();
    let mut id = 0u32;
    let rc = unsafe {
        jqf_compile_args(
            handle,
            b".".as_ptr(),
            1,
            1,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            from_mut(&mut id),
        )
    };
    assert_eq!(rc, -1);
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(count, 1, "NULL arrays retain exactly one setup record");
    unsafe { jqf_free(handle) };
}

/// A5: a program containing an embedded NUL is REJECTED with a typed setup
/// error, never silently truncated at its first NUL (the injection
/// path the binding API exists to close).
#[test]
fn an_embedded_nul_program_is_a_typed_error_not_a_truncated_compile() {
    let handle = new_handle();
    let mut id = 0u32;
    // `1\0| .` — the prefix `1` alone is a VALID program, so a NUL-truncating
    // ABI would compile and run it silently.
    let program = b"1\0| .";
    // SAFETY: `handle` is live, `program` is readable for its length, `id`
    // is a live local slot.
    let rc = unsafe { jqf_compile(handle, program.as_ptr(), program.len(), from_mut(&mut id)) };
    assert_eq!(rc, -1, "an embedded NUL must be rejected, not truncated");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert_eq!(count, 1);
    let diag = read_diag(handle, 0);
    assert_eq!(diag.code, codes::MACHINE_SETUP);
    let payload = diag.payload().unwrap_or_default();
    assert!(
        payload.contains("NUL"),
        "the setup payload must name the NUL rejection, got: {payload}"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The run entry points accept MULTI-VALUE input (the reference's default
/// stdin model) — the previous single-document rejection is gone.
#[test]
fn run_accepts_adjacent_values_like_the_cli() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, ".", b"1 2 3", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"1\n2\n3\n");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The encode options reach the encoder — raw strings
/// write a ROOT string verbatim, and an indent pretty-prints.
#[test]
fn encode_options_reach_the_encoder() {
    let handle = new_handle_limited(
        std::ptr::null(),
        &jqf_sdk_ffi::JqfEncodeOptions {
            indent: -1,
            raw_strings: 1,
            sort_keys: 0,
            ascii_output: 0,
            raw_output_nul: 0,
        },
    );
    let mut out = vec![0u8; 4096];
    let written = run(handle, ".", b"\"hi\"", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"hi\n", "-r writes a root string raw");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };

    let handle = new_handle_limited(
        std::ptr::null(),
        &jqf_sdk_ffi::JqfEncodeOptions {
            indent: 2,
            raw_strings: 0,
            sort_keys: 0,
            ascii_output: 0,
            raw_output_nul: 0,
        },
    );
    let mut out = vec![0u8; 4096];
    let written = run(handle, ".", b"{\"a\":1}", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"{\n  \"a\": 1\n}\n", "indent 2 pretty-prints");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

fn new_handle_limited(
    limits: *const jqf_sdk_ffi::JqfLimits,
    encode: *const jqf_sdk_ffi::JqfEncodeOptions,
) -> *mut c_void {
    let mut handle: *mut c_void = ptr::null_mut();
    // SAFETY: `limits`/`encode` are initialized (or null for defaults) and
    // `handle` is a live local slot.
    let rc = unsafe { jqf_new_limited(limits, encode, from_mut(&mut handle)) };
    assert_eq!(rc, 0, "jqf_new_limited failed");
    handle
}

// --- 084: the diagnostic channel ------------------------------------------

/// `halt_error(n)` delivers BOTH the status and the message through the ABI,
/// and is distinguishable from a bare `halt`. The reference's
/// `halt_error` law: the current input is the message and the argument is
/// the status (`halt_error(5)` over input `"boom"` halts with status 5 and
/// message `boom`).
#[test]
fn halt_error_delivers_status_and_message_and_is_distinguishable_from_halt() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, "halt_error(5)", b"\"boom\"", &mut out);
    assert_eq!(written, -1, "halt_error is a terminal failure");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let terminal = read_diag(handle, (count - 1) as u32);
    assert_eq!(terminal.code, codes::RAISE_HALT, "halt records RAISE_HALT");
    assert_eq!(
        terminal.halt_status, 5,
        "halt_error's status must travel the halt_status channel"
    );
    assert_eq!(
        terminal.payload().as_deref(),
        Some("boom"),
        "halt_error's message must travel the record payload"
    );

    // A bare `halt` is distinguishable: status 0, no message.
    let written = run(handle, "halt", b"null", &mut out);
    assert_eq!(written, -1);
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let terminal = read_diag(handle, (count - 1) as u32);
    assert_eq!(terminal.code, codes::RAISE_HALT);
    assert_eq!(terminal.halt_status, 0, "bare halt has status 0");
    assert_eq!(terminal.payload(), None, "bare halt carries no message");

    // A NON-halt failure reads halt_status -1.
    let written = run(handle, "1/0", b"null", &mut out);
    assert_eq!(written, -1);
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let terminal = read_diag(handle, (count - 1) as u32);
    assert_eq!(
        terminal.halt_status, -1,
        "every non-halt record must read halt_status -1"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// A raised non-string value arrives as its compact JSON payload — a
/// `RAISE_PROGRAM` record is never payload-less.
#[test]
fn a_raised_object_carries_its_payload() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, "error({\"code\":42})", b"null", &mut out);
    assert_eq!(written, -1);
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let terminal = read_diag(handle, (count - 1) as u32);
    assert_eq!(terminal.code, codes::RAISE_PROGRAM);
    assert_eq!(
        terminal.payload().as_deref(),
        Some("{\"code\":42}"),
        "the raised object must be recoverable from the payload"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// N per-value errors from one sequence run are all retrievable through
/// the length-carrying getters, and an error whose TEXT contains a NUL byte
/// arrives intact — a NUL-joined text channel could never carry it.
#[test]
fn sequence_errors_are_all_retrievable_and_nul_bytes_survive() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    // Values 1 and 2 each raise `error("a\0b")`; value 3 publishes.
    let program = "if . <= 2 then error(\"a\\u0000b\") else . end";
    let written = run_sequence(handle, program, b"1\n2\n3\n", &mut out);
    // The LAST value succeeds, so the run completes with its output.
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"3\n");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_run_errors_count(handle) };
    assert_eq!(count, 2, "both errors must be retained");
    for index in 0..count {
        // The `(NULL, 0)` sizing probe answers the required length.
        let required = unsafe { jqf_run_error_get(handle, index, ptr::null_mut(), 0) };
        assert_eq!(required, 3, "the error text `a\\0b` is three bytes");
        let mut buf = [0u8; 8];
        let got = unsafe { jqf_run_error_get(handle, index, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(got, required);
        assert_eq!(
            &buf[..3],
            b"a\0b",
            "the NUL byte inside the error text must survive the channel"
        );
    }
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The locators are readable per diagnostic — a typed index
/// error carries its step, and a setup failure reads the sentinels.
#[test]
fn the_diagnostic_locators_are_readable() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, ".a", b"1", &mut out);
    assert_eq!(written, -1, "indexing a number is a typed error");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let terminal = read_diag(handle, (count - 1) as u32);
    assert_eq!(terminal.code, codes::RAISE_INDEX);
    assert_ne!(
        terminal.step_index,
        u32::MAX,
        "a typed error must carry its failing step"
    );

    // A setup failure reads the sentinels (no step, no input, no offset).
    let written = run(handle, "this is not valid jq (((", b"null", &mut out);
    assert_eq!(written, -1);
    let setup = read_diag(handle, 0);
    assert_eq!(setup.step_index, u32::MAX);
    assert_eq!(setup.input_ordinal, u64::MAX);
    assert_eq!(setup.byte_offset, u64::MAX);
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

// --- 085: safety and the ABI contract -------------------------------------

use std::sync::atomic::{AtomicU32, Ordering};

/// A runaway program under a deadline stops with `MACHINE_DEADLINE` instead
/// of hanging.
#[test]
fn a_deadline_stops_a_runaway_program_with_machine_deadline() {
    let limits = jqf_sdk_ffi::JqfLimits {
        max_output_bytes: u64::MAX,
        max_memory_bytes: u64::MAX,
        max_spill_bytes: u64::MAX,
        max_nesting_depth: u32::MAX,
        // `repeat(.)` publishes forever; 150 ms of it is a bounded, generous
        // window on any machine — if the deadline never fires, the test
        // would run away with it, which is exactly what the ABI must stop.
        deadline_ms: 150,
        control_callback: None,
        control_context: ptr::null_mut(),
    };
    let handle = new_handle_limited(ptr::from_ref(&limits), std::ptr::null());
    let mut out = vec![0u8; 65536];
    let written = run(handle, "repeat(.)", b"1", &mut out);
    assert_eq!(written, -1, "a runaway program under a deadline must fail, not hang");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let terminal = read_diag(handle, (count - 1) as u32);
    assert_eq!(
        terminal.code,
        codes::MACHINE_DEADLINE,
        "the deadline must retain the machine deadline record"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The host control callback can CANCEL a request — the `MACHINE_CANCELLED`
/// code finally has a reachable producer.
#[test]
fn a_cancel_callback_stops_a_runaway_program_with_machine_cancelled() {
    // The callback returns continue twice (the construction check and the
    // first run checkpoint), then cancels.
    static CALLS: AtomicU32 = AtomicU32::new(0);
    unsafe extern "C" fn cancel_on_third_call(_context: *mut c_void) -> c_int {
        if CALLS.fetch_add(1, Ordering::SeqCst) >= 2 {
            jqf_sdk_ffi::JQF_CONTROL_CANCELLED
        } else {
            jqf_sdk_ffi::JQF_CONTROL_CONTINUE
        }
    }
    let limits = jqf_sdk_ffi::JqfLimits {
        max_output_bytes: u64::MAX,
        max_memory_bytes: u64::MAX,
        max_spill_bytes: u64::MAX,
        max_nesting_depth: u32::MAX,
        deadline_ms: 0,
        control_callback: Some(cancel_on_third_call),
        control_context: ptr::null_mut(),
    };
    let handle = new_handle_limited(ptr::from_ref(&limits), std::ptr::null());
    let mut out = vec![0u8; 65536];
    let written = run(handle, "repeat(.)", b"1", &mut out);
    assert_eq!(written, -1, "a cancelled run must fail, not hang");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let terminal = read_diag(handle, (count - 1) as u32);
    assert_eq!(
        terminal.code,
        codes::MACHINE_CANCELLED,
        "a host cancel must retain the machine cancelled record"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The depth ceiling is a DEFINED failure — a document past the handle's
/// nesting depth refuses with a record, never a stack smash.
#[test]
fn a_depth_ceiling_refuses_a_deep_document() {
    let limits = jqf_sdk_ffi::JqfLimits {
        max_output_bytes: u64::MAX,
        max_memory_bytes: u64::MAX,
        max_spill_bytes: u64::MAX,
        max_nesting_depth: 4,
        deadline_ms: 0,
        control_callback: None,
        control_context: ptr::null_mut(),
    };
    let handle = new_handle_limited(ptr::from_ref(&limits), std::ptr::null());
    let mut out = vec![0u8; 4096];
    // Depth 5 nests past the 4-frame ceiling.
    let written = run(handle, ".", b"[[[[[1]]]]]", &mut out);
    assert_eq!(written, -1, "a document past the depth ceiling must refuse");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert!(count >= 1, "the refusal must retain a record");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The documented `(NULL, 0)` sizing probe is DEFINED — `jqf_feed_poll`
/// with a NULL buffer returns the required length, a `(NULL, 0)` run input
/// is the empty input, and a NULL output buffer with zero capacity is
/// never dereferenced.
#[test]
fn null_sizing_probes_are_defined_not_undefined_behavior() {
    // The feed probe: push one record, poll with (NULL, 0) → required length.
    let handle = new_handle();
    let id = compile(handle, ".a");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    assert!(feed_push(handle, feed, b"{\"a\":123}\n") >= 0);
    // SAFETY: `(NULL, 0)` is the documented probe; the poll must return the
    // required length without writing.
    let required = unsafe { jqf_feed_poll(handle, feed, ptr::null_mut(), 0) };
    assert_eq!(
        required, 4,
        "the NULL sizing probe must answer the required length (123\\n)"
    );
    // SAFETY: `handle` is live, freed exactly once.
    unsafe { jqf_free(handle) };

    // A `(NULL, 0)` run input is the empty input: the reference's answer for no values.
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    // SAFETY: `(NULL, 0)` input is the documented empty input.
    let written = unsafe { jqf_run(handle, ".".as_ptr(), 1, ptr::null(), 0, out.as_mut_ptr(), out.len()) };
    assert_eq!(written, 0, "empty input publishes nothing");
    // A NULL output buffer with zero capacity returns the required length.
    let written = run(handle, "[range(10)]", b"null", &mut out);
    assert!(written > 0);
    // SAFETY: `(NULL, 0)` output is the documented sizing probe.
    let probe = unsafe {
        jqf_run(
            handle,
            "[range(10)]".as_ptr(),
            11,
            b"null".as_ptr(),
            4,
            ptr::null_mut(),
            0,
        )
    };
    assert_eq!(
        probe, written,
        "the NULL output probe must return the same required length"
    );
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// An out-of-range `index` to `jqf_diag_get` is a DEFINED `-1` (the same
/// sentinel the length-carrying getters use), never a read past the retained
/// records or a garbage answer.
#[test]
fn diag_get_out_of_range_is_a_defined_minus_one() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    // Retain at least one record so the out-of-range arm is provably a
    // bounds check, not "the vector happens to be empty".
    let _ = run(handle, "1/0", b"null", &mut out);
    // SAFETY: `handle` is live, `index` is validated by the ABI, and every
    // out-parameter is a valid, aligned, writable local slot of its declared
    // type (the out-of-range arm returns -1 before writing).
    let mut diag = Diag {
        code: 0,
        revision: 0,
        class: 0 as c_char,
        severity: 0 as c_char,
        catchable: 0,
        caught: 0,
        step_index: 0,
        input_ordinal: 0,
        byte_offset: 0,
        halt_status: 0,
        kind: ptr::null_mut(),
        operand: ptr::null_mut(),
        payload: ptr::null_mut(),
    };
    let rc = unsafe {
        jqf_diag_get(
            handle,
            u32::MAX,
            from_mut(&mut diag.code),
            from_mut(&mut diag.revision),
            from_mut(&mut diag.class),
            from_mut(&mut diag.severity),
            from_mut(&mut diag.catchable),
            from_mut(&mut diag.caught),
            from_mut(&mut diag.step_index),
            from_mut(&mut diag.input_ordinal),
            from_mut(&mut diag.byte_offset),
            from_mut(&mut diag.halt_status),
            from_mut(&mut diag.kind),
            from_mut(&mut diag.operand),
            from_mut(&mut diag.payload),
        )
    };
    assert_eq!(rc, -1, "an out-of-range diag index must be a defined -1, got {rc}");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The same defined-outcome law for `jqf_run_error_get`: an out-of-range
/// `index` answers `-1` (the doc-comment contract), never a read past the
/// retained error vector.
#[test]
fn run_error_get_out_of_range_is_a_defined_minus_one() {
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    // Retain at least one error so the out-of-range arm is provably a bounds
    // check, not "the vector happens to be empty".
    let _ = run_sequence(handle, "error(\"boom\")", b"1\n", &mut out);
    // SAFETY: `(NULL, 0)` is the documented sizing probe; the out-of-range
    // index must answer -1 before the probe shape matters.
    let rc = unsafe { jqf_run_error_get(handle, u32::MAX, ptr::null_mut(), 0) };
    assert_eq!(rc, -1, "an out-of-range error index must be a defined -1, got {rc}");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// The same defined-outcome law for the length-carrying diag-text getter:
/// an out-of-range index, an unknown field selector, and an ABSENT field
/// each answer a defined `-1`, and a present field answers its required
/// length through the `(NULL, 0)` sizing probe. A binding author wiring
/// this getter has the same contract every other getter pins.
#[test]
fn diag_get_text_error_laws_are_defined_minus_ones() {
    use jqf_sdk_ffi::{JQF_DIAG_TEXT_PAYLOAD, jqf_diag_get_text};

    // A selector value no record text field answers.
    const UNKNOWN_FIELD: c_int = 77;

    // A SUCCESSFUL run retains informational records; at least one of them
    // carries no payload, which is the absent-field arm.
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let _ = run(handle, ".", b"1", &mut out);
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let mut absent_payload = None;
    for i in 0..count {
        if read_diag(handle, i).payload().is_none() {
            absent_payload = Some(i);
            break;
        }
    }
    let absent = absent_payload.expect("an informational record without a payload");
    // SAFETY: `(NULL, 0)` is the documented sizing probe; an absent field
    // must answer -1 before the probe shape matters.
    let rc = unsafe { jqf_diag_get_text(handle, absent, JQF_DIAG_TEXT_PAYLOAD, ptr::null_mut(), 0) };
    assert_eq!(rc, -1, "an absent payload field must be a defined -1, got {rc}");
    // SAFETY: `handle` is live.
    let oor = unsafe { jqf_diag_get_text(handle, u32::MAX, JQF_DIAG_TEXT_PAYLOAD, ptr::null_mut(), 0) };
    assert_eq!(oor, -1, "an out-of-range diag index must be a defined -1, got {oor}");
    // SAFETY: `handle` is live.
    let unknown = unsafe { jqf_diag_get_text(handle, absent, UNKNOWN_FIELD, ptr::null_mut(), 0) };
    assert_eq!(
        unknown, -1,
        "an unknown field selector must be a defined -1, got {unknown}"
    );
    unsafe { jqf_free(handle) };

    // A PRESENT field answers the required length through the probe.
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, "error(\"boom\")", b"null", &mut out);
    assert_eq!(written, -1);
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let required = unsafe { jqf_diag_get_text(handle, count - 1, JQF_DIAG_TEXT_PAYLOAD, ptr::null_mut(), 0) };
    assert_eq!(required, 4, "the sizing probe answers the required length (`boom`)");
    unsafe { jqf_free(handle) };
}

/// The ABI version symbol exists and matches the compiled constant —
/// bindings refuse a mismatched library on exactly this.
#[test]
fn the_abi_version_symbol_matches_the_constant() {
    assert_eq!(jqf_abi_version(), JQF_ABI_VERSION);
}

/// `jqf_new_limited` with a NULL limits struct is exactly `jqf_new` — the
/// unlimited convenience routes through the same construction.
#[test]
fn new_limited_with_defaults_is_new() {
    let handle = new_handle_limited(std::ptr::null(), std::ptr::null());
    let mut out = vec![0u8; 4096];
    let written = run(handle, ".", b"1", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"1\n");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

/// `jqf_free(NULL)` is a DEFINED no-op, never a crash — the null guard every
/// C free-style entry point keeps (`jqf_free` and `jqf_diag_free_text` both
/// check before re-boxing), so a binding that teardown path races a NULL
/// handle stays safe.
#[test]
fn freeing_a_null_handle_is_a_defined_no_op() {
    // SAFETY: NULL is the documented no-op input; the guard must return
    // without touching freed memory.
    unsafe { jqf_free(ptr::null_mut()) };
    // The process is still healthy: a handle constructed after the no-op
    // works normally.
    let handle = new_handle();
    let mut out = vec![0u8; 4096];
    let written = run(handle, ".", b"1", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"1\n");
    // SAFETY: handle is live.
    unsafe { jqf_free(handle) };
}

#[test]
fn null_handle_is_a_defined_minus_one() {
    use jqf_sdk_ffi::{jqf_compile, jqf_diag_count, jqf_run};
    let mut id = 0u32;
    assert_eq!(
        unsafe { jqf_compile(ptr::null_mut(), b".".as_ptr(), 1, from_mut(&mut id)) },
        -1
    );
    assert_eq!(
        unsafe { jqf_run(ptr::null_mut(), b".".as_ptr(), 1, b"1".as_ptr(), 1, ptr::null_mut(), 0,) },
        -1
    );
    assert_eq!(unsafe { jqf_diag_count(ptr::null()) }, 0);
}

#[test]
fn a_nul_bearing_raise_payload_is_recoverable() {
    use jqf_sdk_ffi::{JQF_DIAG_TEXT_PAYLOAD, codes, jqf_diag_get_text};
    let handle = new_handle();
    let mut out = vec![0u8; 64];
    let written = run(handle, "error(\"a\\u0000b\")", b"null", &mut out);
    assert_eq!(written, -1);
    let mut idx = None;
    for i in 0..unsafe { jqf_diag_count(handle) } {
        let diag = read_diag(handle, i);
        if diag.code == codes::RAISE_PROGRAM {
            idx = Some(i);
            assert!(
                diag.payload().is_none(),
                "CString channel must drop a NUL-bearing payload"
            );
            break;
        }
    }
    let idx = idx.expect("RAISE_PROGRAM record");
    let mut buf = [0u8; 8];
    let n = unsafe { jqf_diag_get_text(handle, idx, JQF_DIAG_TEXT_PAYLOAD, buf.as_mut_ptr(), buf.len()) };
    assert_eq!(n, 3);
    assert_eq!(&buf[..3], b"a\0b");
    unsafe { jqf_free(handle) };
}

#[test]
fn a_garbage_negative_indent_is_compact() {
    let handle = new_handle_limited(
        ptr::null(),
        &jqf_sdk_ffi::JqfEncodeOptions {
            indent: -3,
            raw_strings: 0,
            sort_keys: 0,
            ascii_output: 0,
            raw_output_nul: 0,
        },
    );
    let mut out = vec![0u8; 4096];
    let written = run(handle, ".", b"{\"a\":1}", &mut out);
    assert!(written >= 0);
    let required = usize::try_from(written).unwrap();
    assert_eq!(&out[..required], b"{\"a\":1}\n", "indent=-3 is compact");
    unsafe { jqf_free(handle) };
}

#[test]
fn diag_dropped_is_zero_on_a_short_run_and_defined_on_null() {
    assert_eq!(unsafe { jqf_diag_dropped(ptr::null()) }, 0);
    let handle = new_handle();
    let mut out = vec![0u8; 64];
    let _ = run(handle, ".", b"1", &mut out);
    assert_eq!(unsafe { jqf_diag_dropped(handle) }, 0);
    unsafe { jqf_free(handle) };
}

// --- the streaming sequence surface -----------------------------------------
//
// `jqf_run_sequence_streaming` is the buffer entry points' drive with the
// staging removed: output crosses to the host in bounded chunk callbacks
// instead of one contiguous staged Vec. The tests below pin the laws the
// module doc promises: byte identity with the legacy arm (over both a
// single huge value and a multi-chunk multi-value stream), the chunk-flow
// law (deliveries bounded by the chunk bound, the tail flushed), the
// cancellation law (-1 plus a record naming the cancellation and the bytes
// delivered), and parity of the per-value error channel.

use jqf_sdk_ffi::jqf_run_sequence_streaming;

/// The streaming consumer used by these tests: appends every delivered
/// chunk to a `Vec<Vec<u8>>` behind the context pointer.
unsafe extern "C" fn gather_chunks(context: *mut c_void, bytes: *const u8, len: usize) -> c_int {
    // SAFETY: every test passes a `&mut Vec<Vec<u8>>` as the context, valid
    // for the whole run; `bytes` is readable for `len` bytes per the ABI.
    let sink = unsafe { &mut *context.cast::<Vec<Vec<u8>>>() };
    let slice = if bytes.is_null() || len == 0 {
        &[][..]
    } else {
        // SAFETY: `bytes` is valid for `len` bytes for this call.
        unsafe { std::slice::from_raw_parts(bytes, len) }
    };
    sink.push(slice.to_vec());
    jqf_sdk_ffi::JQF_STREAM_CONTINUE
}

struct Reenter {
    handle: *mut c_void,
    nested: u32,
}

unsafe extern "C" fn reenter_diag(context: *mut c_void, _bytes: *const u8, _len: usize) -> c_int {
    let state = unsafe { &mut *context.cast::<Reenter>() };
    state.nested = unsafe { jqf_diag_count(state.handle) };
    jqf_sdk_ffi::JQF_STREAM_CONTINUE
}

/// The cancel-on-Nth-chunk consumer: continues for the first `cancel_after`
/// deliveries, then returns `JQF_STREAM_CANCEL`.
struct Canceller {
    chunks: Vec<Vec<u8>>,
    cancel_after: usize,
}
unsafe extern "C" fn cancel_late(context: *mut c_void, bytes: *const u8, len: usize) -> c_int {
    // SAFETY: context is a `&mut Canceller`, valid for the run.
    let state = unsafe { &mut *context.cast::<Canceller>() };
    let slice = if bytes.is_null() || len == 0 {
        &[][..]
    } else {
        // SAFETY: `bytes` is valid for `len` bytes for this call.
        unsafe { std::slice::from_raw_parts(bytes, len) }
    };
    state.chunks.push(slice.to_vec());
    if state.chunks.len() > state.cancel_after {
        jqf_sdk_ffi::JQF_STREAM_CANCEL
    } else {
        jqf_sdk_ffi::JQF_STREAM_CONTINUE
    }
}

fn stream(
    handle: *mut c_void,
    program: &str,
    input: &[u8],
    chunk: jqf_sdk_ffi::JqfStreamChunkFn,
    context: *mut c_void,
) -> i64 {
    // SAFETY: `handle`/`program`/`input` are live and valid for their
    // lengths; `context` stays valid until the call returns (it does).
    unsafe {
        jqf_run_sequence_streaming(
            handle,
            program.as_ptr(),
            program.len(),
            input.as_ptr(),
            input.len(),
            Some(chunk),
            context,
        )
    }
}

/// The legacy arm's full output over `(handle, program, input)`: probe, then
/// re-call into an exact buffer — the documented snprintf flow.
fn legacy_full(handle: *mut c_void, program: &str, input: &[u8]) -> Vec<u8> {
    let required = run(handle, program, input, &mut []);
    assert!(required >= 0);
    let required = usize::try_from(required).unwrap();
    let mut out = vec![0u8; required];
    let written = run(handle, program, input, &mut out);
    assert_eq!(written, i64::try_from(required).unwrap());
    out
}

#[test]
fn streaming_matches_the_legacy_arm_over_a_single_huge_value() {
    // ~52 MB from ONE encoded value: exercises the oversized-direct-delivery
    // path end to end, at a size no staged buffer would want.
    let handle = new_handle();
    let program = "\"x\" * 52000000";
    let expected = legacy_full(handle, program, b"null");

    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let ctx = (&raw mut chunks).cast::<c_void>();
    let delivered = stream(handle, program, b"null", gather_chunks, ctx);
    assert!(
        delivered >= 0,
        "the streaming run must succeed where the legacy arm did"
    );
    assert_eq!(
        usize::try_from(delivered).unwrap(),
        expected.len(),
        "the total delivered count equals the legacy arm's required count"
    );
    let concatenated: Vec<u8> = chunks.concat();
    assert_eq!(concatenated, expected, "byte identity across chunks");
    // The chunk bound is a FLOW bound, not a value boundary: the encoder
    // streams one huge string in bounded writes, so the callbacks carry it
    // in bounded pieces and the consumer's per-callback memory stays
    // bounded no matter how large ONE value grows. (The sink's direct-
    // delivery arm exists for an oversized SINGLE write; it never buffers
    // past the bound either way — the RSS harness pins the footprint.)
    assert!(chunks.len() > 1, "a 52 MB value must cross in many bounded deliveries");
    unsafe { jqf_free(handle) };
}

#[test]
fn streaming_matches_the_legacy_arm_across_many_chunk_boundaries() {
    // 2 000 adjacent input values, each producing ~1 KB: ~2 MB total,
    // crossing several JQF_STREAM_CHUNK_BYTES boundaries mid-stream.
    let handle = new_handle();
    let program = r#""1234567890" * 100"#;
    let mut input = String::new();
    for i in 0..2000 {
        input.push_str(&i.to_string());
        input.push(' ');
    }
    let input = input.into_bytes();
    let expected = legacy_full(handle, program, &input);

    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let ctx = (&raw mut chunks).cast::<c_void>();
    let delivered = stream(handle, program, &input, gather_chunks, ctx);
    assert!(delivered >= 0);
    let concatenated: Vec<u8> = chunks.concat();
    assert_eq!(concatenated, expected, "byte identity across many sealed chunks");
    assert!(
        expected.len() > jqf_sdk_ffi::JQF_STREAM_CHUNK_BYTES * 2,
        "the fixture must span more than two chunks to mean anything"
    );
    // Every sealed boundary lands on the chunk bound; only the final open
    // buffer is smaller. A value never straddles two callbacks here (each
    // item is far below the bound), so the sealing law is directly visible.
    for chunk in &chunks[..chunks.len() - 1] {
        assert_eq!(
            chunk.len(),
            jqf_sdk_ffi::JQF_STREAM_CHUNK_BYTES,
            "every non-final chunk seals at the chunk bound"
        );
    }
    assert!(
        chunks.last().is_some_and(|c| !c.is_empty()),
        "the final partial chunk is flushed, never dropped"
    );
    unsafe { jqf_free(handle) };
}

#[test]
fn streaming_cancel_stops_publication_and_records_why() {
    let handle = new_handle();
    let program = r#""1234567890" * 100"#;
    let input: Vec<u8> = b"0 ".repeat(2000);
    let full = legacy_full(handle, program, &input);

    let mut state = Canceller {
        chunks: Vec::new(),
        cancel_after: 2,
    };
    let ctx = (&raw mut state).cast::<c_void>();
    let rc = stream(handle, program, &input, cancel_late, ctx);
    assert_eq!(rc, -1, "a cancellation reports the failure sentinel");
    // The delivered prefix is intact and is exactly the legacy output's
    // prefix: stopping cleanly never corrupts what already crossed.
    let prefix: Vec<u8> = state.chunks.concat();
    assert_eq!(&full[..prefix.len()], &prefix[..], "delivered prefix intact");
    assert!(prefix.len() < full.len(), "the run actually stopped early");
    // The stream says why: a setup record naming the cancellation and the
    // byte count delivered before it.
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let named = (0..count)
        .map(|i| read_diag(handle, i))
        .find(|d| d.code == codes::MACHINE_SETUP && d.payload().is_some_and(|p| p.contains("cancelled")))
        .is_some();
    assert!(named, "a MACHINE_SETUP record must name the cancellation");
    unsafe { jqf_free(handle) };
}

#[test]
fn streaming_cancel_on_the_final_flush_is_not_a_success() {
    // A host that cancels the FINAL flush delivery — after every value
    // already ran successfully — must still see the failure sentinel and
    // the cancellation record, never a positive byte count.
    let handle = new_handle();
    let program = r#""1234567890" * 100"#;
    let input: Vec<u8> = b"0 ".repeat(300);
    let full = legacy_full(handle, program, &input);
    // The fixture spans exactly two deliveries: one sealed chunk plus the
    // smaller open buffer the final flush delivers.
    assert!(full.len() > jqf_sdk_ffi::JQF_STREAM_CHUNK_BYTES);
    assert!(full.len() < jqf_sdk_ffi::JQF_STREAM_CHUNK_BYTES * 2);

    let mut state = Canceller {
        chunks: Vec::new(),
        cancel_after: 1,
    };
    let ctx = (&raw mut state).cast::<c_void>();
    let rc = stream(handle, program, &input, cancel_late, ctx);
    assert_eq!(rc, -1, "a cancelled final flush reports the failure sentinel");
    assert_eq!(state.chunks.len(), 2, "the flush delivery was reached");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let named = (0..count)
        .map(|i| read_diag(handle, i))
        .find(|d| d.code == codes::MACHINE_SETUP && d.payload().is_some_and(|p| p.contains("cancelled")))
        .is_some();
    assert!(named, "a cancelled final flush retains the cancellation record");
    unsafe { jqf_free(handle) };
}

#[test]
fn a_null_chunk_callback_is_a_defined_setup_failure() {
    let handle = new_handle();
    // SAFETY: `handle` is live; the NULL callback is the documented misuse.
    let rc =
        unsafe { jqf_run_sequence_streaming(handle, b".".as_ptr(), 1, b"null".as_ptr(), 4, None, ptr::null_mut()) };
    assert_eq!(rc, -1);
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    assert!(
        (0..count).any(|i| read_diag(handle, i).code == codes::MACHINE_SETUP),
        "a NULL callback retains its setup record"
    );
    unsafe { jqf_free(handle) };
}

#[test]
fn streaming_per_value_errors_match_the_legacy_channel() {
    // `error/1` raises per input value: two adjacent values, two retained
    // per-value error texts, and the LAST-record failure class (-1) on both
    // arms.
    let handle = new_handle();
    let program = r#"error("boom")"#;
    let input = b"1 2";

    // The legacy arm retains one error per failed value.
    let mut out = vec![0u8; 4096];
    let legacy_rc = run(handle, program, input, &mut out);
    assert_eq!(legacy_rc, -1);
    // SAFETY: `handle` is live.
    let legacy_count = unsafe { jqf_run_errors_count(handle) };
    assert_eq!(legacy_count, 2, "both values raise under error/1");
    let mut legacy_texts = Vec::new();
    for i in 0..legacy_count {
        let mut buf = [0u8; 256];
        let written = unsafe { jqf_run_error_get(handle, i, buf.as_mut_ptr(), buf.len()) };
        assert!(written >= 0);
        legacy_texts.push(buf[..usize::try_from(written).unwrap()].to_vec());
    }

    // The streaming arm keeps the SAME channel: same count, same texts.
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let ctx = (&raw mut chunks).cast::<c_void>();
    let rc = stream(handle, program, input, gather_chunks, ctx);
    assert_eq!(rc, -1, "the last-record failure class carries over");
    // SAFETY: `handle` is live.
    let streaming_count = unsafe { jqf_run_errors_count(handle) };
    assert_eq!(streaming_count, legacy_count);
    for i in 0..streaming_count {
        let mut buf = [0u8; 256];
        // SAFETY: `handle` is live, `buf` is valid for its length.
        let written = unsafe { jqf_run_error_get(handle, i, buf.as_mut_ptr(), buf.len()) };
        assert!(written >= 0, "error {i} must be readable after the stream");
        assert_eq!(
            &buf[..usize::try_from(written).unwrap()],
            legacy_texts[i as usize],
            "streaming error {i} must match the buffer path"
        );
    }
    unsafe { jqf_free(handle) };
}

#[test]
fn streaming_an_empty_input_delivers_nothing_and_succeeds() {
    let handle = new_handle();
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let ctx = (&raw mut chunks).cast::<c_void>();
    let rc = stream(handle, ".", b"", gather_chunks, ctx);
    assert_eq!(rc, 0, "zero values is a clean zero-byte answer");
    assert!(chunks.is_empty(), "no callbacks for zero output");
    unsafe { jqf_free(handle) };
}

#[test]
fn a_chunk_callback_must_not_reenter_the_handle() {
    let handle = new_handle();
    let mut state = Reenter { handle, nested: 99 };
    let ctx = (&raw mut state).cast::<c_void>();
    let rc = stream(handle, ".", b"1", reenter_diag, ctx);
    assert!(rc >= 0, "the run itself must complete");
    assert_eq!(state.nested, 0, "nested hop is a defined empty answer, not a hang");
    unsafe { jqf_free(handle) };
}

#[test]
fn truncated_output_is_an_exact_prefix_of_the_full_output() {
    // The counting-sink law behind PART 2: a too-small caller buffer holds
    // EXACTLY the first out_cap bytes of the full output, and the returned
    // count is the full length — the re-call reproduces the rest.
    let handle = new_handle();
    let program = r#""1234567890" * 50"#;
    let input: Vec<u8> = b"0 ".repeat(300);
    let full = legacy_full(handle, program, &input);

    let tiny_cap = 137; // deliberately NOT an item boundary
    let mut tiny = vec![0u8; tiny_cap];
    let written = run(handle, program, &input, &mut tiny);
    let required = usize::try_from(written).unwrap();
    assert_eq!(required, full.len(), "the count is the FULL length");
    assert_eq!(
        &tiny[..],
        &full[..tiny_cap],
        "a short buffer holds the exact prefix, not garbage"
    );
    unsafe { jqf_free(handle) };
}

#[test]
fn a_single_record_over_the_batch_cap_is_refused_with_a_named_limit() {
    // PART-3 law: one record whose ENCODED output alone breaches the feed's
    // batch cap is a clean refusal, and the diagnostic NAMES the record,
    // its progress, and the cap — never an anonymous batch total. (The
    // cap here is the memory ceiling the retained-input dial maps to.)
    let limits = jqf_sdk_ffi::JqfLimits {
        max_output_bytes: u64::MAX,
        max_memory_bytes: 4096,
        max_spill_bytes: u64::MAX,
        max_nesting_depth: u32::MAX,
        deadline_ms: 0,
        control_callback: None,
        control_context: ptr::null_mut(),
    };
    let handle = new_handle_limited(&raw const limits, ptr::null());
    let id = compile(handle, "[range(5000)]");
    let feed = open_feed(handle, id, JQF_FEED_PROFILE_STRICT);
    assert!(feed_push(handle, feed, b"null\n") >= 0);

    let mut out = vec![0u8; 65536];
    let written = feed_poll(handle, feed, &mut out);
    assert_eq!(written, -1, "a single record over the cap is terminal");
    // SAFETY: `handle` is live.
    let count = unsafe { jqf_diag_count(handle) };
    let named = (0..count)
        .map(|i| read_diag(handle, i))
        .find(|d| {
            d.payload()
                .is_some_and(|p| p.contains("exceeds its 4096-byte cap at record 0"))
        })
        .is_some();
    assert!(
        named,
        "the refusal must name the record and the limit, got {count} records"
    );
    unsafe { jqf_free(handle) };
}
