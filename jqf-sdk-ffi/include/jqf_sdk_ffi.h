/* jqf-sdk-ffi: the C ABI for jqf's SDK.
 *
 * CHECKED IN AND GATED: `make ffi-header-lint` (tools/gates/jqf-ffi-header-lint.py)
 * fails when this file drifts from the Rust signatures in
 * jqf-sdk-ffi/src/lib.rs — every entry point here must exist there with the
 * same arity, and vice versa. A C embedder declares nothing by hand, which
 * is exactly what makes a drifted signature detectable instead of corrupting
 * memory.
 *
 * Convention summary (see the Rust module doc for the full contract):
 * - Byte-producing entry points follow the `snprintf` convention: call with
 *   (NULL, 0) to learn the REQUIRED length; a returned count > the capacity
 *   offered means the buffer holds only the first `cap` bytes and the caller
 *   must re-call with at least the required size. Failure is -1. The -1
 *   sentinel, never the buffer contents, decides failure: bytes published
 *   before a failure may already be in `out` (or already delivered to a
 *   streaming consumer).
 * - A (NULL, 0) input is the documented EMPTY input; a (NULL, 0) output is
 *   the sizing probe. Both are defined, never undefined behavior.
 * - Programs carry an explicit length; an embedded NUL is rejected with a
 *   typed MACHINE_SETUP error, never silently truncated.
 * - The handle is one thread at a time. jqf_abi_version() must be checked at
 *   load and a mismatch refused.
 * - All text allocations returned via `char **` out-params are released with
 *   jqf_diag_free_text.
 */
#ifndef JQF_SDK_FFI_H
#define JQF_SDK_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The ABI version this header is written against. */
#define JQF_ABI_VERSION 2u

/* The host control callback's verdicts (JqfLimits.control_callback). */
#define JQF_CONTROL_CONTINUE 0
#define JQF_CONTROL_CANCELLED 1
#define JQF_CONTROL_DEADLINE 2
#define JQF_CONTROL_MEMORY 3

/* The feed framing profiles (jqf_feed_open's `profile` argument). */
#define JQF_FEED_PROFILE_STRICT 0
#define JQF_FEED_PROFILE_RECOVERING 1

/* The limits and cancellation contract one handle runs under.
 * Every numeric ceiling is the VALUE the request ledger charges against;
 * pass the MAX for an unbounded dimension. deadline_ms counts from
 * jqf_new_limited and bounds every run on the handle (0 = none). The
 * callback returns one of the JQF_CONTROL_* verdicts and must not call any
 * jqf_* on this handle; NULL = none; the context must stay valid for the
 * handle's lifetime. */
typedef struct jqf_limits {
    uint64_t max_output_bytes;
    uint64_t max_memory_bytes;
    uint64_t max_spill_bytes;
    uint32_t max_nesting_depth;
    uint64_t deadline_ms;
    int32_t (*control_callback)(void *context);
    void *control_context;
} jqf_limits;

/* The JSON output formatting every run on a handle uses.
 * indent: -1 compact (default), -2 tabs, 0..=7 spaces. NULL encode options
 * to jqf_new_limited means compact output. */
typedef struct jqf_encode_options {
    int32_t indent;
    uint8_t raw_strings;   /* -r */
    uint8_t sort_keys;     /* -S */
    uint8_t ascii_output;  /* -a */
    uint8_t raw_output_nul; /* --raw-output0 */
} jqf_encode_options;

uint32_t jqf_abi_version(void);

int32_t jqf_new(void **out);
int32_t jqf_new_limited(const jqf_limits *limits,
                        const jqf_encode_options *encode,
                        void **out);
void jqf_free(void *handle);

int32_t jqf_compile(void *handle, const char *program, size_t program_len,
                    uint32_t *out_id);
int32_t jqf_compile_args(void *handle, const char *program,
                         size_t program_len, uint32_t binding_count,
                         const char *const *names,
                         const uint8_t *const *json_values,
                         const size_t *json_value_lengths,
                         uint32_t *out_id);
int32_t jqf_program_free(void *handle, uint32_t program_id);

int32_t jqf_feed_open(void *handle, uint32_t program_id, int32_t profile,
                      uint32_t *out_id);
int64_t jqf_feed_push(void *handle, uint32_t feed_id, const uint8_t *input,
                      size_t input_len);
int64_t jqf_feed_poll(void *handle, uint32_t feed_id, uint8_t *out,
                      size_t out_cap);
int32_t jqf_feed_close(void *handle, uint32_t feed_id);
int32_t jqf_feed_finish(void *handle, uint32_t feed_id);

int64_t jqf_run(void *handle, const char *program, size_t program_len,
                const uint8_t *input, size_t input_len, uint8_t *out,
                size_t out_cap);
int64_t jqf_run_compiled(void *handle, uint32_t program_id,
                         const uint8_t *input, size_t input_len,
                         uint8_t *out, size_t out_cap);
int64_t jqf_run_sequence(void *handle, const char *program,
                         size_t program_len, const uint8_t *input,
                         size_t input_len, uint8_t *out, size_t out_cap);
int64_t jqf_run_sequence_compiled(void *handle, uint32_t program_id,
                                  const uint8_t *input, size_t input_len,
                                  uint8_t *out, size_t out_cap);

/* Per-value sequence errors of the LAST run, read back through the
 * length-carrying getter (a NUL byte inside an error text survives). */
uint32_t jqf_run_errors_count(const void *handle);
int64_t jqf_run_error_get(const void *handle, uint32_t index, uint8_t *out,
                          size_t out_cap);

/* The streaming sequence entry point's chunk-consumer verdicts. */
#define JQF_STREAM_CONTINUE 0
#define JQF_STREAM_CANCEL 1

/* Runs one program over a MULTI-VALUE input stream and hands the output to
 * the host in bounded chunks through chunk(context, bytes, len), instead of
 * staging one contiguous buffer. Same drive, input model, diagnostic
 * stream, per-value error channel, and halt-status law as
 * jqf_run_sequence; only publication differs. Returns the total bytes
 * delivered (-1 on failure); concatenating the chunks in order reproduces
 * jqf_run_sequence's output byte for byte. Chunks are FLOW units, not
 * frame boundaries: a delivery is at most one internal chunk (256 KiB)
 * except when a single encoder write exceeds it, and one huge value may
 * straddle many callbacks — the output byte stream carries the framing.
 * `bytes` is valid ONLY for the duration of the call. Verdicts:
 * JQF_STREAM_CONTINUE (0) / JQF_STREAM_CANCEL (1); an unknown verdict
 * continues. A cancellation stops publication cleanly and returns -1 with
 * a MACHINE_SETUP record naming the cancellation and the bytes delivered.
 * A NULL `chunk` is a setup failure (-1). The callback runs on this
 * handle's request thread before the entry returns. */
typedef int32_t (*jqf_stream_chunk_fn)(void *context, const uint8_t *bytes,
                                       size_t len);
int64_t jqf_run_sequence_streaming(void *handle, const char *program,
                                   size_t program_len, const uint8_t *input,
                                   size_t input_len,
                                   jqf_stream_chunk_fn chunk, void *context);

uint32_t jqf_diag_count(const void *handle);
uint32_t jqf_diag_dropped(const void *handle);
/* The locator out-params (step_index/input_ordinal/byte_offset)
 * read u32::MAX/u64::MAX when the record carries none; halt_status reads -1
 * for every non-halt record, the exit status for the RAISE_HALT terminal
 * (0 for a bare `halt`). */
int32_t jqf_diag_get(const void *handle, uint32_t index, uint16_t *code,
                     uint16_t *revision, char *record_class, char *severity,
                     uint8_t *catchable, uint32_t *caught,
                     uint32_t *step_index, uint64_t *input_ordinal,
                     uint64_t *byte_offset, int32_t *halt_status,
                     char **kind, char **operand, char **payload);
/* Length-carrying sibling of jqf_diag_get's text fields: a NUL inside the
 * record text survives. field is JQF_DIAG_TEXT_KIND / OPERAND / PAYLOAD.
 * snprintf convention; (NULL, 0) is the sizing probe. */
#define JQF_DIAG_TEXT_KIND 0
#define JQF_DIAG_TEXT_OPERAND 1
#define JQF_DIAG_TEXT_PAYLOAD 2
int64_t jqf_diag_get_text(const void *handle, uint32_t index, int32_t field,
                          uint8_t *out, size_t out_cap);
void jqf_diag_free_text(char *text);

#ifdef __cplusplus
}
#endif

#endif /* JQF_SDK_FFI_H */
