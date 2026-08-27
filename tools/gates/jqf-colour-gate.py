#!/usr/bin/env python3
"""The colour law's standing gate: colour is a RENDERING of bytes
that are already decided, and this lane proves it on the process boundary.

THE LAW: it must be impossible for colour to change the byte stream. The
colour pass inserts ANSI SGR spans around the JSON tokens the encoder's bytes
own and touches nothing else, so stripping the spans from a coloured run
recovers the plain run's bytes EXACTLY. Every gate, the compat corpus, and
every differential run non-TTY, so they stay byte-identical by construction —
this lane is the stated test that says so, and it refuses to be vacuous:

* every coloured arm must actually CARRY escape spans (a colour that broke
  would make the strip-identity pass trivially, so the vacuity guard is the
  gate's teeth);
* every strip-identity row asserts strip(coloured) == plain byte-for-byte;
* the -M arm asserts byte identity with the plain run;
* the JQ_COLORS rows pin the palette parse (custom fields apply, a malformed
  value falls back to the defaults with `Failed to set $JQ_COLORS` on stderr
  at exit 0);
* the pty rows pin the DECISION law end-to-end — TTY default on, NO_COLOR
  (non-empty) off, empty NO_COLOR ignored, -C forcing on under NO_COLOR,
  -M forcing off in both orders — because the decision is the other half of
  "byte-identical by construction".

The -C rows of the compat corpus are the byte-oracle half (coloured bytes vs
the reference, live); this lane owns the strip-identity half.

Usage: tools/gates/jqf-colour-gate.py JQF  (JQF = the jqf binary)
"""

import os
import pty
import re
import select
import subprocess
import sys
import threading
import time

from jqfgate import proc

proc.set_hermetic()

# (program, input, extra flags, expect_colour). Both arms run the SAME
# program and input; `expect_colour` is False only for the `-r` raw-arm rows
# whose root is a text item — those print the string's OWN bytes with no
# colour BY LAW (jq's raw arm, probed), so their vacuity expectation is the
# reverse: the raw arm must SUPPRESS the spans.
ROWS = [
    (".", '{"a":1,"b":[true,false,"s",1.5],"c":{"nested":"v"}}', "", True),
    (".", '{"a":1}', "-c", True),
    (".", '[[],{},{"k":{}}]', "-c", True),
    (".", '{"b":2,"a":1}', "-S", True),
    (".", '{"héllo":1,"a":2}', "-Sa -c", True),
    (".", '{"esc":"a\\"b\\\\c\\u0041"}', "", True),
    (".", '["héllo",-0.5e+3,1E2,0]', "-c", True),
    (".", '{"a":1}', "--tab", True),
    (".", '{"a":1}', "--indent 0", True),
    (".", '{"a":1}', "-j", True),
    (".", '"hello"', "-r", False),
    (".", '"null"', "-r", False),
    (".[0]", "[1]", "-r", True),
    (".[]", "[true,null,1.5]", "-r", True),
    (".[0] | @tsv", '[[1,"x"]]', "-r", False),
    (".", "\x1e{\"a\":1}\x1e{\"b\":[2]}", "--seq", True),
    (".", "\x1e{\"a\":1}\x1e{\"b\":[2]}", "--seq -c", True),
    (".", "\x1e\"x\"", "--seq -r", False),
    # render.terminal@1's tree shape colours under the same decision law;
    # strip identity must hold over the frame bytes too. The plain and table
    # shapes have no lexical token law and stay monochrome (no row: a
    # monochrome format is the ABSENCE of colour, which vacuity cannot probe).
    (
        ".",
        '{"name":"ada","id":1,"tags":[true,null,1.5],"t":{"!money":2}}',
        "--output-format render --output-dialect render.terminal@1",
        True,
    ),
]

# JQ_COLORS rows: (value, must_contain, stderr_contains_or_none).
JQ_COLORS_ROWS = [
    # The custom eight-field palette: keys are the 8th field.
    ("1;31:2;32:3;33:4;34:5;35:6;36:7;37:8;38", b"\x1b[8;38m", None),
    # A malformed value: defaults render, Failed to set $JQ_COLORS on stderr, exit 0.
    ("notacolor", None, b"Failed to set $JQ_COLORS\n"),
    # Fewer than eight fields: the rest default.
    ("1;31", None, None),
]

# pty rows: (argv suffix, env, coloured-expected).
PTY_ROWS = [
    (["."], {}, True),
    (["."], {"NO_COLOR": "1"}, False),
    (["."], {"NO_COLOR": ""}, True),
    (["-C", "."], {"NO_COLOR": "1"}, True),
    (["-M", "."], {}, False),
    (["-M", "-C", "."], {}, False),
    (["-C", "-M", "."], {}, False),
]

ESC_RE = re.compile(rb"\x1b\[[0-9;]*m")

# Hang guards. Every jqf invocation is bounded, and the pty loop has a hard
# deadline: a binary that blocks on stdout (or never exits) must fail its row,
# not park the gate.
RUN_TIMEOUT_SECONDS = 30
PTY_DEADLINE_SECONDS = 30


def run(jqf, args, stdin, env=None):
    """Runs jqf, returning (exit code, stdout bytes, stderr bytes).

    A timeout is reported as exit 124 with the reason on stderr — the caller's
    per-row checks then fail the row loudly instead of the gate dying in
    subprocess internals.
    """
    full_env = dict(os.environ)
    full_env.pop("NO_COLOR", None)
    full_env.pop("JQ_COLORS", None)
    if env:
        full_env.update(env)
    try:
        child = proc.run_gate(
            jqf,
            args,
            input=stdin.encode(),
            env=full_env,
            timeout=RUN_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return 124, b"", b"colour-gate: jqf timed out"
    except OSError as error:
        return 127, b"", f"colour-gate: could not run jqf: {error}".encode()
    return child.returncode, child.stdout, child.stderr


def strip_ansi(bytes_):
    """Removes every ANSI SGR span, leaving the decided bytes."""
    return ESC_RE.sub(b"", bytes_)


def run_on_pty(jqf, args, env=None):
    """Runs jqf with stdout on a pty, returning (stdout bytes, stderr bytes).

    stderr is drained from the moment the child exists — a chatty child could
    fill its stderr pipe and deadlock on write before we ever read it — and
    the whole rendezvous is bounded by PTY_DEADLINE_SECONDS, so a jqf that
    never exits fails its row instead of hanging the gate.
    """
    full_env = dict(os.environ)
    full_env.pop("NO_COLOR", None)
    if env:
        full_env.update(env)
    master, slave = pty.openpty()
    child = subprocess.Popen(
        [jqf] + args,
        stdin=subprocess.PIPE,
        stdout=slave,
        stderr=subprocess.PIPE,
        env=full_env,
        close_fds=True,
    )
    os.close(slave)
    err_chunks = []

    def drain_stderr():
        try:
            while True:
                chunk = child.stderr.read(65536)
                if not chunk:
                    break
                err_chunks.append(chunk)
        except OSError:
            pass

    drainer = threading.Thread(target=drain_stderr, daemon=True)
    drainer.start()
    out = b""
    deadline = time.monotonic() + PTY_DEADLINE_SECONDS
    timed_out = False
    try:
        child.stdin.write(b'{"a":1}')
        child.stdin.close()
    except OSError:
        # An already-dead child surfaces at the write; the loop below still
        # collects whatever reached the pty before it died.
        pass
    while True:
        if time.monotonic() >= deadline:
            timed_out = True
            break
        try:
            ready, _, _ = select.select([master], [], [], 0.5)
        except OSError:
            break
        if not ready:
            if child.poll() is not None:
                try:
                    while True:
                        chunk = os.read(master, 65536)
                        if not chunk:
                            break
                        out += chunk
                except OSError:
                    pass
                break
            continue
        try:
            chunk = os.read(master, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    os.close(master)
    if timed_out:
        # A deadline miss is a row failure, never a silent partial read: the
        # marker makes `err` non-empty so the caller's stderr check fires.
        err_chunks.append(
            f"colour-gate: jqf timed out after {PTY_DEADLINE_SECONDS}s\n".encode()
        )
        child.kill()
    try:
        child.wait(timeout=10)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait()
    drainer.join(timeout=5)
    return out, b"".join(err_chunks)


def main():
    if len(sys.argv) != 2:
        print("usage: tools/gates/jqf-colour-gate.py JQF", file=sys.stderr)
        return 2
    jqf = sys.argv[1]
    # run() drives jqf through proc.run_gate, which refuses a stale binary;
    # surface that refusal as this gate's own exit rather than a traceback.
    stale = proc.freshness_problem(jqf)
    if stale:
        print(f"colour-gate: {jqf} is stale: {stale}", file=sys.stderr)
        return 2
    failures = []
    rows = 0

    for program, input_, flags, expect_colour in ROWS:
        rows += 1
        plain_args = (flags.split() if flags else []) + [program]
        plain_code, plain_out, plain_err = run(jqf, plain_args, input_)
        coloured_code, coloured_out, _ = run(jqf, ["-C"] + plain_args, input_)
        mono_code, mono_out, _ = run(jqf, ["-M"] + plain_args, input_)
        label = f"{flags} {program} on {input_!r}"
        # The plain arm is the reference the other two are judged against; a
        # jqf that exits nonzero on ALL three arms would otherwise pass every
        # identity check vacuously (three empty streams agree).
        if plain_code != 0:
            failures.append(
                f"plain arm exited {plain_code} on {label}: "
                f"{plain_err.strip()[:200]!r}"
            )
            continue
        if not (plain_code == coloured_code == mono_code):
            failures.append(
                f"exit divergence on {label}: plain={plain_code} "
                f"coloured={coloured_code} mono={mono_code}"
            )
        if mono_out != plain_out:
            failures.append(f"-M diverges from plain on {label}")
        if expect_colour:
            if b"\x1b[" not in coloured_out:
                failures.append(f"VACUITY: -C produced no escapes on {label}")
            elif strip_ansi(coloured_out) != plain_out:
                failures.append(f"strip identity broken on {label}")
        else:
            # The raw-arm rows: a root text item's bytes are the value, so
            # colour must NOT touch them — no escapes at all.
            if b"\x1b[" in coloured_out:
                failures.append(f"raw arm leaked colour on {label}")
            if coloured_out != plain_out:
                failures.append(f"raw arm changed the bytes on {label}")

    for value, must_contain, stderr_want in JQ_COLORS_ROWS:
        rows += 1
        env = {"JQ_COLORS": value}
        plain_code, plain_out, _ = run(jqf, [".", "-c"], '{"a":1}', env=env)
        coloured_code, coloured_out, coloured_err = run(
            jqf, ["-C", ".", "-c"], '{"a":1}', env=env
        )
        label = f"JQ_COLORS={value!r}"
        if plain_code != 0 or coloured_code != 0:
            failures.append(f"exit nonzero under {label}")
        if strip_ansi(coloured_out) != plain_out:
            failures.append(f"strip identity broken under {label}")
        if must_contain is not None and must_contain not in coloured_out:
            failures.append(f"custom palette field missing under {label}")
        if stderr_want is not None and coloured_err != stderr_want:
            failures.append(
                f"stderr under {label}: {coloured_err!r}, want {stderr_want!r}"
            )

    pty_passed = 0
    for args, env, want_coloured in PTY_ROWS:
        rows += 1
        out, err = run_on_pty(jqf, args, env=env)
        got = b"\x1b[" in out
        label = f"pty {' '.join(args)} env={env}"
        if got != want_coloured:
            failures.append(f"{label}: coloured={got}, want {want_coloured}")
        elif err:
            failures.append(f"{label}: unexpected stderr {err!r}")
        else:
            pty_passed += 1

    if failures:
        for failure in failures:
            print(f"colour-gate: FAIL: {failure}")
        print(
            f"colour-gate: rows={rows} pass={rows - len(failures)} "
            f"fail={len(failures)} RED"
        )
        return 1
    print(
        f"colour-gate: rows={rows} pass={rows} strip_identity=1 vacuity=1 "
        f"tty_rows={pty_passed} GREEN"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
