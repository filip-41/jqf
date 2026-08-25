#!/usr/bin/env python3
"""The peak-RSS proof for the streaming sequence entry point (PAB-14).

Three consumers run the SAME ~200 MB-output workload, each in its own
subprocess so `getrusage` peaks cannot contaminate each other:

  legacy     `run_many`      — the contiguous-buffer contract: the caller
                               holds the whole output (twice across the
                               ctypes buffer and the bytes copy)
  streaming  `run_many_streaming` consumed chunk-by-chunk, contents dropped
  feed       `Session.open_feed` push/poll loop, batches dropped

Verdict (printed, exit code carried): the streaming arm's peak must stay
within 2x of the feed twin's on this workload. The absolute numbers are the
receipt; the ratio is the regression tripwire.

Usage:  python3 bindings/python/tests/rss_streaming_proof.py
"""
import os
import resource
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
# ~200 MB total output. The per-record size is chosen so ONE record-drive batch (256 records or 256 KiB of input
# payload, whichever binds first — here 256 records x ~64 KB ≈ 16 MB) fits the limited feed twin's ceiling: a batch
# whose ENCODED output exceeds that cap is refused terminally, by the account law, so the twin's dials must admit at
# least one batch.
VALUES = 3200
VALUE_BYTES = 65_000
# The limited feed twin's dials: the retained-input/batch ceiling and the poll buffer a bounded host would offer. Both
# are the embedder's own choices; these are representative, not magic.
FEED_MEMORY_CEILING = 32 * 1024 * 1024
FEED_POLL_BUFFER = 256 * 1024


def peak_mb():
    """This process's ru_maxrss in MB. macOS reports BYTES, Linux KiB —
    getrusage's own historical inconsistency."""
    raw = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if sys.platform == "darwin":
        return raw / (1024 * 1024)
    return raw / 1024


def scenario(name):
    """Runs ONE consumer in this process and prints its own peak RSS."""
    sys.path.insert(0, os.path.join(HERE, ".."))
    import ctypes

    import jqf
    from jqf import _ffi

    program_text = f'"x" * {VALUE_BYTES}'
    data = " ".join(["0"] * VALUES).encode()
    ndjson = b"0\n" * VALUES
    published = 0

    if name == "feed-limited":
        # The honest feed twin: a host that BOUNDS its feed through `jqf_new_limited`, the way an embedder must — the
        # batch cap (the retained-input ceiling) is what keeps a feed's staging bounded, and the Python `Session`
        # deliberately exposes no limits dial.
        class Limits(ctypes.Structure):
            _fields_ = [
                ("max_output_bytes", ctypes.c_uint64),
                ("max_memory_bytes", ctypes.c_uint64),
                ("max_spill_bytes", ctypes.c_uint64),
                ("max_nesting_depth", ctypes.c_uint32),
                ("deadline_ms", ctypes.c_uint64),
                ("control_callback", ctypes.c_void_p),
                ("control_context", ctypes.c_void_p),
            ]

        limits = Limits(
            2**64 - 1, FEED_MEMORY_CEILING, 2**64 - 1, 2**32 - 1, 0, None, None
        )
        # This script is the ONE jqf_new_limited caller in-tree, so the declaration lives here (see _ffi.py): without
        # argtypes ctypes converts the byref()/None arguments by default conversion, and a header drift would corrupt
        # memory silently instead of failing.
        _ffi._lib.jqf_new_limited.restype = ctypes.c_int
        _ffi._lib.jqf_new_limited.argtypes = [
            ctypes.POINTER(Limits),
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_void_p),
        ]
        handle = ctypes.c_void_p()
        if _ffi._lib.jqf_new_limited(
            ctypes.byref(limits), None, ctypes.byref(handle)
        ) != 0:
            raise SystemExit("jqf_new_limited failed")
        try:
            pid = ctypes.c_uint32()
            prog = program_text.encode()
            if _ffi._lib.jqf_compile(handle, prog, len(prog), ctypes.byref(pid)) != 0:
                raise SystemExit("compile failed")
            fid = ctypes.c_uint32()
            if (
                _ffi._lib.jqf_feed_open(handle, pid, ctypes.c_int(0), ctypes.byref(fid))
                != 0
            ):
                raise SystemExit("feed open failed")
            buf = (ctypes.c_uint8 * len(ndjson)).from_buffer_copy(ndjson)
            if _ffi._lib.jqf_feed_push(handle, fid, buf, len(ndjson)) < 0:
                raise SystemExit("feed push failed")
            out = (ctypes.c_uint8 * FEED_POLL_BUFFER)()
            buf_size = FEED_POLL_BUFFER
            while True:
                written = _ffi._lib.jqf_feed_poll(handle, fid, out, buf_size)
                if written < 0:
                    raise SystemExit("feed poll failed")
                if written == 0:
                    break
                if written > buf_size:
                    # The documented convention: a required count larger than the offered buffer re-delivers the SAME
                    # batch on the next poll. A compliant host GROWS and re-polls; spinning on the small buffer would
                    # loop forever.
                    buf_size = written
                    out = (ctypes.c_uint8 * buf_size)()
                    continue
                published += written  # bytes consumed, contents dropped
        finally:
            _ffi._lib.jqf_free(handle)
    else:
        with jqf.Session() as session:
            compiled = session.compile(program_text)
            if name == "legacy":
                result = compiled.run_many(data)
                if result.failure is not None:
                    raise SystemExit(f"legacy run failed: {result.failure}")
                published = len(result.output)
            elif name == "streaming":
                for chunk in compiled.run_many_streaming(data):
                    published += len(chunk)
            elif name == "feed":
                feed = session.open_feed(compiled, profile="strict")
                feed.push(ndjson)
                while True:
                    batch = feed.poll()
                    if not batch:
                        break
                    published += len(batch)
                feed.close()
            else:
                raise SystemExit(f"unknown scenario {name}")
    print(f"scenario={name} published={published} rss_mb={peak_mb():.1f}")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--scenario":
        scenario(sys.argv[2])
        return
    peaks = {}
    for name in ("legacy", "streaming", "feed-limited"):
        out = subprocess.run(
            [sys.executable, __file__, "--scenario", name],
            capture_output=True, text=True, env=os.environ.copy(),
        )
        if out.returncode != 0:
            print(out.stdout)
            print(out.stderr)
            raise SystemExit(f"scenario {name} failed")
        for line in out.stdout.splitlines():
            if line.startswith(f"scenario={name}"):
                fields = dict(part.split("=") for part in line.split())
                peaks[name] = float(fields["rss_mb"])
                print(line, flush=True)
    legacy_mb = peaks["legacy"]
    stream_mb = peaks["streaming"]
    feed_mb = peaks["feed-limited"]
    ratio = stream_mb / feed_mb if feed_mb else float("inf")
    ok = ratio <= 2.0
    print(
        f"rss-streaming-proof: legacy={legacy_mb:.1f}MB "
        f"streaming={stream_mb:.1f}MB feed_twin={feed_mb:.1f}MB "
        f"streaming_over_feed={ratio:.2f}x limit=2.00x "
        f"{'PASS' if ok else 'FAIL'}"
    )
    raise SystemExit(0 if ok else 1)


if __name__ == "__main__":
    main()
