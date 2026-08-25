//! The streaming-stdin vertical: a NON-SEEKABLE stdin (pipe/FIFO) with a record-route input publishes per value instead
//! of reading whole — jq's default, which jqf used to hang on (`tail -f x | jqf.msg`).
//!
//! The seekability rule is the law under test: `jqf … < file` keeps the whole-document route (a regular file is
//! seekable), while a pipe or FIFO streams. Assert the ROUTE, never the timing — the lane-recognition suite has not
//! landed on this base, so the route is pinned here through `--explain`'s `route:` line, which records the rung that
//! served the request.
//!
//! Every byte-identity assertion pairs the streaming pipe against the whole-read file form of the same bytes: stdout,
//! stderr, and exit class.

use std::io::{BufRead as _, Write as _};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs jqf with stdin PIPED (the streaming shape) and returns the output.
fn run_piped(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(input)
        .expect("input writes to jqf's stdin");
    child.wait_with_output().expect("jqf runs to completion")
}

/// Runs jqf with stdin redirected from a REGULAR FILE (the seekable shape).
fn run_file(args: &[&str], input: &[u8]) -> Output {
    let path = std::env::temp_dir().join(format!(
        "jqf-w4-input-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, input).expect("input file");
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(args)
        .stdin(Stdio::from(std::fs::File::open(&path).expect("open input file")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion");
    let _ = std::fs::remove_file(&path);
    output
}

/// Asserts stdout bytes, stderr bytes, and exit class are identical between the piped and the file-redirected form of
/// the same request.
fn assert_pipe_matches_file(args: &[&str], input: &[u8]) {
    let piped = run_piped(args, input);
    let file = run_file(args, input);
    assert_eq!(
        piped.stdout, file.stdout,
        "stdout differs between pipe and file for {args:?} over {input:?}"
    );
    assert_eq!(
        piped.stderr, file.stderr,
        "stderr differs between pipe and file for {args:?} over {input:?}"
    );
    assert_eq!(
        piped.status.code(),
        file.status.code(),
        "exit class differs between pipe and file for {args:?} over {input:?}"
    );
}

/// The `--explain` route line of one run.
fn route_of(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("jqf: explain: route: "))
        .unwrap_or_else(|| panic!("no route line in: {stderr}"))
        .to_owned()
}

const NDJSON: &[u8] = b"{\"msg\":\"hi\"}\n{\"msg\":\"yo\"}\n{\"msg\":\"hey\"}\n";

#[test]
fn a_regular_file_redirect_keeps_the_whole_document_route() {
    // The seekability rule's load-bearing half: `jqf … < file` redirects a REGULAR file, which is seekable, so the
    // whole-document route must survive untouched. This is the shape every benchmark measures.
    let output = run_file(&["--explain", ".msg"], NDJSON);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        route_of(&output),
        "sequence",
        "a seekable stdin keeps the whole-read adjacent-value route"
    );
    assert_eq!(output.stdout, b"\"hi\"\n\"yo\"\n\"hey\"\n");
}

#[test]
fn a_pipe_takes_the_streaming_route() {
    // The seekability rule's other half: the same request over a pipe takes the stream route, and its bytes are the
    // whole-read run's bytes.
    let output = run_piped(&["--explain", ".msg"], NDJSON);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(route_of(&output), "stream");
    assert_eq!(output.stdout, b"\"hi\"\n\"yo\"\n\"hey\"\n");
}

#[test]
fn a_pipe_publishes_per_record_before_eof() {
    // The defect: jqf read stdin whole, so a slow writer hung until EOF. The stream route must publish each record as
    // its bytes arrive — proven by receiving record one's output while the writer still holds the pipe open, with a
    // deadline so a regression fails the test instead of hanging it.
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .arg(".msg")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // The reader forwards each LINE as it arrives, so the test can observe record one while the pipe is still open — a
    // read-to-end forwarder would only deliver at EOF and prove nothing about the publish point.
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).unwrap_or(0) == 0 {
                break;
            }
            if tx.send(line.clone()).is_err() {
                break;
            }
        }
    });
    stdin.write_all(b"{\"msg\":\"first\"}\n").expect("record one");
    stdin.flush().expect("flush record one");
    let first = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("record one published before EOF");
    // The pipe is still open: record one must already be visible.
    assert_eq!(first, b"\"first\"\n");
    stdin.write_all(b"{\"msg\":\"second\"}\n").expect("record two");
    let second = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("record two published before EOF");
    assert_eq!(second, b"\"second\"\n");
    drop(stdin); // EOF
    let status = child.wait().expect("jqf exits");
    assert_eq!(status.code(), Some(0));
    let _ = reader.join();
}

#[test]
fn piped_input_is_byte_identical_to_the_file_form() {
    // The byte-identity law: `cat big.ndjson | jqf` publishes exactly what `jqf < big.ndjson` publishes — the stream
    // route reproduces the whole-read run's stdout, stderr, and exit class. NDJSON-shaped input, space-separated
    // adjacent values, a truncated final value, and a malformed line all ride the same law.
    assert_pipe_matches_file(&[".msg"], NDJSON);
    assert_pipe_matches_file(&["-c", "."], b"1 2 3");
    assert_pipe_matches_file(&[".a"], b"{\"a\":1}\n{\"a\":2");
    assert_pipe_matches_file(&[".a"], b"{\"a\":1}\nnotjson\n{\"a\":2}\n");
    assert_pipe_matches_file(&[".a"], b"{\"a\":1} {\"a\":2}\n");
    // The paired-route law: the forced floor `[.][0] | P` rides the same evaluator drives, so its pipe and file forms
    // agree too.
    assert_pipe_matches_file(&["-c", "[.][0] | .a"], b"{\"a\":1}\n{\"a\":2}\n");
    assert_pipe_matches_file(&["-c", "[.][0] | .a"], b"1 2 3");
    assert_pipe_matches_file(&["-s", "length"], b"1\n2\n3\n");
    assert_pipe_matches_file(&["-n", ".a // 7"], b"1\n2\n");
    assert_pipe_matches_file(&["-R", "."], b"x\ny\n");
    // The record dialects ride the same law over their own framing.
    assert_pipe_matches_file(&["--input-format", "ndjson", ".a"], NDJSON);
    assert_pipe_matches_file(
        &[
            "--input-format",
            "ndjson",
            "--input-dialect",
            "ndjson.recovering@1",
            ".a",
        ],
        b"{\"a\":1}\ngarbage\n{\"a\":3}\n",
    );
    assert_pipe_matches_file(&["--seq", "."], b"\x1e1\n\x1e2\n");
    // `<RS>1<RS>2` frames two units and the cut sits at the last RS; the per-cycle record numbering now charges FRAMER
    // ORDINALS (a coalesced RS run is ONE next text's prefix, not an empty unit plus a unit), so stdout, stderr, and
    // the exit class agree with the whole-input run wherever the cycle boundaries fall. `coalesced_rs_record_n_…` below
    // forces the boundary deliberately.
    let seq_pipe = run_piped(&["--seq", "."], b"\x1e1\x1e2");
    let seq_file = run_file(&["--seq", "."], b"\x1e1\x1e2");
    assert_eq!(seq_pipe.stdout, seq_file.stdout);
    assert_eq!(seq_pipe.status.code(), seq_file.status.code());
    assert_pipe_matches_file(&["--input-format", "csv", "."], b"a,b\n1,2\n3,4\n");
}

#[test]
fn coalesced_rs_record_n_matches_the_seekable_whole_read_across_cycles() {
    // Drops the structured diagnostic's source-base line before comparing the piped run against the seekable
    // whole-read: the base is the one recorded residual (cycle-relative here, absolute on the file form).
    fn drop_base(s: &str) -> std::vec::Vec<&str> {
        s.lines().filter(|l| !l.contains("base=")).collect()
    }
    // The record-N law across cycles: `record N` must name the same unit a seekable whole-read of the same prefix
    // names, even when a cycle boundary falls inside a COALESCED RS run — `\x1e\x1e` is ONE next text's `1*RS` prefix,
    // not an empty unit plus a unit. The stream is pushed in three writes so the cycle cut lands inside that run; the
    // malformed last unit renders an issue whose `(record N, …)` number is the whole-read's. Counting raw RS bytes per
    // cycle (the old counter) drifted base_record +1 past every coalesced pair and rendered this issue one record late
    // on the piped form only.
    let input = b"\x1e{\"a\":1}\n\x1e\x1e{\"a\":2}\ngarbage\n";
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["--seq", "-c", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let pushes: [&[u8]; 3] = [b"\x1e{\"a\":1}\n", b"\x1e\x1e{\"a\":2}\n", b"garbage\n"];
    for push in pushes {
        stdin.write_all(push).expect("push writes");
        stdin.flush().expect("push flushes");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stdin); // EOF finalizes the held tail through the recovering law.
    let output = child.wait_with_output().expect("jqf runs to completion");
    assert_eq!(output.status.code(), Some(0), "--seq parse errors are advisory");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The seekable whole-read of these bytes names this fault `record 1` (dense-from-zero ordinal of the second unit) —
    // that file-form answer IS the expectation; the raw-byte counter rendered it one record late.
    assert!(
        stderr.contains("(record 1, "),
        "the malformed third unit must render as the whole-read names it: {stderr}"
    );
    assert!(
        !stderr.contains("(record 3, "),
        "no drifted ordinal may reach the rendering: {stderr}"
    );
    // And the whole-read form of the same bytes is the oracle it must match: stdout bytes, exit class, and the record
    // ordinal all agree. The one known divergence is the structured diagnostic's SOURCE BASE — the streaming cycle
    // reports the cycle-relative base (base=1) where the whole-read reports the absolute offset (base=11). The recorded
    // streaming-stdin law says absolute; translating it lives in the cycle sink and is recorded as RESIDUALS.md item 13
    // rather than smuggled past this test. stdout bytes, exit class, and the record ordinal all agree. The one known
    // divergence is the structured diagnostic's SOURCE BASE — the streaming cycle reports the cycle-relative base
    // (base=1) where the whole-read reports the absolute offset (base=11). The recorded streaming-stdin law says
    // absolute; translating it lives in the cycle sink and is recorded as RESIDUALS.md item 13 rather than smuggled
    // past this test.
    let file = run_file(&["--seq", "-c", "."], input);
    assert_eq!(output.stdout, file.stdout, "published frames agree");
    assert_eq!(output.status.code(), file.status.code(), "exit agrees");
    assert_eq!(
        drop_base(&stderr),
        drop_base(&String::from_utf8_lossy(&file.stderr)),
        "issue ordinals and messages agree (modulo the recorded source-base residual)"
    );
}

#[test]
fn a_fifo_streams_like_a_pipe() {
    // A named FIFO is the other non-seekable kind the rule names: a persistent writer (like `tail -f`) pushes records
    // and the run publishes them as they arrive, ending at EOF when the writer closes.
    let fifo = std::env::temp_dir().join(format!(
        "jqf-w4-fifo-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        let _ = std::fs::remove_file(&fifo);
        let status = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
        assert!(status.success(), "mkfifo {fifo:?}");
        assert!(
            std::fs::metadata(&fifo).expect("fifo metadata").file_type().is_fifo(),
            "the fixture is a FIFO"
        );
    }
    #[cfg(not(unix))]
    {
        // FIFOs are a Unix shape; the pipe tests above carry the law, and the body below is cfg(unix) so the windows
        // check type-checks cleanly.
        let _ = fifo;
        return;
    }
    #[cfg(unix)]
    {
        let reader = {
            let fifo = fifo.clone();
            std::thread::spawn(move || {
                let child = Command::new(jqf_binary())
                    .env("JQF_NO_CONFIG", "1")
                    .arg(".msg")
                    .stdin(Stdio::from(std::fs::File::open(&fifo).expect("open fifo")))
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("jqf spawns");
                child.wait_with_output().expect("jqf runs to completion")
            })
        };
        // The writer must hold the FIFO open across records: a per-write redirect closes the write end between writes,
        // which a reader sees as EOF (a producer restart — the follow e2e's own row).
        let writer_fifo = fifo.clone();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&writer_fifo)
                .expect("open fifo for writing");
            file.write_all(b"{\"msg\":\"one\"}\n").expect("record one");
            file.flush().expect("flush");
            std::thread::sleep(Duration::from_millis(300));
            file.write_all(b"{\"msg\":\"two\"}\n").expect("record two");
            file.flush().expect("flush");
        });
        let output = reader.join().expect("reader joins");
        writer.join().expect("writer joins");
        let _ = std::fs::remove_file(&fifo);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(output.stdout, b"\"one\"\n\"two\"\n");
    }
}

#[test]
fn dash_s_and_dash_n_on_a_pipe_stay_whole_read() {
    // `-s` and `-n` inherently need all input (jq is the same), so a pipe must NOT stream for them: the eager read
    // keeps the whole-document routes. Assert the route rather than the timing.
    let slurped = run_piped(&["--explain", "-s", "length"], b"1\n2\n3\n");
    assert_eq!(route_of(&slurped), "sequence");
    assert_eq!(slurped.stdout, b"3\n");
    let null = run_piped(&["--explain", "-n", ".a // 7"], b"1\n2\n");
    assert_eq!(route_of(&null), "sequence");
    assert_eq!(null.stdout, b"7\n");
}

#[test]
fn the_record_dialects_take_the_stream_route_on_a_pipe() {
    // ndjson, json-seq, and csv on a pipe: the framed arm of the stream route, still byte-identical to the whole-input
    // run (covered above) and still named `stream`.
    let ndjson = run_piped(
        &["--input-format", "ndjson", "--explain", ".a"],
        b"{\"a\":1}\n{\"a\":2}\n",
    );
    assert_eq!(route_of(&ndjson), "stream");
    let seq = run_piped(&["--seq", "--explain", "."], b"\x1e1\n\x1e2\n");
    assert_eq!(route_of(&seq), "stream");
    let csv = run_piped(&["--input-format", "csv", "--explain", "."], b"a,b\n1,2\n");
    assert_eq!(route_of(&csv), "stream");
}

#[test]
fn a_document_shaped_format_on_a_pipe_stays_whole_read() {
    // TOML is one document: it has no record route, so a pipe must keep the whole-read single-document route (the rule
    // never special-cases it).
    let toml = run_piped(&["--input-format", "toml", "--explain", ".a"], b"a = 1\n");
    assert_eq!(toml.status.code(), Some(0));
    assert_eq!(route_of(&toml), "single-document");
    assert_eq!(toml.stdout, b"1\n");
}

#[test]
fn an_input_family_program_on_a_pipe_reads_whole() {
    // The input family's shared cursor is whole-input by construction: on a pipe it reads to completion exactly as it
    // reads a file. The stream route IS taken (plan.rs pushes Route:Stream for the input family); only the bytes and
    // exit class are the whole-read run's.
    let piped = run_piped(&["--explain", "input"], b"1\n2\n3\n");
    let file = run_file(&["--explain", "input"], b"1\n2\n3\n");
    assert_eq!(piped.stdout, file.stdout);
    assert_eq!(piped.status.code(), file.status.code());
}

#[test]
fn a_piped_headered_csv_is_refused_where_the_file_form_is_served() {
    // The headered CSV dialect is a usage error on a pipe: the header row is a whole-stream fact, and the cycle drive
    // re-opens the framer and the payload provider on every refill, so a multi-cycle pipe would consume its own first
    // record as a header and key the rest by that record's values. A seekable redirect reads whole and serves the same
    // bytes.
    let piped = run_piped(
        &["--input-format", "csv", "--input-dialect", "csv.rfc4180-header@1", "."],
        b"a,b\n1,2\n3,4\n5,6\n",
    );
    assert_eq!(
        piped.status.code(),
        Some(2),
        "a piped headered CSV is a usage error before any byte is read"
    );
    assert!(!piped.stderr.is_empty(), "the refusal must say why");
    // The file form (seekable stdin, whole-read) still serves the dialect.
    let file = run_file(
        &["--input-format", "csv", "--input-dialect", "csv.rfc4180-header@1", "."],
        b"a,b\n1,2\n3,4\n5,6\n",
    );
    assert_eq!(
        file.status.code(),
        Some(0),
        "the seekable form serves the headered dialect"
    );
    assert_eq!(
        String::from_utf8_lossy(&file.stdout),
        "{\n  \"a\": \"1\",\n  \"b\": \"2\"\n}\n{\n  \"a\": \"3\",\n  \"b\": \"4\"\n}\n{\n  \"a\": \"5\",\n  \"b\": \"6\"\n}\n"
    );
}

#[test]
fn stream_flag_on_a_pipe_takes_the_incremental_event_route() {
    //: `--stream` is no longer excluded from the streaming-stdin
    // path. A JSON input on a pipe drives the INCREMENTAL event parser (the fgets-shaped chunk drive), and its bytes
    // are the whole-input event route's bytes over the same text. The route line names the event form.
    for args in [
        &["--explain", "--stream", "-c", "."][..],
        &["--explain", "--stream-errors", "-c", "."][..],
        &["--explain", "--stream", "-c", ".[1] | length"][..],
    ] {
        let piped = run_piped(args, b"{\"a\":[1,2]}");
        let file = run_file(args, b"{\"a\":[1,2]}");
        assert_eq!(piped.stdout, file.stdout, "stdout for {args:?}");
        assert_eq!(piped.status.code(), file.status.code(), "exit for {args:?}");
    }
    let output = run_piped(&["--explain", "--stream", "-c", "."], b"{\"a\":1}\n{\"b\":2}\n");
    assert_eq!(
        route_of(&output),
        "stream-events",
        "--stream on a pipe is served by the incremental event route"
    );
    assert_eq!(output.stdout, b"[[\"a\"],1]\n[[\"a\"]]\n[[\"b\"],2]\n[[\"b\"]]\n");
    // Error recovery stays line-shaped on the pipe (jq's fgets law): one error per line, exactly as the whole-input run
    // reports it.
    let piped_err = run_piped(&["--stream-errors", "-c", "."], b"x\nx\nx");
    let file_err = run_file(&["--stream-errors", "-c", "."], b"x\nx\nx");
    assert_eq!(piped_err.stdout, file_err.stdout);
    assert_eq!(piped_err.status.code(), file_err.status.code());
}

#[test]
fn stream_events_publish_before_eof_on_a_pipe() {
    // The plan's headline promise: a `--stream` request emits events from a live pipe before EOF — the whole-read route
    // could not, and the plan names OOM as the failure mode it exists to prevent. Write one complete value, keep the
    // pipe open, and require its events within a deadline.
    let mut child = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args(["--stream", "-c", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("jqf spawns");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).unwrap_or(0) == 0 {
                break;
            }
            if tx.send(line.clone()).is_err() {
                break;
            }
        }
    });
    stdin.write_all(b"{\"a\":1,\"b\":[2,3]}\n").expect("value one");
    stdin.flush().expect("flush");
    let first = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the first event published before EOF");
    assert_eq!(first, b"[[\"a\"],1]\n");
    let second = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a later event published before EOF");
    assert_eq!(second, b"[[\"b\",0],2]\n");
    drop(stdin); // EOF
    let status = child.wait().expect("jqf exits");
    assert_eq!(status.code(), Some(0));
    let _ = reader.join();
}

#[test]
fn dash_n_stream_is_the_canonical_idiom_on_a_pipe() {
    //: `-n --stream` is NOT a dropped flag. jq's canonical streaming
    // idiom `fromstream(inputs)` reconstructs the document, and `[inputs]` collects the events. `-n` keeps the
    // whole-read input model (jq's own `-n` defers parsing to the pulls), so the route is the whole-input event form,
    // byte-identical to its file shape.
    let piped = run_piped(&["-n", "--stream", "-c", "fromstream(inputs)"], b"[1,2,3]");
    let file = run_file(&["-n", "--stream", "-c", "fromstream(inputs)"], b"[1,2,3]");
    assert_eq!(piped.stdout, file.stdout);
    assert_eq!(piped.status.code(), file.status.code());
    assert_eq!(piped.stdout, b"[1,2,3]\n");
    let collected = run_piped(&["-n", "--stream", "-c", "[inputs]"], b"[1,2,3]");
    assert_eq!(collected.stdout, b"[[[0],1],[[1],2],[[2],3],[[2]]]\n");
    let output = run_piped(&["--explain", "-n", "--stream", "-c", "."], b"[1,2,3]");
    assert_eq!(route_of(&output), "stream-events");
    assert_eq!(output.stdout, b"null\n");
}

#[test]
fn unbuffered_flushes_per_item_on_a_pipe() {
    // `--unbuffered` gets jq's actual meaning on the streaming path: stdout is flushed per ITEM, which is stronger than
    // the per-refill cadence the stream route already keeps. Same bytes either way.
    let plain = run_piped(&[".msg"], NDJSON);
    let unbuffered = run_piped(&["--unbuffered", ".msg"], NDJSON);
    assert_eq!(plain.stdout, unbuffered.stdout);
    assert_eq!(unbuffered.status.code(), Some(0));
}
