//! Integration coverage for the RSS governor: the `--max-rss` dial's three forms, the default-on ceiling with cgroup
//! detection, the release-and-recheck grace step, the retained-input attribution, the overshoot bound, and the
//! degradation paths. The sibling `max_memory.rs` pins the ACCOUNTED ceiling; this file pins the PHYSICAL one, with its
//! own diag code (`MACHINE_MEMORY`), message shape, and exit class.

use std::process::{Command, Output, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

fn run_jqf(args: &[&str], input: &[u8]) -> Output {
    run_jqf_on(args, input, &[])
}

fn run_jqf_with_env(args: &[&str], input: &[u8], env: &[(&str, &str)]) -> Output {
    run_jqf_on(args, input, env)
}

/// Runs jqf with `input` redirected from a REGULAR FILE, the seekable stdin form.
///
/// The retained-input laws this file pins are whole-read laws, and the streaming-stdin seekability rule keeps them
/// exactly there: a PIPE stdin now streams per value and never retains the input (a giant piped document is refused by
/// the ceiling MID-run, when the decode crosses it — a different, streaming law). The seekable redirect is therefore
/// the shape these assertions were written against, and remains the shape that exercises them.
fn run_jqf_on(args: &[&str], input: &[u8], env: &[(&str, &str)]) -> Output {
    let mut file_path = std::env::temp_dir();
    file_path.push(format!(
        "jqf-rss-input-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&file_path, input).expect("input file");
    let mut command = Command::new(jqf_binary());
    command.env("JQF_NO_CONFIG", "1");
    command
        .args(args)
        .stdin(Stdio::from(std::fs::File::open(&file_path).expect("open input file")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("jqf runs to completion");
    let _ = std::fs::remove_file(&file_path);
    output
}

/// A collect big enough to exceed any ceiling this file names (the release binary materializes ~20 bytes per integer,
/// so 4M integers are ~80 MiB).
fn blowup() -> &'static str {
    "[range(0;4000000)]"
}

/// A fake memory-detection hierarchy: 16 GiB of "physical RAM" with a 2 GiB cgroup limit, so the default ceiling must
/// resolve to 80% of 2 GiB = 1,717,986,918 bytes. Written into a per-test temp dir.
fn fake_memory_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jqf-rss-fake-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("cgroup")).expect("fake cgroup dir");
    std::fs::write(dir.join("meminfo"), "MemTotal: 16777216 kB\n").expect("meminfo");
    std::fs::write(dir.join("cgroup/memory.max"), "2147483648\n").expect("memory.max");
    dir
}

#[test]
fn tiny_max_rss_refuses_a_blowup_with_the_physical_message_and_exit_5() {
    let output = run_jqf(&["--max-rss", "67108864", "-c", blowup()], b"null");
    assert_eq!(output.status.code(), Some(5), "the runtime failure class");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("physical memory ceiling exceeded"),
        "the physical refusal names its own class, got: {stderr:?}"
    );
    assert!(
        stderr.contains("67108864") && stderr.contains("--max-rss"),
        "the refusal names the ceiling and the override flag, got: {stderr:?}"
    );
    assert!(
        stderr.contains("freed pages were released and the footprint re-measured"),
        "the grace step is named, got: {stderr:?}"
    );
    assert!(
        stderr.contains("retained input is 4 bytes"),
        "the retained-input split is attributed, got: {stderr:?}"
    );
    assert!(
        !stderr.contains("memory limit exceeded: the ceiling is"),
        "the ACCOUNTED message must not leak into the physical refusal"
    );
    assert!(output.stdout.is_empty(), "a refused request publishes nothing");
}

#[test]
fn the_refusal_carries_its_own_diag_code_under_diagnostics() {
    let output = run_jqf(&["--max-rss", "67108864", "--diagnostics", "-c", blowup()], b"null");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("MACHINE_MEMORY"),
        "the diag record names MACHINE_MEMORY, got: {stderr:?}"
    );
    assert!(
        !stderr.contains("MACHINE_RESOURCE"),
        "the physical refusal must not masquerade as the accounted rejection"
    );
}

#[test]
fn max_rss_zero_disables_the_ceiling() {
    // The same blowup that refused at 64 MiB completes with the dial off.
    let output = run_jqf(&["--max-rss", "0", "-c", blowup()], b"null");
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(!stderr.contains("ceiling exceeded"));
}

#[test]
fn the_default_ceiling_follows_the_cgroup_limit() {
    let dir = fake_memory_dir();
    let output = run_jqf_with_env(
        &["--diagnostics", "-c", "."],
        b"[1,2,3]",
        &[("JQF_MEMORY_DETECT_DIR", dir.to_str().expect("utf8 dir"))],
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("ceiling=1717986918"),
        "the default ceiling is 80% of the 2 GiB cgroup limit, not of the 16 GiB host: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_refusal_under_the_fake_cgroup_names_its_provenance() {
    let dir = fake_memory_dir();
    // The fake cgroup limits the default ceiling to 1.6 GiB; a collect big enough to cross it must be refused with the
    // cgroup provenance named. 60M elements, not the 4M of `blowup`: on Linux the enforcement number is the OS RSS
    // (statm), and the DEBUG build's final materialization phase can outrun the governor's sample window — the collect
    // must cross the ceiling during the METERED fill, which 40M elements did not in debug (its metered phase tops out
    // below the ceiling; in the container). 60M crosses on both platforms.
    let output = run_jqf_with_env(
        &["-c", "[range(0;60000000)]"],
        b"null",
        &[("JQF_MEMORY_DETECT_DIR", dir.to_str().expect("utf8 dir"))],
    );
    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("default: 80% of 2147483648 bytes (cgroup memory limit)"),
        "the refusal names the cgroup-derived ceiling, got: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_explicit_percent_resolves_against_the_same_detection() {
    let dir = fake_memory_dir();
    let output = run_jqf_with_env(
        &["--max-rss", "50%", "--diagnostics", "-c", "."],
        b"[1,2,3]",
        &[("JQF_MEMORY_DETECT_DIR", dir.to_str().expect("utf8 dir"))],
    );
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("ceiling=1073741824"),
        "50% of the 2 GiB cgroup limit is 1 GiB, got: {stderr:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn malformed_dial_values_are_usage_errors() {
    for value in ["", "abc", "-5", "80%%", "0%"] {
        let output = run_jqf(&["--max-rss", value, "."], b"null");
        assert_eq!(
            output.status.code(),
            Some(2),
            "--max-rss {value:?} must be a usage error"
        );
    }
}

#[test]
fn the_grace_step_rescues_a_run_whose_freed_pages_were_still_cached() {
    // The first collect (~51 MiB in the release build) stays under the 64 MiB ceiling and is freed; the second collect
    // runs while the first's pages are still cached, so the observed footprint crosses the ceiling. The
    // release-and-recheck grace step must return the cached pages and let the run COMPLETE — a default-on ceiling that
    // fired here would be the refusal-mode regression returning.
    let output = run_jqf(
        &[
            "--max-rss",
            "67108864",
            "-c",
            "[range(0;500000)] | empty | [range(0;250000)] | length",
        ],
        b"null",
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "the grace step must rescue the run: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn the_refusal_fires_before_the_overshoot_passes_1_25x() {
    // The bound, pinned: the refusal must fire before the resident set exceeds ceiling x (1 + 1/margin). The reported
    // RSS at the refusal is in the message; the OS peak (`maxrss` under --diagnostics) is the ceiling on what the
    // process actually reached.
    //
    // The margin is committed PER PLATFORM because the overshoot is set by the largest single allocation step — a Vec
    // doubling lands the working set past the ceiling in one jump, between any two governor checks — which is
    // allocator/OS-dependent, not something sampling cadence can chase. Measured overshoots of the working set over the
    // ceiling: macOS ~1.12x, linux-aarch64 ~1.31x, x86_64-linux ~1.52x (its final Vec doubling is larger relative to
    // the ceiling), so each platform commits the next standard margin above its measurement. The overshoot allowance,
    // as the fraction `1/margin` of the ceiling the refusal may let the OS peak reach: (numerator, denominator) of that
    // fraction. macOS 1/4 (1.25x), linux-aarch64 1/2 (1.5x), x86_64-linux 3/4 (1.75x).
    let (margin_num, margin_den) = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        (3, 4)
    } else if cfg!(target_os = "linux") {
        (1, 2)
    } else {
        (1, 4)
    };
    let output = run_jqf(&["--max-rss", "67108864", "--diagnostics", "-c", blowup()], b"null");
    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    let maxrss = stderr
        .lines()
        .find_map(|line| {
            line.strip_prefix("jqf: rss:")
                .and_then(|rest| rest.split_whitespace().find_map(|field| field.strip_prefix("maxrss=")))
        })
        .and_then(|text| text.parse::<u64>().ok())
        .expect("the diagnostics report carries maxrss");
    let ceiling = 67_108_864u64;
    let bound = ceiling + ceiling * margin_num / margin_den;
    assert!(
        maxrss <= bound,
        "the refusal must fire before resident exceeds ceiling x (1 + {margin_num}/{margin_den}): \
         peak {maxrss} vs ceiling {ceiling} (bound {bound})"
    );
}

#[test]
fn a_giant_input_is_attributed_and_refused_before_the_run() {
    // The retained input is the dominant term on the streaming lanes (the 001 table's 122x-1004x ratios). A request
    // whose retained input alone exceeds the ceiling refuses at the first CLI decision point, and the message says the
    // input is the piece that fired.
    let input: Vec<u8> = {
        let mut bytes = Vec::new();
        bytes.push(b'[');
        for index in 0..400_000_u32 {
            if index > 0 {
                bytes.push(b',');
            }
            bytes.extend_from_slice(index.to_string().as_bytes());
        }
        bytes.push(b']');
        bytes
    };
    let output = run_jqf(&["--max-rss", "16777216", "-c", "."], &input);
    assert_eq!(output.status.code(), Some(5));
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        stderr.contains("retained input is") && stderr.contains("bytes of that"),
        "the refusal attributes the retained input, got: {stderr:?}"
    );
}

#[test]
fn the_diagnostics_report_prints_the_physical_footprint_and_cross_check() {
    let output = run_jqf(&["--diagnostics", "-c", "."], b"[1,2,3]");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    let line = stderr
        .lines()
        .find(|line| line.starts_with("jqf: rss:"))
        .expect("--diagnostics prints the rss report");
    for field in [
        "current_rss=",
        "peak_rss=",
        "current_commit=",
        "peak_commit=",
        "page_faults=",
        "maxrss=",
        "retained_input=7",
        "ceiling=",
    ] {
        assert!(line.contains(field), "the report carries {field}: {line}");
    }
}

#[test]
fn the_streamed_null_input_fold_charges_only_what_it_holds() {
    //: the `-n` input-family fold no longer reads whole — the
    // route pulls the source bytes on demand and drops them as the fold advances, so NOTHING is retained wholesale. The
    // truthful charge is the pulled-byte tally on the request account, and `retained_input` reports zero (the pre-A1
    // law charged the full read because every byte of it stayed resident for the whole request).
    let input: &[u8] = b"{\"amount\":1}\n{\"amount\":2}\n{\"amount\":3}\n";
    let output = run_jqf(
        &[
            "--diagnostics",
            "--no-parallel",
            "--input-format",
            "ndjson",
            "-n",
            "-c",
            "reduce inputs as $r (0; . + $r.amount)",
        ],
        input,
    );
    assert!(output.status.success(), "the fold succeeds: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    let line = stderr
        .lines()
        .find(|line| line.starts_with("jqf: rss:"))
        .expect("--diagnostics prints the rss report");
    assert!(
        line.contains("retained_input=0"),
        "a streamed fold retains no whole-input buffer: {line}"
    );
}

#[test]
fn the_deferred_null_input_read_is_charged_as_retained_input() {
    // §0b, narrowed by A1 to the shapes that still read whole: a MULTI-FILE input-family request keeps the combined
    // read (per-file provenance and the missing-file walk are whole-buffer facts), so its deferred read must still
    // report every byte it retained.
    let dir = std::env::temp_dir();
    let first = dir.join(format!("jqf-rss-nfile-a-{}", std::process::id()));
    let second = dir.join(format!("jqf-rss-nfile-b-{}", std::process::id()));
    std::fs::write(&first, b"{\"amount\":1}\n").expect("writes the first file");
    std::fs::write(&second, b"{\"amount\":2}\n").expect("writes the second file");
    let retained = first.metadata().map_or(0, |m| m.len()) + second.metadata().map_or(0, |m| m.len());
    let output = Command::new(jqf_binary())
        .env("JQF_NO_CONFIG", "1")
        .args([
            "--diagnostics",
            "--no-parallel",
            "--input-format",
            "ndjson",
            "-n",
            "-c",
            "reduce inputs as $r (0; . + $r.amount)",
        ])
        .args([&first, &second])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("jqf runs to completion");
    let _ = std::fs::remove_file(&first);
    let _ = std::fs::remove_file(&second);
    assert!(output.status.success(), "the fold succeeds: {output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "3");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    let line = stderr
        .lines()
        .find(|line| line.starts_with("jqf: rss:"))
        .expect("--diagnostics prints the rss report");
    assert!(
        line.contains(&format!("retained_input={retained}")),
        "the multi-file deferred read charges both files' bytes: {line}"
    );
}

#[test]
fn the_report_is_diagnostics_only_never_explain() {
    // The explain-surface law: `--explain` alone stays counter-free.
    let output = run_jqf(&["--explain", "-c", "."], b"[1,2,3]");
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(
        !stderr.contains("jqf: rss:"),
        "--explain alone must not print the rss report: {stderr:?}"
    );
}
