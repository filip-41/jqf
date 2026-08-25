//! The `.jqf.toml` config file: the two-tier design's acceptance battery.
//!
//! Every test runs the binary in a scratch directory with a scrubbed HOME, so discovery and the global config are fully
//! controlled: the only config a test can see is the one it wrote. This is the same hermeticity law the gates live
//! under (`JQF_NO_CONFIG` in the harnesses) — a developer's real `~/.jqf.toml` must never reach a test.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn jqf_binary() -> &'static str {
    env!("CARGO_BIN_EXE_jqf")
}

/// Runs jqf with a controlled cwd and environment: the working directory (config discovery walks up from it), the
/// child's HOME (the global config lives under it), extra env, and the stdin bytes. `JQF_NO_CONFIG` is scrubbed first,
/// so a test opts INTO hermeticity explicitly.
fn run_in(dir: &Path, home: &Path, args: &[&str], stdin: &str, env: &[(&str, &str)]) -> (i32, Vec<u8>, Vec<u8>) {
    let mut command = Command::new(jqf_binary());
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env_remove("JQF_NO_CONFIG")
        .env_remove("XDG_CONFIG_HOME")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let mut child = command.spawn().expect("jqf spawns");
    // A usage-error child exits WITHOUT reading stdin, closing the pipe mid-write; BrokenPipe is the expected race
    // there.
    if let Err(error) = child.stdin.take().expect("stdin is piped").write_all(stdin.as_bytes()) {
        assert!(
            error.kind() == std::io::ErrorKind::BrokenPipe,
            "input writes to jqf's stdin: {error}"
        );
    }
    let output = child.wait_with_output().expect("jqf runs");
    (output.status.code().unwrap_or(-1), output.stdout, output.stderr)
}

/// A fresh scratch directory under the system temp dir.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jqf-config-{name}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
    std::fs::write(path, body).expect("config file");
}

/// The global config directory under HOME, mirroring `config.rs`'s platform law (macOS: `~/Library/Application
/// Support/jqf`; elsewhere the XDG base).
fn global_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library").join("Application Support").join("jqf")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".config").join("jqf")
    }
}

const INPUT: &str = "{\"b\":1,\"a\":\"x\"}";

/// The plain (hermetic) answer for `.` over `INPUT` — every comparison in this file is byte-identity against this
/// baseline.
fn plain(home: &Path, dir: &Path) -> Vec<u8> {
    run_in(dir, home, &["--no-config", "."], INPUT, &[]).1
}

/// A Tier P default (compact) actually changes presentation bytes.
#[test]
fn tier_p_defaults_change_presentation() {
    let dir = scratch("tier-p");
    let home = scratch("tier-p-home");
    write(&dir.join(".jqf.toml"), "[defaults]\ncompact = true\n");
    let baseline = plain(&home, &dir);
    let (code, out, _) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 0);
    assert_eq!(out, b"{\"b\":1,\"a\":\"x\"}\n", "the config's compact must apply");
    assert_ne!(out, baseline, "the config must have changed the bytes");
    // argv wins over the config: --indent 3 beats the config's compact.
    let (_, out, _) = run_in(&dir, &home, &["--indent", "3", "."], INPUT, &[]);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "{\n   \"b\": 1,\n   \"a\": \"x\"\n}\n",
        "argv --indent beats the config's compact"
    );
    // The indent family resolves in ALPHABETICAL key order (TOML tables iterate sorted here; there is no preserve-order
    // feature), so `indent` speaks after `compact-output`/`compact` regardless of which line is written first.
    write(&dir.join(".jqf.toml"), "[defaults]\ncompact = true\nindent = 4\n");
    let (_, out, _) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "{\n    \"b\": 1,\n    \"a\": \"x\"\n}\n",
        "the alphabetically-last family key (indent) wins over compact"
    );
    // The same law with the lines swapped: file order does NOT decide, the alphabetical order of the keys does.
    write(&dir.join(".jqf.toml"), "[defaults]\nindent = 4\ncompact = true\n");
    let (_, out, _) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "{\n    \"b\": 1,\n    \"a\": \"x\"\n}\n",
        "indent still wins over compact when written first"
    );
}

/// EVERY Tier S flag in a config file: none takes effect — stdout is byte-identical to the hermetic run and the exit
/// code is unchanged. The "forgot to classify" law's other half: a Tier S key is seen, warned, and ignored (the stderr
/// names the semantic rule).
#[test]
fn tier_s_keys_are_never_read() {
    let dir = scratch("tier-s");
    let home = scratch("tier-s-home");
    // Every long spelling the parser accepts that is NOT config-readable
    //. Values are plausible for the flag's
    // own grammar where one exists; the point is that none of them acts.
    let semantic = "\
[defaults]
null-input = true
raw-input = true
slurp = true
raw-output = true
join-output = true
ascii-output = true
sort-keys = true
raw-output0 = true
exit-status = true
seed = 1
stream = true
follow = true
seq = true
input-format = \"yaml\"
input-dialect = \"yaml.core@1\"
output-dialect = \"yaml.block@1\"
from-file = \"x\"
library-path = \"x\"
arg = \"x\"
argjson = \"1\"
slurpfile = \"x\"
rawfile = \"x\"
args = true
jsonargs = true
plan-out = \"x\"
plan-file = \"x\"
edit = true
diff = true
output = \"x\"
in-place = true
no-atomic = true
render-header = \"absent\"
render-width = \"cjk\"
render-shape = \"table\"
render-max-width = 5
ndjson-terminator = \"crlf\"
help = true
version = true
list-builtins = true
list-formats = true
help-format = \"json\"
explain-code = 1
config = \"x\"
no-config = true
show-config = true
";
    write(&dir.join(".jqf.toml"), semantic);
    let baseline = plain(&home, &dir);
    let (code, out, err) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 0);
    assert_eq!(out, baseline, "no Tier S key may change the output bytes");
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("slurp is a semantic (argv-only) flag"),
        "the Tier S key must be seen and warned, not silently dropped: {err}"
    );
    assert!(
        err.contains("show-config is a semantic (argv-only) flag"),
        "--show-config must never be config-readable: {err}"
    );
    assert!(
        err.contains("no-config is a semantic (argv-only) flag"),
        "--no-config must never be config-readable: {err}"
    );
}

/// Precedence, highest wins: argv beats the config file.
#[test]
fn precedence_argv_beats_config() {
    let dir = scratch("prec-argv");
    let home = scratch("prec-argv-home");
    write(&dir.join(".jqf.toml"), "[defaults]\ncompact = true\n");
    let (code, out, _) = run_in(&dir, &home, &["--indent", "3", "."], INPUT, &[]);
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "{\n   \"b\": 1,\n   \"a\": \"x\"\n}\n",
        "argv --indent must beat the discovered compact"
    );
}

/// Precedence: `--config PATH` beats discovery entirely (the explicit file is the only file read).
#[test]
fn precedence_config_flag_beats_discovery() {
    let dir = scratch("prec-config");
    let home = scratch("prec-config-home");
    write(&dir.join(".jqf.toml"), "[defaults]\ncompact = true\n");
    let explicit = dir.join("explicit.toml");
    write(&explicit, "[defaults]\nindent = 5\n");
    let (code, out, _) = run_in(&dir, &home, &["--config", explicit.to_str().unwrap(), "."], INPUT, &[]);
    assert_eq!(code, 0);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "{\n     \"b\": 1,\n     \"a\": \"x\"\n}\n",
        "--config must replace the discovered file"
    );
}

/// Precedence: the nearest discovered `.jqf.toml` is overlaid on the global file and wins per key; the global file
/// still fills keys the discovery file leaves unset.
#[test]
fn precedence_discovery_beats_global_and_global_fills_gaps() {
    let dir = scratch("prec-global");
    let home = scratch("prec-global-home");
    write(&global_dir(&home).join(".jqf.toml"), "[defaults]\nindent = 6\n");
    // Discovery compact wins the indent family over the global's indent=6.
    write(&dir.join(".jqf.toml"), "[defaults]\ncompact = true\n");
    let (_, out, _) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(out, b"{\"b\":1,\"a\":\"x\"}\n", "nearest wins per key");
    // With no discovery file, the global's indent=6 is the effective default.
    let empty = scratch("prec-global-empty");
    let (_, out, _) = run_in(&empty, &home, &["."], INPUT, &[]);
    assert_eq!(
        String::from_utf8_lossy(&out),
        "{\n      \"b\": 1,\n      \"a\": \"x\"\n}\n",
        "the global file is the effective default when nothing is nearer"
    );
}

/// `--no-config` and a non-empty `JQF_NO_CONFIG` are fully hermetic: a hostile config on disk cannot reach the run.
#[test]
fn hermetic_no_config_and_env() {
    let dir = scratch("hermetic");
    let home = scratch("hermetic-home");
    write(&dir.join(".jqf.toml"), "[defaults]\ncompact = true\n");
    let baseline = plain(&home, &dir);
    // A hostile config would compact; both hermetic spellings must not.
    let (code, out, _) = run_in(&dir, &home, &["--no-config", "."], INPUT, &[]);
    assert_eq!(code, 0);
    assert_eq!(out, baseline, "--no-config must be byte-identical to no config");
    let (code, out, _) = run_in(&dir, &home, &["."], INPUT, &[("JQF_NO_CONFIG", "1")]);
    assert_eq!(code, 0);
    assert_eq!(out, baseline, "JQF_NO_CONFIG=1 must be byte-identical to no config");
    // An EMPTY JQF_NO_CONFIG is ignored (the NO_COLOR-shaped law).
    let (code, out, _) = run_in(&dir, &home, &["."], INPUT, &[("JQF_NO_CONFIG", "")]);
    assert_eq!(code, 0);
    assert_eq!(
        out, b"{\"b\":1,\"a\":\"x\"}\n",
        "an empty JQF_NO_CONFIG does not disable config"
    );
}

/// `--show-config` reports the effective configuration and the origin of every value (argv, a config file, built-in
/// default), naming the files it read. It is a command: exit 0, no stdin read.
#[test]
fn show_config_reports_provenance() {
    let dir = scratch("show-config");
    let home = scratch("show-config-home");
    write(&dir.join(".jqf.toml"), "[defaults]\ncompact = true\n");
    let (code, out, _) = run_in(&dir, &home, &["-M", "--show-config"], "", &[]);
    assert_eq!(code, 0);
    let report = String::from_utf8_lossy(&out);
    assert!(report.contains("compact = true  # config file"), "{report}");
    assert!(
        report.contains("color = false  # argv"),
        "argv -M must report argv origin: {report}"
    );
    assert!(report.contains("parallel = true  # built-in default"), "{report}");
    let read = format!(
        "# read from: {}",
        dir.canonicalize()
            .unwrap_or_else(|_| dir.clone())
            .join(".jqf.toml")
            .display()
    );
    assert!(report.contains(&read), "the report must name the file: {report}");
}

/// Unknown keys and sections warn on stderr and are ignored (visible, not fatal); a key outside any section warns too.
/// The run still answers.
#[test]
fn unknown_keys_and_sections_warn_and_are_ignored() {
    let dir = scratch("unknown");
    let home = scratch("unknown-home");
    write(
        &dir.join(".jqf.toml"),
        "not-a-key = 1\n[defaults]\nfuture-flag = true\n[query]\nname = \"x\"\n[other]\nx = 1\n",
    );
    let baseline = plain(&home, &dir);
    let (code, out, err) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 0);
    assert_eq!(out, baseline, "unknown keys must not change the output");
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("unknown key \"future-flag\""), "{err}");
    assert!(
        err.contains("[query] is reserved for the query-artifact direction"),
        "{err}"
    );
    assert!(err.contains("unknown section [other]"), "{err}");
    assert!(err.contains("key \"not-a-key\" is outside a section"), "{err}");
}

/// A malformed config file is a hard usage error naming the file — never a silent ignore.
#[test]
fn malformed_config_is_a_usage_error() {
    let dir = scratch("malformed");
    let home = scratch("malformed-home");
    write(&dir.join(".jqf.toml"), "[defaults\ncompact = true\n");
    let (code, _, err) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 2, "a malformed config is a usage error");
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("invalid TOML"), "{err}");
    assert!(err.contains(".jqf.toml"), "{err}");
}

/// A mistyped known key (wrong TOML type) is a hard usage error naming the file and key.
#[test]
fn mistyped_key_is_a_usage_error() {
    let dir = scratch("mistyped");
    let home = scratch("mistyped-home");
    write(&dir.join(".jqf.toml"), "[defaults]\ncompact = \"yes\"\n");
    let (code, _, err) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 2);
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("compact must be a boolean"), "{err}");
    assert!(err.contains(".jqf.toml"), "{err}");
}

/// The §2 border-case ruling: a config `output-format` may name only the JSON family; a non-JSON target changes the
/// value model and is rejected with the ruling spelled out.
#[test]
fn config_output_format_is_json_family_only() {
    let dir = scratch("output-format");
    let home = scratch("output-format-home");
    write(&dir.join(".jqf.toml"), "[defaults]\noutput-format = \"yaml\"\n");
    let (code, _, err) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 2);
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("JSON family"), "{err}");
    assert!(err.contains("must be given per invocation"), "{err}");
    // The JSON family is accepted and applied.
    write(&dir.join(".jqf.toml"), "[defaults]\noutput-format = \"ndjson\"\n");
    let (code, out, _) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 0);
    assert_eq!(out, b"{\"b\":1,\"a\":\"x\"}\n", "ndjson output is one compact record");
    // argv `--seq` beats the config preference: the output is RS-framed json-seq, not the config's plain json. jq's
    // --seq pretty-prints by default (json-seq does not force compact), so the payload keeps the 2-space layout.
    write(&dir.join(".jqf.toml"), "[defaults]\noutput-format = \"json\"\n");
    let (code, out, _) = run_in(&dir, &home, &["--seq", "."], "\x1e{\"b\":1,\"a\":\"x\"}", &[]);
    assert_eq!(code, 0);
    assert_eq!(
        out, b"\x1e{\n  \"b\": 1,\n  \"a\": \"x\"\n}\n",
        "--seq's json-seq wins over the config (argv beats config)"
    );
}

/// The help text documents the config surface (the one-table law: the two new flags are rows of the same table the
/// parser reads).
#[test]
fn help_documents_the_config_surface() {
    let dir = scratch("help");
    let home = scratch("help-home");
    let (code, out, _) = run_in(&dir, &home, &["--help"], "", &[]);
    assert_eq!(code, 0);
    let help = String::from_utf8_lossy(&out);
    assert!(help.contains("  --config PATH"), "{help}");
    assert!(help.contains("  --no-config"), "{help}");
    assert!(help.contains("  --show-config"), "{help}");
    assert!(help.contains("Configuration:"), "{help}");
}

/// A nonexistent explicit `--config` file is a hard error, never a silent fallback to discovery.
#[test]
fn missing_explicit_config_is_an_error() {
    let dir = scratch("missing-config");
    let home = scratch("missing-config-home");
    let (code, _, err) = run_in(&dir, &home, &["--config", "does-not-exist.toml", "."], INPUT, &[]);
    assert_eq!(code, 2);
    let err = String::from_utf8_lossy(&err);
    assert!(err.contains("cannot read config file"), "{err}");
}

/// `--show-config` output is valid TOML under `[defaults]` and the binary accepts it as a config file.
#[test]
fn show_config_output_round_trips_as_a_config_file() {
    let dir = scratch("show-roundtrip");
    let home = scratch("show-roundtrip-home");
    let (code, out, err) = run_in(&dir, &home, &["--show-config"], "", &[]);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    let report = String::from_utf8(out).expect("utf-8");
    assert!(
        report.contains("[defaults]"),
        "the report must carry a [defaults] header: {report}"
    );
    write(&dir.join(".jqf.toml"), &report);
    let (code, _, err) = run_in(&dir, &home, &["-n", "1"], "", &[]);
    assert_eq!(
        code,
        0,
        "the binary must accept its own --show-config report: {}",
        String::from_utf8_lossy(&err)
    );
}

#[test]
fn config_color_true_does_not_override_no_color() {
    let dir = scratch("color-true");
    let home = scratch("color-true-home");
    write(&dir.join(".jqf.toml"), "[defaults]\ncolor = true\n");
    let (code, out, _) = run_in(&dir, &home, &["-c", "."], INPUT, &[("NO_COLOR", "1")]);
    assert_eq!(code, 0);
    assert!(
        !out.contains(&0x1b),
        "config color=true must not force colour under NO_COLOR: {out:?}"
    );
    let (code, out, _) = run_in(&dir, &home, &["-c", "-C", "."], INPUT, &[("NO_COLOR", "1")]);
    assert_eq!(code, 0);
    assert!(out.contains(&0x1b), "-C must still force colour on under NO_COLOR");
}

/// An unknown key warns ONCE per run: argv is parsed twice (the catalog-less pre-pass, then the catalog pass for
/// file-name detection), and the warning must not double with the second pass.
#[test]
fn unknown_key_warning_prints_once_per_run() {
    let dir = scratch("warn-once");
    let home = scratch("warn-once-home");
    write(&dir.join(".jqf.toml"), "[defaults]\nbogus_key = 1\n");
    let (code, _, err) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 0, "an unknown key warns; it never fails the run");
    let err = String::from_utf8_lossy(&err);
    assert_eq!(
        err.matches("unknown key").count(),
        1,
        "the warning must print exactly once per run: {err}"
    );
}

/// `tab = false` in a project file UNDOES an inherited global `tab = true`: the false spelling clears the indent family
/// so the request falls back to argv or the built-in default — never stuck on inherited tabs.
#[test]
fn project_tab_false_undoes_an_inherited_global_tab() {
    let dir = scratch("tab-false");
    let home = scratch("tab-false-home");
    write(&global_dir(&home).join(".jqf.toml"), "[defaults]\ntab = true\n");
    write(&dir.join(".jqf.toml"), "[defaults]\ntab = false\n");
    let baseline = plain(&home, &dir);
    // The global file alone does apply.
    let moved = scratch("tab-false-empty");
    let (_, out, _) = run_in(&moved, &home, &["."], INPUT, &[]);
    assert_ne!(out, baseline, "the global tab = true must be visible");
    // The project's false clears it back to the default.
    let (code, out, _) = run_in(&dir, &home, &["."], INPUT, &[]);
    assert_eq!(code, 0);
    assert_eq!(
        out, baseline,
        "a false tab clears the family; the default indentation stands"
    );
}

/// The `--edit` format-mismatch refusal names WHERE the clashing output format came from: a config-file preference must
/// not read as a flag the user never typed.
#[test]
fn edit_mismatch_names_the_config_origin() {
    let dir = scratch("edit-origin");
    let home = scratch("edit-origin-home");
    write(&dir.join(".jqf.toml"), "[defaults]\noutput-format = \"json\"\n");
    std::fs::write(dir.join("x.toml"), "a = 1\n").expect("input file");
    let (code, _, err) = run_in(&dir, &home, &["--edit", ".", "x.toml"], "", &[]);
    assert_eq!(code, 2, "the mismatch is a usage error");
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("the config file's output-format"),
        "the refusal must name the config origin, got {err}"
    );
    // An explicit argv flag names itself instead.
    let (code, _, err) = run_in(
        &dir,
        &home,
        &["--edit", "--no-config", "--output-format", "json", ".", "x.toml"],
        "",
        &[],
    );
    assert_eq!(code, 2);
    let err = String::from_utf8_lossy(&err);
    assert!(
        err.contains("--output-format selected json"),
        "an argv selection names the flag, got {err}"
    );
}
