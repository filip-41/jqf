"""jqf — the SDK's structured diagnostics as a Python consumer.

The record stream is the first-class surface: a failed run gives you a
`Record` (code, kind, operand, payload, rendered text), not a string, and the
retained stream is pollable after the run. The stderr text is one renderer;
this is another.
"""
import ctypes
import json
import queue
import threading

from . import codes
from ._ffi import (
    JQF_DIAG_TEXT_KIND,
    JQF_DIAG_TEXT_OPERAND,
    JQF_DIAG_TEXT_PAYLOAD,
    JQF_STREAM_CANCEL,
    JQF_STREAM_CHUNK_BYTES,
    JQF_STREAM_CHUNK_FN,
    JQF_STREAM_CONTINUE,
    _lib,
)

__all__ = [
    "run", "run_many", "Session", "Program", "Feed", "Record", "JqfError",
    "JqfTruncatedError", "codes",
]

# `jqf_run`/`jqf_run_sequence` follow the `snprintf` convention: a returned byte count greater than the buffer offered
# is the EXACT size a re-call needs, not a truncated success. A handful of growing re-calls absorbs that without looping
# forever on a program whose output can never stabilize (e.g. one seeded by `now`/`env` that keeps growing between
# calls).
_MAX_GROW_ATTEMPTS = 4

# The growth loop's seed ceiling: a huge input must not speculatively stage sixteen times its own size before the first
# output byte is known. Past this bound the first attempt takes the floor-sized buffer and the ABI-reported required
# size drives the one re-call that sizes it exactly.
_MAX_SEED_OUT_CAP = 64 << 20


def _diag_text(handle_ptr, index, field):
    """One record's text field through the length-carrying getter, under the
    ABI's `snprintf` convention (a required size larger than the offered
    buffer means the exact size a re-call needs).

    The `jqf_diag_get` channel is NUL-terminated: a text value containing a
    literal NUL byte cannot travel as a C string, so the pointer arrives
    NULL even though the record carries text. The getter is length-carrying,
    so it recovers those bytes exactly; `-1` means the field is genuinely
    absent and `None` is the right answer.
    """
    required = _lib.jqf_diag_get_text(handle_ptr, index, field, None, 0)
    if required < 0:
        return None
    buf = (ctypes.c_uint8 * required)()
    written = _lib.jqf_diag_get_text(handle_ptr, index, field, buf, len(buf))
    if written < 0:
        return None
    return bytes(buf[:written]).decode()


# The render templates that depend on NO record field: hoisted to module level so a rendered record costs one dict
# lookup instead of a fresh dict build. The kind/operand/payload-dependent families are rendered by `Record.render`'s
# branches.
_STATIC_TEMPLATES = {
    codes.Code.RAISE_SLICE_INDICES: "Array/string slice indices must be integers",
    codes.Code.RAISE_DIVIDE_BY_ZERO: "division by zero",
    codes.Code.RAISE_NONTERMINATING: "non-terminating decimal division",
    codes.Code.PRECISION_BOUNDARY: "exact-to-binary64 contagion",
}


class Record:
    """One structured diagnostic record (the stable binding contract).

    The record carries its locators (`step_index`, `input_ordinal`,
    `byte_offset` — `None` when absent) and the halt exit status
    (`halt_status`, `None` for every non-halt record; `0` for a bare `halt`
    and the requested status for `halt_error(n)`), so a handler can act on a
    controlled halt the way the CLI does.
    """

    def __init__(self, code, revision, class_, severity, catchable, caught,
                 step_index, input_ordinal, byte_offset, halt_status,
                 kind, operand, payload):
        self.code = code
        self.revision = revision
        self.class_ = class_
        self.severity = severity
        self.catchable = bool(catchable)
        self.caught = None if caught == 0xFFFFFFFF else caught
        self.step_index = None if step_index == 0xFFFFFFFF else step_index
        self.input_ordinal = None if input_ordinal == 0xFFFFFFFFFFFFFFFF else input_ordinal
        self.byte_offset = None if byte_offset == 0xFFFFFFFFFFFFFFFF else byte_offset
        self.halt_status = None if halt_status < 0 else halt_status
        self.kind = kind
        self.operand = operand
        self.payload = payload

    @property
    def name(self):
        return codes.NAME.get(self.code, "<unknown>")

    def render(self):
        """The jq-shaped text for this record (pure: template + fields)."""
        static = _STATIC_TEMPLATES.get(self.code)
        if static is not None:
            return static
        kind = self.kind or "value"
        operand = self.operand or ""
        paren = f" ({operand})" if operand else ""
        if self.code == codes.Code.RAISE_ITERATE:
            return f"Cannot iterate over {kind}{paren}"
        if self.code == codes.Code.RAISE_INDEX:
            return f"Cannot index {kind} with {operand}" if operand else f"Cannot index {kind}"
        if self.code == codes.Code.RAISE_OBJECT_KEY:
            return f"Cannot use {kind}{paren} as object key"
        if self.code == codes.Code.RAISE_NO_LENGTH:
            return f"{kind}{paren} has no length"
        if self.code == codes.Code.RAISE_NO_KEYS:
            return f"{kind}{paren} has no keys"
        if self.code == codes.Code.RAISE_PROGRAM:
            return self.payload or "error"
        if self.code == codes.Code.ROUTE_SELECTED:
            return f"route: {operand}"
        if self.code == codes.Code.COST_SNAPSHOT:
            return f"cost: {operand}"
        # A code with no template (a v2 reservation, or MACHINE_SETUP's free-form setup-failure text) still has
        # SOMETHING to say when a payload was recorded — that beats the bare registry name.
        return self.payload or f"{self.name} (no template in v1)"

    def __repr__(self):
        return f"<jqf.Record {self.name} code={self.code} class={self.class_} caught={self.caught}>"


class JqfError(RuntimeError):
    """A failed run: the structured record is the payload, not a string."""

    def __init__(self, record):
        self.record = record
        super().__init__(record.render())


class JqfTruncatedError(RuntimeError):
    """The ABI kept reporting a larger required output size than we could
    satisfy within `_MAX_GROW_ATTEMPTS` growing re-calls — the output's real
    size never stabilized (a non-deterministic program), so returning a
    silently truncated buffer would be worse than raising."""


def _undiagnosed_failure(written):
    """The ABI's own `written < 0` failure sentinel, standing in for a
    diagnostic record when the retained stream has none to explain it.

    `Diagnostics::record_setup_failure` (Rust side) now records a
    `MACHINE_SETUP` record for every early setup failure — a program that
    fails to compile, an invalid codec — and `execute`'s own recording
    already covers every uncaught pipeline failure, so this is normally
    dead code. It exists for the one case Rust-side recording cannot reach:
    a panic `jqf_run`/`jqf_run_sequence` caught at the FFI boundary, which
    unwinds past every diagnostic-recording call site. Either way, `written
    < 0` must not need a diagnostic record to agree with it before a caller
    raises: the ABI's sentinel is authoritative on its own.
    """
    return Record(
        code=0, revision=0, class_="M", severity="E", catchable=0, caught=0xFFFFFFFF,
        step_index=0xFFFFFFFF, input_ordinal=0xFFFFFFFFFFFFFFFF,
        byte_offset=0xFFFFFFFFFFFFFFFF, halt_status=-1,
        kind=None, operand=None,
        payload=f"the run failed (code {written}) with no diagnostic record",
    )


class RunResult:
    """One run's outcome.

    `output` is the published bytes; `diagnostics` is the run's retained
    record stream (oldest first); `failure` is the terminal `Record` when
    the run failed, else `None`. `diag_dropped` is how many records
    overflowed the ABI's capped diagnostic buffer THIS run — nonzero means
    the stream was truncated, so records exist that `diagnostics` cannot
    show. `errors` holds each per-value error TEXT the run retained, in
    input order: a sequence continues past a per-value error, so those
    texts ride beside a successful `output`.
    """

    def __init__(self, output, diagnostics, failure, diag_dropped=0,
                 errors=()):
        self.output = output
        self.diagnostics = diagnostics
        self.failure = failure
        self.diag_dropped = diag_dropped
        self.errors = list(errors)

    @property
    def ok(self):
        return self.failure is None


def _dead_session_record(subject):
    """A session whose `close()` ran has no live ABI to ask, so its
    consumers' failures are synthesized on this side, in the same shape the
    ABI's dead-id setup record would take. `subject` names what died with
    the session: the session itself, a program, or a feed."""
    return Record(
        code=0, revision=0, class_="M", severity="E", catchable=0,
        caught=0xFFFFFFFF, step_index=0xFFFFFFFF,
        input_ordinal=0xFFFFFFFFFFFFFFFF, byte_offset=0xFFFFFFFFFFFFFFFF,
        halt_status=-1, kind=None, operand=None,
        payload=f"{subject} session was closed",
    )


def _abi_locked(method):
    """Serializes one `_Handle` method under the session lock: the Rust
    handle is one-thread-at-a-time and ctypes releases the GIL during a
    call, so two Python threads sharing one Session would race it.

    The lock is also the close fence. `Session.close` frees the Rust handle
    under this same lock and marks it dead there, so a caller that resolved
    the handle BEFORE the free — passing its pre-lock liveness check — finds
    the mark when it acquires the lock here and raises the defined
    dead-session `JqfError` instead of passing a freed pointer into the
    FFI. The check-then-lock interleave never reaches ctypes.
    """
    def wrapper(self, *args, **kwargs):
        with self._lock:
            if self._closed:
                raise JqfError(_dead_session_record("this"))
            return method(self, *args, **kwargs)
    wrapper.__name__ = method.__name__
    wrapper.__doc__ = method.__doc__
    return wrapper


class _Handle:
    def __init__(self):
        self._ptr = ctypes.c_void_p()
        if _lib.jqf_new(ctypes.byref(self._ptr)) != 0:
            # A failed `jqf_new` leaves NO handle to read the diagnostic stream from, so the cause cannot be fetched
            # through the ABI (that is why the failure message cannot carry a record). Name the construction stage and
            # point the host at the engine's own stderr, where a construction panic is reported before the ABI boundary.
            raise RuntimeError(
                "jqf_new failed: engine construction failed (see the host's "
                "stderr for the construction panic)"
            )
        # The session path's reusable output buffer: a resident host runs many records against one handle, so the
        # per-record 64 KiB ctypes allocation is a tax this path exists to remove (measured ~1 us per call). Grown on
        # demand, kept for the session's life.
        self._out_buf = None
        # The Rust handle is one-thread-at-a-time, and ctypes releases the GIL for the duration of a call, so two Python
        # threads sharing one Session would race the handle. A re-entrant lock serializes every ABI call on this side; a
        # host that wants real parallelism uses one Session per thread. `close()` sets `_closed` and drops `_ptr` under
        # that same lock, so a caller holding a stale handle reference from before the free refuses at the lock instead
        # of touching the freed C session.
        self._lock = threading.RLock()
        self._closed = False

    # --- The compiled-program surface --------------------------------------
    # A program is a handle-LOCAL u32 id. Every misuse is a defined -1 with a recorded setup diagnostic (run entry
    # points) or a plain -1 (free), so the binding never touches freed memory and surfaces misuse as ordinary
    # exceptions.

    @_abi_locked
    def compile(self, program):
        pid = ctypes.c_uint32()
        program_bytes = program.encode()
        rc = _lib.jqf_compile(
            self._ptr, program_bytes, len(program_bytes), ctypes.byref(pid)
        )
        if rc != 0:
            diagnostics, _ = self._collect_diags()
            # The SAME failure predicate every other entry point uses: the LAST `severity == "E"` record with `caught is
            # None`, with the `written < 0` sentinel fallback. Compile-time records are never caught, so the `caught is
            # None` test is trivially true today — but taking the FIRST record with no such test would be a second
            # spelling of the failure predicate, and the two can drift.
            raise JqfError(self._select_failure(-1, diagnostics))
        return pid.value

    @_abi_locked
    def compile_args(self, program, bindings):
        """Compiles `program` with each `bindings` name bound to a JSON value:
        the values are parsed by the engine's own reader, never
        spliced into the source, and `$ARGS` resolves. Raises `JqfError` when
        the program or any binding value cannot be parsed."""
        pid = ctypes.c_uint32()
        program_bytes = program.encode()
        names = []
        values = []
        lengths = []
        for name, value in bindings.items():
            try:
                encoded = json.dumps(value).encode()
            except (TypeError, ValueError) as exc:
                raise ValueError(
                    f"binding {name!r} is not JSON-encodable: {exc}"
                ) from exc
            names.append(ctypes.c_char_p(name.encode()))
            values.append(encoded)
            lengths.append(len(encoded))
        name_array = (ctypes.c_char_p * len(names))(*names)
        value_array = (ctypes.POINTER(ctypes.c_uint8) * len(values))(
            *[ctypes.cast(v, ctypes.POINTER(ctypes.c_uint8)) for v in values]
        )
        length_array = (ctypes.c_size_t * len(lengths))(*lengths)
        rc = _lib.jqf_compile_args(
            self._ptr, program_bytes, len(program_bytes), len(names),
            name_array, value_array, length_array, ctypes.byref(pid),
        )
        if rc != 0:
            diagnostics, _ = self._collect_diags()
            raise JqfError(self._select_failure(-1, diagnostics))
        return pid.value

    @_abi_locked
    def free_program(self, program_id):
        return _lib.jqf_program_free(self._ptr, program_id)

    @_abi_locked
    def run_compiled(self, program_id, data):
        input_buf = (ctypes.c_uint8 * len(data)).from_buffer_copy(data)

        def attempt(out_cap):
            # One buffer for the session, resized only when the published output outgrows it — never one allocation per
            # record.
            if self._out_buf is None or len(self._out_buf) < out_cap:
                self._out_buf = (ctypes.c_uint8 * out_cap)()
            written = _lib.jqf_run_compiled(
                self._ptr, program_id, input_buf, len(data), self._out_buf,
                len(self._out_buf),
            )
            return written, self._out_buf

        written, out_buf = self._run_with_growth(attempt, len(data))
        return self._finish_compiled(written, out_buf)

    @_abi_locked
    def run_sequence_compiled(self, program_id, data):
        input_buf = (ctypes.c_uint8 * len(data)).from_buffer_copy(data)

        def attempt(out_cap):
            if self._out_buf is None or len(self._out_buf) < out_cap:
                self._out_buf = (ctypes.c_uint8 * out_cap)()
            written = _lib.jqf_run_sequence_compiled(
                self._ptr, program_id, input_buf, len(data), self._out_buf,
                len(self._out_buf),
            )
            return written, self._out_buf

        written, out_buf = self._run_with_growth(attempt, len(data))
        return self._finish_compiled(written, out_buf)

    # --- The resident feed ---------------------------------------------------
    # A feed is a handle-LOCAL u32 id, like a program. `open_feed` validates the program id at open; `feed_push` appends
    # input; `feed_poll` pulls ONE bounded batch with the snprintf growth convention (re-polling re-delivers the SAME
    # batch); `finish_feed` marks end of stream so the held partial record is delivered as the FINAL record under the
    # profile's own tail law; `close_feed` releases the retained input.

    @_abi_locked
    def open_feed(self, program_id, profile):
        fid = ctypes.c_uint32()
        rc = _lib.jqf_feed_open(self._ptr, program_id, profile, ctypes.byref(fid))
        if rc != 0:
            diagnostics, _ = self._collect_diags()
            raise JqfError(
                self._select_failure(-1, diagnostics) or _undiagnosed_failure(-1)
            )
        return fid.value

    @_abi_locked
    def feed_push(self, feed_id, data):
        input_buf = (ctypes.c_uint8 * len(data)).from_buffer_copy(data)
        retained = _lib.jqf_feed_push(
            self._ptr, feed_id, input_buf, len(data)
        )
        if retained < 0:
            diagnostics, _ = self._collect_diags()
            raise JqfError(
                self._select_failure(retained, diagnostics)
                or _undiagnosed_failure(retained)
            )
        return retained

    @_abi_locked
    def feed_poll(self, feed_id):
        """Polls ONE bounded batch, returning `(written, out_buf)`. The
        caller (the `Feed` wrapper) reads the diagnostic stream — success or
        failure — and raises, because a RECOVERING feed's ordered issues are
        observable on a SUCCESSFUL poll."""
        def attempt(out_cap):
            if self._out_buf is None or len(self._out_buf) < out_cap:
                self._out_buf = (ctypes.c_uint8 * out_cap)()
            written = _lib.jqf_feed_poll(
                self._ptr, feed_id, self._out_buf, len(self._out_buf)
            )
            return written, self._out_buf

        return self._run_with_growth(attempt, 0)

    @_abi_locked
    def finish_feed(self, feed_id):
        rc = _lib.jqf_feed_finish(self._ptr, feed_id)
        if rc != 0:
            diagnostics, _dropped = self._collect_diags()
            raise JqfError(
                self._select_failure(-1, diagnostics) or _undiagnosed_failure(-1)
            )

    @_abi_locked
    def close_feed(self, feed_id):
        return _lib.jqf_feed_close(self._ptr, feed_id)

    def _finish_compiled(self, written, out_buf):
        """The compiled path's finish: the record stream is read ONLY when
        the run failed.

        A successful run's retained records are informational (route, cost);
        copying them costs a jqf_diag_get round-trip with fresh C strings PER
        RECORD — the exact per-call tax the compiled path exists to remove
        (measured: ~6.7 us of the one-shot path's per-call cost). The ABI's
        `written` sentinel is authoritative: `written < 0` is exactly "the
        run failed", so skipping the stream on success loses no failure
        signal. On failure the stream is read exactly as the one-shot path
        reads it, and the explaining record becomes the raised JqfError.
        """
        output = (
            ctypes.string_at(out_buf, written) if written >= 0 else b""
        )
        if written < 0:
            diagnostics, dropped = self._collect_diags()
            failure = self._select_failure(written, diagnostics)
            return RunResult(
                output, diagnostics, failure,
                diag_dropped=dropped, errors=self._collect_run_errors(),
            )
        # The success path skips the per-record read-back by design (see above), but neither truncation nor per-value
        # errors may be silent: both scalars are one FFI call each (and zero when there is nothing to report).
        return RunResult(
            output, [], None,
            diag_dropped=_lib.jqf_diag_dropped(self._ptr),
            errors=self._collect_run_errors(),
        )

    @_abi_locked
    def run_sequence(self, program, data):
        input_buf = (ctypes.c_uint8 * len(data)).from_buffer_copy(data)

        def attempt(out_cap):
            out_buf = (ctypes.c_uint8 * out_cap)()
            program_bytes = program.encode()
            written = _lib.jqf_run_sequence(
                self._ptr, program_bytes, len(program_bytes),
                input_buf, len(data), out_buf, out_cap,
            )
            return written, out_buf

        written, out_buf = self._run_with_growth(attempt, len(data))
        return self._finish(written, out_buf)

    @staticmethod
    def _run_with_growth(attempt, data_len):
        """Calls `attempt(out_cap)` — which must return `(written, out_buf)`
        from one `jqf_run`/`jqf_run_sequence` call — growing `out_cap` to the
        ABI-reported required size and re-calling until the output fits or
        the retry budget is exhausted. `written > out_cap` is NOT a truncated
        success; it is the exact size the next call needs.
        """
        out_cap = min(max(65536, data_len * 16 + 4096), _MAX_SEED_OUT_CAP)
        for _ in range(_MAX_GROW_ATTEMPTS):
            written, out_buf = attempt(out_cap)
            if written < 0 or written <= out_cap:
                return written, out_buf
            out_cap = written
        raise JqfTruncatedError(
            f"output did not fit after {_MAX_GROW_ATTEMPTS} growing re-calls "
            f"(last required size: {out_cap} bytes)"
        )

    def _finish(self, written, out_buf):
        # One memcpy, not a Python int list: `bytes(ctypes_array[:n])` materializes every byte as an int object first —
        # on a 200 MB output that is a ~2 GB transient, which swamped the staged-vs- streamed RSS comparison this
        # binding exists to serve.
        output = ctypes.string_at(out_buf, written) if written >= 0 else b""
        diagnostics, dropped = self._collect_diags()
        return RunResult(
            output, diagnostics, self._select_failure(written, diagnostics),
            diag_dropped=dropped, errors=self._collect_run_errors(),
        )

    @staticmethod
    def _select_failure(written, diagnostics):
        """This run's failure, or `None` on success — from either of TWO
        independent signals, neither needing the other to agree:

        1. A retained record that is an uncaught error (`severity == "E"`
           and `caught is None` — not `catchable`, which means "this
           error's CLASS is catch-eligible in principle", almost always
           true whether or not THIS occurrence was actually caught).
        2. The ABI's own `written < 0` sentinel. Diagnostics normally
           explain every failure this can also catch, but `written < 0`
           must still raise even when the retained stream has nothing — a
           run with zero diagnostic records would otherwise report
           `ok=True` with silently empty output.
        """
        failure = None
        for record in diagnostics:
            if record.caught is not None:
                continue
            # A halt is the run's terminal failure even though RAISE_HALT is severity Warning (jq's own weight for a
            # controlled exit): a predicate that read only severity would lose both the status and the message.
            if record.severity == "E" or record.code == codes.Code.RAISE_HALT:
                failure = record
        if failure is None and written < 0:
            failure = _undiagnosed_failure(written)
        return failure

    def _collect_diags(self):
        """Reads the handle's retained diagnostic stream: `(records,
        dropped)` — the records oldest first, plus how many overflowed the
        ABI's capped buffer (4096 records, evicting the oldest
        non-terminal first). Every entry point clears the buffer and its
        counter before it runs, so a nonzero count belongs to the very
        operation whose stream this is: records were emitted that this
        list cannot show.

        Callers must hold the session lock: this is a multi-call ABI
        sequence (jqf_diag_count + jqf_diag_get×N + jqf_diag_free_text×3N)
        against a one-thread-at-a-time handle.
        """
        diagnostics = []

        def read_text(index, pointer, field):
            # A NULL from the C-string channel is either an absent field or a NUL-bearing text the channel cannot carry;
            # the length- carrying getter distinguishes the two and recovers the bytes.
            if pointer:
                return pointer.decode()
            return _diag_text(self._ptr, index, field)

        for i in range(_lib.jqf_diag_count(self._ptr)):
            code = ctypes.c_uint16()
            revision = ctypes.c_uint16()
            class_ = ctypes.c_char()
            severity = ctypes.c_char()
            catchable = ctypes.c_uint8()
            caught = ctypes.c_uint32()
            step_index = ctypes.c_uint32()
            input_ordinal = ctypes.c_uint64()
            byte_offset = ctypes.c_uint64()
            halt_status = ctypes.c_int32()
            kind = ctypes.c_char_p()
            operand = ctypes.c_char_p()
            payload = ctypes.c_char_p()
            rc = _lib.jqf_diag_get(
                self._ptr, i, ctypes.byref(code), ctypes.byref(revision),
                ctypes.byref(class_), ctypes.byref(severity),
                ctypes.byref(catchable), ctypes.byref(caught),
                ctypes.byref(step_index), ctypes.byref(input_ordinal),
                ctypes.byref(byte_offset), ctypes.byref(halt_status),
                ctypes.byref(kind), ctypes.byref(operand), ctypes.byref(payload),
            )
            if rc != 0:
                break
            diagnostics.append(Record(
                code.value, revision.value,
                chr(class_.value[0]) if class_.value else "?",
                chr(severity.value[0]) if severity.value else "?",
                catchable.value, caught.value,
                step_index.value, input_ordinal.value, byte_offset.value,
                halt_status.value,
                read_text(i, kind.value, JQF_DIAG_TEXT_KIND),
                read_text(i, operand.value, JQF_DIAG_TEXT_OPERAND),
                read_text(i, payload.value, JQF_DIAG_TEXT_PAYLOAD),
            ))
            for slot in (kind, operand, payload):
                if slot.value:
                    _lib.jqf_diag_free_text(slot)
        return diagnostics, _lib.jqf_diag_dropped(self._ptr)

    def _collect_run_errors(self):
        """The LAST run's retained per-value error texts, through the
        length-carrying getters (a NUL byte inside an error text survives).
        Cleared per run exactly like the diagnostic records, so an empty
        list means the run reported none. Callers must hold the session
        lock, exactly as for [`_collect_diags`].
        """
        texts = []
        for index in range(_lib.jqf_run_errors_count(self._ptr)):
            required = _lib.jqf_run_error_get(self._ptr, index, None, 0)
            if required < 0:
                break
            buf = (ctypes.c_uint8 * required)()
            written = _lib.jqf_run_error_get(self._ptr, index, buf, len(buf))
            if written < 0:
                break
            texts.append(bytes(buf[:written]).decode())
        return texts

    @_abi_locked
    def run(self, program, data):
        input_buf = (ctypes.c_uint8 * len(data)).from_buffer_copy(data)

        def attempt(out_cap):
            out_buf = (ctypes.c_uint8 * out_cap)()
            program_bytes = program.encode()
            written = _lib.jqf_run(
                self._ptr,
                program_bytes,
                len(program_bytes),
                input_buf,
                len(data),
                out_buf,
                out_cap,
            )
            return written, out_buf

        written, out_buf = self._run_with_growth(attempt, len(data))
        return self._finish(written, out_buf)

    @_abi_locked
    def close(self):
        if self._closed:
            return
        _lib.jqf_free(self._ptr)
        # The free, the death mark, and the pointer drop are one atomic step under the lock: a caller that resolved this
        # handle before the free acquires the lock afterwards and refuses on `_closed` in the `@_abi_locked` wrapper —
        # it never reaches ctypes with the freed address.
        self._ptr = None
        self._closed = True


def run(program, data=b"null"):
    """Runs `program` over ONE document `data` (bytes) and returns a
    `RunResult` whose `diagnostics` is the retained record stream and whose
    `failure` is the terminal `Record` when the run failed. Raises `JqfError`
    with the record when it did.
    """
    return _run(program, data, single=True)


def run_many(program, data):
    """Runs `program` over a MULTI-VALUE input stream (adjacent JSON values,
    like jq's stdin). Same result shape as `run`.
    """
    return _run(program, data, single=False)


def _run(program, data, single):
    handle = _Handle()
    try:
        if single:
            result = handle.run(program, data)
        else:
            result = handle.run_sequence(program, data)
    finally:
        handle.close()
    if result.failure is not None:
        raise JqfError(result.failure)
    return result


class Session:
    """A resident engine handle: one codec context, one program table.

    The compiled-program embedding path: compile programs once with `compile` and run
    them many times — per-record calls that skip the parse/lower/classify
    half entirely (the per-call cold-start tax is 11-12x the batched
    per-record cost; the compiled path is within ~2x of the batched figure).
    A program is an id into THIS session's table: it dies with the session
    (`close`), is never reused, and every misuse is a defined `JqfError`,
    never a segfault.

    The record stream is per-run exactly as in the one-shot `run`: a run
    clears the stream, and `diagnostics` on the result reflects that run
    only. `compile` also clears the stream (a failed compile retains exactly
    its setup record).
    """

    def __init__(self):
        self._handle = _Handle()

    def _dead_session_failure(self):
        return _dead_session_record("this")

    def compile(self, program):
        """Compiles `program` once. Raises `JqfError` with the setup record
        when it cannot compile."""
        handle = self._handle
        if handle is None:
            raise JqfError(self._dead_session_failure())
        program_id = handle.compile(program)
        return Program(self, program_id, program)

    def compile_args(self, program, **bindings):
        """Compiles `program` with host data bound as `$name` constants: each
        binding value is JSON-encoded and parsed by the engine's
        own reader, so data reaches the program as a VALUE — never spliced
        into the source. `$ARGS` resolves to `{"positional": [], "named":
        {…}}`, exactly the CLI's shape. Raises `JqfError` with the setup
        record when the program or a binding value cannot be parsed."""
        handle = self._handle
        if handle is None:
            raise JqfError(self._dead_session_failure())
        program_id = handle.compile_args(program, bindings)
        return Program(self, program_id, program)

    def open_feed(self, program, profile="strict"):
        """Opens one resident record stream over `program`, a
        [`Program`] on THIS session: push input in pieces, pull bounded
        batches of output — the embedded analogue of the record route.

        `profile` selects the framing profile: `"strict"` (the first framing
        or payload fault is terminal) or `"recovering"` (faults become
        ordered issues and the stream continues). Returns a [`Feed`] whose
        `push`/`poll` produce byte-identical output to the whole-input record
        route over the same records. A feed dies with its session; a feed id
        is never reused, so every misuse is a defined `JqfError`.
        """
        handle = self._handle
        if handle is None:
            raise JqfError(self._dead_session_failure())
        profile_id = Feed._PROFILES.get(profile)
        if profile_id is None:
            raise ValueError(
                f"unknown feed profile {profile!r} (strict or recovering)"
            )
        feed_id = handle.open_feed(program._program_id, profile_id)
        return Feed(self, feed_id, program, profile)

    def close(self):
        """Releases the session and every program and feed compiled on it.
        Safe to call twice; a [`Program`] or [`Feed`] from this session is
        dead afterwards and its use raises `JqfError` (a defined outcome,
        never a segfault)."""
        handle = self._handle
        if handle is not None:
            # The free AND the dead-reference swap are atomic under the session lock, and `handle.close()` marks the
            # handle dead under that same lock. A caller that resolved `self._handle` before this free — passing its
            # pre-lock liveness check — acquires the lock afterwards and refuses on the mark inside every locked ABI
            # entry, so a locked sequence can never start against a handle this thread freed. The RLock makes this
            # re-entrant.
            with handle._lock:
                handle.close()
                self._handle = None

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()


class Program:
    """One program compiled onto a [`Session`].

    `run` / `run_many` are the compiled twins of `jqf.run` / `jqf.run_many`
    with the same result and failure contract; they raise `JqfError` on
    failure exactly like the one-shot functions. A `Program` used after its
    session closed, after `free`, or from a session it does not belong to
    raises `JqfError` — the ABI defines those outcomes; they are not
    undefined behavior.
    """

    def __init__(self, session, program_id, source):
        self._session = session
        self._program_id = program_id
        self.source = source

    def _dead_session_failure(self):
        return _dead_session_record("this program's")

    def _dead_program_failure(self):
        # `jqf_program_free` never touches the diagnostic stream, so a dead id's cause is synthesized here in the same
        # shape its live-id setup record would take.
        return Record(
            code=0, revision=0, class_="M", severity="E", catchable=0,
            caught=0xFFFFFFFF, step_index=0xFFFFFFFF,
            input_ordinal=0xFFFFFFFFFFFFFFFF, byte_offset=0xFFFFFFFFFFFFFFFF,
            halt_status=-1, kind=None, operand=None,
            payload="this program was already freed",
        )

    def _run(self, name, data):
        # The bound method is looked up AFTER the liveness check: resolving `self._session._handle.run_compiled` on a
        # closed session would itself raise AttributeError, not the defined JqfError. A close that lands between this
        # check and the locked call is the `@_abi_locked` wrapper's problem: it re-checks `_closed` under the lock and
        # raises the defined dead-session JqfError before touching ctypes.
        handle = self._session._handle
        if handle is None:
            raise JqfError(self._dead_session_failure())
        result = getattr(handle, name)(self._program_id, data)
        if result.failure is not None:
            raise JqfError(result.failure)
        return result

    def run(self, data=b"null"):
        """Runs this program over ONE document `data` (bytes)."""
        return self._run("run_compiled", data)

    def run_many(self, data):
        """Runs this program over a MULTI-VALUE input stream (adjacent JSON
        values, like jq's stdin)."""
        return self._run("run_sequence_compiled", data)

    def run_many_streaming(self, data, max_pending_chunks=8):
        """Runs this program over a MULTI-VALUE input stream and yields the
        output as an iterator of bounded byte chunks — the streaming twin of
        [`run_many`][jqf.Program.run_many].

        The chunks cross the FFI through `jqf_run_sequence_streaming`'s
        consumer callback; concatenating them reproduces `run_many`'s output
        byte for byte. A chunk is at most `JQF_STREAM_CHUNK_BYTES` (256 KiB)
        except when one encoder write exceeds it, and chunks are FLOW units:
        one huge value may straddle several. Because nothing stages the
        whole output, peak memory stays near the input plus a handful of
        chunks where `run_many` holds the entire result (the measured gap on
        a ~200 MB output was 608 MB staged against ~30 MB streamed).

        Backpressure is real: at most `max_pending_chunks` undelivered
        chunks exist, so a slow consumer pauses the run itself. Abandoning
        the iterator (closing it, breaking out) cancels the run cleanly at
        the next chunk boundary.

        The failure contract matches `run_many`: chunks already yielded are
        the run's published prefix, and a failed run raises `JqfError` with
        the structured record once the prefix has been drained. Uses the
        program TEXT (recompiled per call, like the one-shot path): the
        compile is milliseconds against megabytes of output.
        """
        handle = self._session._handle
        if handle is None:
            raise JqfError(self._dead_session_failure())
        return _stream_chunks(handle, self.source, data, max_pending_chunks)

    def free(self):
        """Releases this program's compiled arena and ledger residency now.
        A later `run`/`run_many`/`free` on the same id raises `JqfError` —
        the ABI defines a dead id as a failure, never undefined behavior."""
        handle = self._session._handle
        if handle is None:
            raise JqfError(self._dead_session_failure())
        if handle.free_program(self._program_id) != 0:
            raise JqfError(self._dead_program_failure())


def _stream_chunks(handle, program, data, max_pending_chunks):
    """The generator behind [`Program.run_many_streaming`][jqf.Program.run_many_streaming].

    Drives `jqf_run_sequence_streaming` on a worker thread (the FFI call is synchronous; yielding must interleave),
    copies each callback's bytes at the boundary the ABI guarantees them valid, and hands them to the consumer through
    a bounded queue so a slow consumer pauses the RUN rather than growing an unbounded backlog.
    """
    program_bytes = program.encode()
    input_bytes = bytes(data)
    chunks = queue.Queue(maxsize=max_pending_chunks)
    cancel = threading.Event()
    done = object()  # sentinel: the worker finished

    outcome = {}

    def on_chunk(_context, pointer, size):
        # Runs on the worker thread INSIDE the FFI call. The bytes are valid only for this call (the ABI contract): copy
        # first. A full queue blocks here, which is exactly the backpressure law — the engine's cooperative checkpoints
        # pause with it.
        if cancel.is_set():
            return JQF_STREAM_CANCEL
        payload = ctypes.string_at(pointer, size)
        chunks.put(payload)
        return JQF_STREAM_CONTINUE

    def work():
        callback = JQF_STREAM_CHUNK_FN(on_chunk)
        input_buf = (ctypes.c_uint8 * len(input_bytes)).from_buffer_copy(input_bytes)
        try:
            # The whole call AND its diagnostics read-back are one locked sequence: the handle is one-thread-at-a-time
            # and the record stream must not be cleared under us mid-read. This worker can hold the handle reference it
            # was captured with across a concurrent `Session.close`, so re-check the death mark after acquiring the lock
            # — the same fence every `@_abi_locked` entry applies — and raise the defined dead-session failure instead
            # of passing the freed pointer into ctypes.
            with handle._lock:
                if handle._closed:
                    raise JqfError(_dead_session_record("this program's"))
                written = _lib.jqf_run_sequence_streaming(
                    handle._ptr,
                    program_bytes,
                    len(program_bytes),
                    input_buf,
                    len(input_bytes),
                    callback,
                    None,
                )
                records, _ = handle._collect_diags()
            outcome["written"] = written
            outcome["diagnostics"] = records
        except BaseException as exc:  # noqa: BLE001 - re-raised on consumer side
            outcome["error"] = exc
        finally:
            chunks.put(done)

    worker = threading.Thread(target=work, name="jqf-stream", daemon=True)
    worker.start()
    try:
        while True:
            chunk = chunks.get()
            if chunk is done:
                break
            yield chunk
        error = outcome.get("error")
        if error is not None:
            raise error
        written = outcome.get("written", -1)
        if written < 0:
            diagnostics = outcome.get("diagnostics", [])
            failure = handle._select_failure(written, diagnostics)
            raise JqfError(failure or _undiagnosed_failure(written))
    finally:
        # Abandonment (close/break) or exhaustion both land here: stop the run at the next chunk boundary, unstick a
        # worker blocked on a full queue by draining it, and wait for the FFI call to return.
        cancel.set()
        while True:
            try:
                chunks.get_nowait()
            except queue.Empty:
                break
        worker.join()


class Feed:
    """One resident record stream on a [`Session`] (pull-buffered).

    Push input in pieces with `push` — a record is complete only after its physical terminator, so everything after
    the last line feed is HELD until more bytes arrive — and pull ONE bounded batch of output per `poll`. The batch
    bound is the record route's own `RECORD_BATCH_TARGET_BYTES`, and the output is byte-identical to the whole-input
    record route over the same records: one `poll` per batch instead of one `run_compiled` per record.

    The profile is selected at open (`"strict"` faults are terminal; `"recovering"` faults become ordered issues and
    the stream continues). A strict fault raises `JqfError` with the failure record retained; a feed dies with its
    session and its `push`/`poll`/`finish`/`close` after that are defined `JqfError`s, never a segfault.
    """

    # The c_int the ABI maps to NdjsonProfile::Strict / Recovering.
    _PROFILES = {"strict": 0, "recovering": 1}

    def __init__(self, session, feed_id, program, profile):
        self._session = session
        self._feed_id = feed_id
        self.program = program
        self.profile = profile

    def _dead_session_failure(self):
        return _dead_session_record("this feed's")

    def _dead_feed_failure(self):
        # `jqf_feed_close` never touches the diagnostic stream (the `jqf_program_free` law), so a dead id's cause is
        # synthesized here.
        return Record(
            code=0, revision=0, class_="M", severity="E", catchable=0,
            caught=0xFFFFFFFF, step_index=0xFFFFFFFF,
            input_ordinal=0xFFFFFFFFFFFFFFFF, byte_offset=0xFFFFFFFFFFFFFFFF,
            halt_status=-1, kind=None, operand=None,
            payload="this feed was already closed",
        )

    def _handle(self):
        handle = self._session._handle
        if handle is None:
            raise JqfError(self._dead_session_failure())
        return handle

    def push(self, data):
        """Appends one piece of input `data` (bytes) and frames complete
        records. Returns the number of bytes the feed now retains (the
        published prefix is drained by `poll`, so the count is the feed's
        current buffered input). Raises `JqfError` on a dead feed id or a
        resource failure."""
        return self._handle().feed_push(self._feed_id, data)

    def poll(self):
        """Pulls ONE bounded batch of published output as bytes, or `b""`
        when no record is completed and unpublished. A strict fault raises
        `JqfError` with the failure record retained (a recovering feed keeps
        going past its ordered issues, which ride the session's diagnostic
        stream).

        Every poll also records the batch's diagnostic stream, readable via
        [`last_diagnostics`](Feed.last_diagnostics) — so a RECOVERING feed's
        ordered framing/payload issues are observable on a SUCCESSFUL poll,
        not only on a failure that raises.
        """
        handle = self._handle()
        # The poll's ABI sequence is atomic under the session lock: `feed_poll` AND the `_collect_diags` read-back (a
        # multi-call jqf_diag_count + jqf_diag_get×N + jqf_diag_free_text×3N sequence) must not interleave with another
        # thread's run clearing and repopulating the same handle's stream mid-iteration. Both inner calls are already
        # `@_abi_locked`; the RLock makes this re-entrant.
        with handle._lock:
            written, out_buf = handle.feed_poll(self._feed_id)
            # The stream reflects THIS poll (the ABI clears it per poll except on re-delivery); read it on success too,
            # so recovering-feed issues are observable. Feeds are batch-oriented, not the per-record hot path, so the
            # extra jqf_diag_get round-trips are not the tax the compiled path avoids.
            self._last_diagnostics, self._last_diag_dropped = (
                handle._collect_diags()
            )
            if written < 0:
                failure = handle._select_failure(written, self._last_diagnostics)
                raise JqfError(failure or _undiagnosed_failure(written))
        return ctypes.string_at(out_buf, written)

    def last_diagnostics(self):
        """The diagnostic records of the LAST poll — on a recovering feed,
        the ordered framing/payload issues that poll raised, observable
        even though the poll itself succeeded."""
        return list(getattr(self, "_last_diagnostics", []))

    def last_diag_dropped(self):
        """How many of the LAST poll's diagnostic records overflowed the
        session's capped buffer (`0` normally). Nonzero means the poll's
        ordered issue stream was truncated: issues exist that
        [`last_diagnostics`](Feed.last_diagnostics) cannot show."""
        return getattr(self, "_last_diag_dropped", 0)

    def finish(self):
        """Marks the end of this feed's input stream: the held partial
        record, if any, becomes the stream's FINAL record, delivered by
        subsequent [`poll`][jqf.Feed.poll] calls under the profile's own
        tail law — the same answer the whole-input record route gives over
        the same bytes (a complete final value without its terminator is
        accepted; a truncated one faults per the profile). Later `push`es
        are accepted-and-ignored by the engine; [`close`][jqf.Feed.close]
        still releases the feed. Raises `JqfError` on a dead feed id."""
        self._handle().finish_feed(self._feed_id)

    def close(self):
        """Releases this feed's retained-input residency now. A later
        `push`/`poll`/`finish`/`close` on the same id raises `JqfError` —
        the ABI defines a dead id as a failure, never undefined behavior."""
        handle = self._session._handle
        if handle is None:
            raise JqfError(self._dead_session_failure())
        if handle.close_feed(self._feed_id) != 0:
            raise JqfError(self._dead_feed_failure())
