"""Subprocess drive for the jqf gate scripts: one resolver, one check, one drive.

Every script that drives the jqf binary resolves it through `resolve_binary`
and drives it with `run_gate`, so the conventions live in exactly one place:

- the binary default (`target/release/jqf`) and the `sys.argv[1]` override;
- the executable check;
- the per-command drive: `[binary] + args` argv, byte stdin, captured
  stdout/stderr, a per-command timeout, and exit-code handling that never
  raises on a nonzero run (the caller decides what a nonzero means);
- the stale-binary guard: `freshness_problem` compares the binary's mtime
  against the newest product source mtime and fails loud on staleness.
"""

import os
import subprocess

# The repo root and its canonical binaries. Walk up from this file
# (tools/gates/jqfgate/proc.py) until Cargo.toml.
_ROOT = os.path.dirname(os.path.abspath(__file__))
while _ROOT != os.path.dirname(_ROOT) and not os.path.isfile(os.path.join(_ROOT, "Cargo.toml")):
    _ROOT = os.path.dirname(_ROOT)
ROOT = _ROOT
DEFAULT_RELEASE_JQF = os.path.join(ROOT, "target", "release", "jqf")

# Product source directories: top-level crate trees only. Everything under
# tools/ is harness, target/ is build output, and the dot-directories hold
# plans and ledgers — none of them affect the binary under test.
_PRODUCT_DIR_PREFIXES = ("jqf-",)

# Directories whose contents never link into the binary.
_NON_SOURCE_DIRS = frozenset({"benches", "tests", "tools", "target", ".git"})


def set_hermetic():
    """A developer's .jqf.toml must never reach a gate — the harnesses are
    hermetic by construction, not by convention."""
    os.environ["JQF_NO_CONFIG"] = "1"


def resolve_binary(argv, default=DEFAULT_RELEASE_JQF):
    """The binary under test: the first positional argument, else the default.

    `argv` is the script's `sys.argv[1:]`; `default` is the per-script
    default (the release binary for correctness lanes, the PGO binary for
    the measurement-facing ones). Pure resolution — the caller still owns
    its executable/staleness message and exit code.
    """
    return argv[0] if argv else default


def executable(path):
    """Whether `path` names a real executable file."""
    return os.path.isfile(path) and os.access(path, os.X_OK)


def _product_source_mtimes():
    """Yields the mtimes of every product source file under the repo.

    A product source is a `*.rs` or `Cargo.toml` outside the non-source
    directories (benches/tests/tools/target). Build scripts (`build.rs`) and
    the lock file are included — any of them changing can change the binary.
    """
    for name in ("Cargo.toml", "Cargo.lock"):
        path = os.path.join(ROOT, name)
        try:
            yield os.stat(path).st_mtime
        except FileNotFoundError:
            pass
    for dirpath, dirnames, filenames in os.walk(ROOT):
        # Prune the walk at every level. At the root only product crate
        # trees survive (tools/ is harness, target/ is build output, the
        # dot-directories hold plans and ledgers); below the root the
        # non-source directories are skipped.
        if dirpath == ROOT:
            dirnames[:] = [
                d for d in dirnames if d.startswith(_PRODUCT_DIR_PREFIXES)
            ]
        else:
            dirnames[:] = [d for d in dirnames if d not in _NON_SOURCE_DIRS]
        for filename in filenames:
            if not (filename.endswith(".rs") or filename == "Cargo.toml"):
                continue
            path = os.path.join(dirpath, filename)
            try:
                yield os.stat(path).st_mtime
            except FileNotFoundError:
                pass


_newest_source_mtime = None


def newest_source_mtime():
    """The newest product source mtime, computed once per process."""
    global _newest_source_mtime
    if _newest_source_mtime is None:
        _newest_source_mtime = max(_product_source_mtimes(), default=0.0)
    return _newest_source_mtime


_freshness_cache = {}


def freshness_problem(binary):
    """Why `binary` is stale, or None when it is not.

    Compares the binary's mtime against the newest product source mtime. The
    Makefile already builds `$(JQF)` before its gate recipes run, so under a
    make target this never fires; it exists for the direct-invocation path
    where a stale binary used to print a green receipt. The result is cached
    per binary path — a differential drives thousands of runs and must not
    rescan the tree for each one.
    """
    if not binary:
        return None
    if binary in _freshness_cache:
        return _freshness_cache[binary]
    try:
        binary_mtime = os.stat(binary).st_mtime
    except FileNotFoundError:
        _freshness_cache[binary] = None  # the executable check owns this
        return None
    source = newest_source_mtime()
    problem = None
    if source > binary_mtime + 1.0:
        problem = (
            "the newest product source is newer than the binary; run "
            "`make` (or `cargo build --release -p jqf`) before judging "
            "this run"
        )
    _freshness_cache[binary] = problem
    return problem


class FreshnessError(RuntimeError):
    """A gate drove a binary that is stale."""


def run_command(argv, input=None, env=None, timeout=None, text=False,
               check=False, cwd=None, **kwargs):
    """One subprocess invocation with the shared conventions, to completion.

    The core of `run_gate` — same capture/timeout/check conventions — for
    the non-jqf drives the scripts make (cargo metadata, the compat corpus
    dump, git, make). `argv` is the full argument vector; stdout/stderr
    default to capture but an explicit redirect (`subprocess.DEVNULL`,
    `subprocess.STDOUT`) wins. Returns the `CompletedProcess`.
    """
    stdout = kwargs.pop("stdout", None)
    stderr = kwargs.pop("stderr", None)
    return subprocess.run(
        list(argv),
        input=input,
        env=env,
        stdout=subprocess.PIPE if stdout is None else stdout,
        stderr=subprocess.PIPE if stderr is None else stderr,
        timeout=timeout,
        check=check,
        text=text,
        cwd=cwd,
        **kwargs,
    )


def run_gate(binary, args, input=None, env=None, timeout=None, text=False,
             check=False, **kwargs):
    """One jqf invocation with the shared conventions, to completion.

    `args` is the argument vector AFTER the binary (flags and program).
    `input` is the byte stdin (None = inherit). stdout/stderr are captured
    unless the caller overrides via `kwargs`; a timeout is a hang guard, not
    a budget; `check=False` by default so a nonzero exit is the caller's
    decision, exactly as the extracted scripts made it. Returns the
    `CompletedProcess`.
    """
    problem = freshness_problem(binary)
    if problem:
        raise FreshnessError(f"{binary} is stale: {problem}")
    return run_command(
        [binary] + list(args),
        input=input, env=env, timeout=timeout, text=text, check=check,
        **kwargs,
    )
