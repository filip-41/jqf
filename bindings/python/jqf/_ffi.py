"""ctypes bindings over jqf-sdk-ffi (the C ABI). No build step, no pyo3."""
import ctypes
import os

# The ABI version this binding is written against: the cdylib exports the same number via jqf_abi_version(), and the
# load refuses a mismatch. A stale library with the same symbol names would otherwise be called with today's argtypes —
# a signature drift corrupts memory rather than failing. Bump this constant when a jqf-sdk-ffi entry point's signature
# or meaning changes.
JQF_ABI_VERSION = 2

_LIB_NAMES = [
    "libjqf_sdk_ffi.dylib",
    "libjqf_sdk_ffi.so",
    "jqf_sdk_ffi.dll",
]


def _load():
    env = os.environ.get("JQF_FFI_LIB")
    candidates = [env] if env else []
    # The wheel-bundled copy (next to the package) wins over the checkout's target/release and over any bare soname on
    # the loader path, so an installed wheel never silently picks up a different jqf from the system. In the checkout
    # this candidate does not exist and the loop falls through to target/release exactly as before.
    candidates += [
        os.path.join(os.path.dirname(__file__), name) for name in _LIB_NAMES
    ]
    candidates += [
        os.path.join(os.path.dirname(__file__), "..", "..", "..", "target", "release", name)
        for name in _LIB_NAMES
    ]
    # A bare soname (no directory component) goes straight to ctypes: the loader search path
    # (DYLD_LIBRARY_PATH/LD_LIBRARY_PATH, the system library dirs) is where an embedder puts the library, and
    # `os.path.exists` on a bare name only tests the process CWD, so an existence gate would turn a library in /usr/lib
    # into an ImportError. A failed CDLL raises OSError, which the loop continues past, exactly as a missing absolute
    # path does under that gate.
    candidates += _LIB_NAMES
    for candidate in candidates:
        if candidate:
            try:
                return ctypes.CDLL(candidate)
            except OSError:
                continue
    raise ImportError(
        "cannot find libjqf_sdk_ffi; build with `cargo build --release -p jqf-sdk-ffi` "
        "and set JQF_FFI_LIB if it is not under target/release"
    )


_lib = _load()

# The ABI version check: a signature drift on a stale library is a memory corruption, so the load REFUSES a mismatched
# version with a clear message instead of calling into it.
_lib.jqf_abi_version.restype = ctypes.c_uint32
if _lib.jqf_abi_version() != JQF_ABI_VERSION:
    raise ImportError(
        f"libjqf_sdk_ffi ABI version mismatch: the library exports "
        f"jqf_abi_version()={_lib.jqf_abi_version()}, this binding was "
        f"written against version {JQF_ABI_VERSION}. Rebuild the cdylib "
        f"(`cargo build --release -p jqf-sdk-ffi`) and check that the wheel "
        f"matches the checkout."
    )

_lib.jqf_new.restype = ctypes.c_int
_lib.jqf_new.argtypes = [ctypes.POINTER(ctypes.c_void_p)]
# `jqf_new_limited` is deliberately NOT declared here: this binding never constructs a limited engine (`Session` always
# constructs through `jqf_new` with the engine's defaults), and a correct signature needs ctypes Structure definitions
# for the JqfLimits/JqfEncodeOptions pointers that nothing else in this module shares. The ONE caller in-tree owns its
# own declaration instead — bindings/python/tests/rss_streaming_proof.py defines the Limits Structure, sets
# restype/argtypes on this same CDLL object, and calls it — so the declared surface here stays exactly the used surface.
# A void return must be declared as such: ctypes defaults to c_int and reads a garbage return register after the call.
_lib.jqf_free.restype = None
_lib.jqf_free.argtypes = [ctypes.c_void_p]
_lib.jqf_run.restype = ctypes.c_int64
_lib.jqf_run.argtypes = [
    ctypes.c_void_p,
    ctypes.c_char_p,
    ctypes.c_size_t,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
]
_lib.jqf_diag_count.restype = ctypes.c_uint
_lib.jqf_diag_count.argtypes = [ctypes.c_void_p]
_lib.jqf_diag_dropped.restype = ctypes.c_uint
_lib.jqf_diag_dropped.argtypes = [ctypes.c_void_p]
_lib.jqf_diag_get.restype = ctypes.c_int
_lib.jqf_diag_get.argtypes = [
    ctypes.c_void_p,
    ctypes.c_uint,
    ctypes.POINTER(ctypes.c_uint16),
    ctypes.POINTER(ctypes.c_uint16),
    ctypes.POINTER(ctypes.c_char),
    ctypes.POINTER(ctypes.c_char),
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.POINTER(ctypes.c_uint32),
    # The locators and the halt status ride jqf_diag_get.
    ctypes.POINTER(ctypes.c_uint32),
    ctypes.POINTER(ctypes.c_uint64),
    ctypes.POINTER(ctypes.c_uint64),
    ctypes.POINTER(ctypes.c_int32),
    ctypes.POINTER(ctypes.c_char_p),
    ctypes.POINTER(ctypes.c_char_p),
    ctypes.POINTER(ctypes.c_char_p),
]
_lib.jqf_diag_get_text.restype = ctypes.c_int64
_lib.jqf_diag_get_text.argtypes = [
    ctypes.c_void_p,
    ctypes.c_uint,
    ctypes.c_int,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
]
# Which of one record's text fields `jqf_diag_get_text` copies. These values are part of the ABI contract (mirrored from
# the header).
JQF_DIAG_TEXT_KIND = 0
JQF_DIAG_TEXT_OPERAND = 1
JQF_DIAG_TEXT_PAYLOAD = 2
_lib.jqf_diag_free_text.restype = None
_lib.jqf_diag_free_text.argtypes = [ctypes.c_char_p]

# Per-value sequence errors are read back through length-carrying getters (a NUL byte inside an error text survives),
# never a NUL-joined pointer.
_lib.jqf_run_errors_count.restype = ctypes.c_uint
_lib.jqf_run_errors_count.argtypes = [ctypes.c_void_p]
_lib.jqf_run_error_get.restype = ctypes.c_int64
_lib.jqf_run_error_get.argtypes = [
    ctypes.c_void_p,
    ctypes.c_uint,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
]

_lib.jqf_run_sequence.restype = ctypes.c_int64
_lib.jqf_run_sequence.argtypes = [
    ctypes.c_void_p,
    ctypes.c_char_p,
    ctypes.c_size_t,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
]

# --- the compiled-program surface (compile once, run many) ----------------
# `jqf_program` is a handle-LOCAL u32 id, not a pointer: every misuse (a freed id, a double free, a foreign id) is a
# defined -1, never undefined behavior, so a binding can surface it as a plain exception. Programs carry an explicit
# length: an embedded NUL is a typed error, never a silent truncation.
_lib.jqf_compile.restype = ctypes.c_int
_lib.jqf_compile.argtypes = [
    ctypes.c_void_p,
    ctypes.c_char_p,
    ctypes.c_size_t,
    ctypes.POINTER(ctypes.c_uint32),
]
_lib.jqf_compile_args.restype = ctypes.c_int
_lib.jqf_compile_args.argtypes = [
    ctypes.c_void_p,
    ctypes.c_char_p,
    ctypes.c_size_t,
    ctypes.c_uint,
    ctypes.POINTER(ctypes.c_char_p),
    ctypes.POINTER(ctypes.POINTER(ctypes.c_uint8)),
    ctypes.POINTER(ctypes.c_size_t),
    ctypes.POINTER(ctypes.c_uint32),
]
_lib.jqf_program_free.restype = ctypes.c_int
_lib.jqf_program_free.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
_lib.jqf_run_compiled.restype = ctypes.c_int64
_lib.jqf_run_compiled.argtypes = [
    ctypes.c_void_p,
    ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
]
_lib.jqf_run_sequence_compiled.restype = ctypes.c_int64
_lib.jqf_run_sequence_compiled.argtypes = [
    ctypes.c_void_p,
    ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
]

# --- the streaming sequence surface -----------------------------------------
# `jqf_run_sequence_streaming` hands output to a host callback in bounded chunks instead of staging one contiguous
# buffer; `run_many_streaming` in `__init__.py` wraps it as a generator. The verdicts and the chunk bound are ABI
# constants mirrored from the header. These declarations are GUARDED: an older cdylib without the symbol fails here with
# a clear rebuild message rather than an AttributeError at first call, while older BINDINGS against a new library keep
# working (additive entry points are exactly why JQF_ABI_VERSION did not move).
JQF_STREAM_CONTINUE = 0
JQF_STREAM_CANCEL = 1

# The chunk bound the FFI seals at (`STREAM_CHUNK_BYTES` in lib.rs): a delivery is at most this many bytes except when a
# single encoder write exceeds it. Chunks are flow units, not value boundaries.
JQF_STREAM_CHUNK_BYTES = 256 * 1024

if hasattr(_lib, "jqf_run_sequence_streaming"):
    JQF_STREAM_CHUNK_FN = ctypes.CFUNCTYPE(
        ctypes.c_int32, ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint8), ctypes.c_size_t
    )
    _lib.jqf_run_sequence_streaming.restype = ctypes.c_int64
    _lib.jqf_run_sequence_streaming.argtypes = [
        ctypes.c_void_p,
        ctypes.c_char_p,
        ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_uint8),
        ctypes.c_size_t,
        JQF_STREAM_CHUNK_FN,
        ctypes.c_void_p,
    ]
else:
    raise ImportError(
        "libjqf_sdk_ffi does not export jqf_run_sequence_streaming; rebuild "
        "the cdylib (`cargo build --release -p jqf-sdk-ffi`) to match this "
        "binding"
    )

# --- the resident feed (push input in pieces, poll batches out) -----------
# A feed is a handle-LOCAL u32 id, like a program: every misuse (a freed id, an id past the table) is a defined -1,
# never undefined behavior. The profile is a c_int: 0 = ndjson.strict@1, 1 = ndjson.recovering@1.
_lib.jqf_feed_open.restype = ctypes.c_int
_lib.jqf_feed_open.argtypes = [
    ctypes.c_void_p,
    ctypes.c_uint32,
    ctypes.c_int,
    ctypes.POINTER(ctypes.c_uint32),
]
_lib.jqf_feed_push.restype = ctypes.c_int64
_lib.jqf_feed_push.argtypes = [
    ctypes.c_void_p,
    ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
]
_lib.jqf_feed_poll.restype = ctypes.c_int64
_lib.jqf_feed_poll.argtypes = [
    ctypes.c_void_p,
    ctypes.c_uint32,
    ctypes.POINTER(ctypes.c_uint8),
    ctypes.c_size_t,
]
# Marks end of a feed's input stream: the held partial record becomes the stream's FINAL record (delivered by later
# polls under the profile's own tail law). Returns 0 on a live feed id, -1 with a recorded setup diagnostic otherwise.
_lib.jqf_feed_finish.restype = ctypes.c_int
_lib.jqf_feed_finish.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
_lib.jqf_feed_close.restype = ctypes.c_int
_lib.jqf_feed_close.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
