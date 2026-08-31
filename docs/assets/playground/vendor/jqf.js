/**
 * jqf.js — the browser wrapper over jqf's WebAssembly binding.
 *
 * The raw wasm-bindgen glue (`jqf_wasm.js`, generated at build time) takes
 * byte arrays; this wrapper owns text encoding, option defaults, and
 * envelope parsing, so a page needs exactly one call:
 *
 *   const jqf = await loadJqf();                       // once per page
 *   const result = jqf.run('.name', '{"name":"Ada"}'); // -> parsed envelope
 *
 * The envelope shape (see bindings/wasm/src/lib.rs):
 *   { ok: true,  output: "...", value_errors: [], records: [...] }
 *   { ok: false, output: "partial", error: "...", records: [...] }
 * Binary output arrives base64 under output_base64 with binary: true.
 */
import init, {
    jqf_run,
    jqf_formats,
    jqf_version,
    jqf_abi_version,
} from './jqf_wasm.js';

const encoder = new TextEncoder();

/** Bit flags accepted by run()'s opts.flags. */
export const FLAGS = Object.freeze({
    RAW_STRINGS: 1,
    SORT_KEYS: 2,
    ASCII: 4,
    NULL_INPUT: 8,
});

/** The ABI version this wrapper speaks; loadJqf refuses a mismatch. */
export const ABI_VERSION = 1;

let ready = null;

/**
 * Loads and initializes the jqf engine. Idempotent; safe to call many times.
 * @param {string|URL|Uint8Array} [wasmModule] Optional explicit location or
 *   raw bytes of the .wasm artifact (needed when default relative resolution
 *   does not fit your layout, e.g. under Node).
 * @returns {Promise<Jqf>} a loaded instance exposing .run/.formats/.version
 */
export function loadJqf(wasmModule) {
    if (!ready) {
        ready = init(wasmModule ? { module_or_path: wasmModule } : undefined).then(() => {
            if (Number(jqf_abi_version()) !== ABI_VERSION) {
                throw new Error(
                    `jqf-wasm: ABI mismatch — glue reports ${jqf_abi_version()}, wrapper expects ${ABI_VERSION}; rebuild both together`,
                );
            }
            return new Jqf();
        });
    }
    return ready;
}

/** Thin facade over the exported functions with JS-friendly types. */
export class Jqf {
    /**
     * Runs one program over one input.
     * @param {string} program the jq program
     * @param {string|Uint8Array|null} input the document(s); null runs -n
     * @param {{input?:string, output?:string, indent?:number, flags?:number, slurp?:boolean}} opts
     *   indent follows jq's --indent law: 0 = compact (default), 1..=7 =
     *   spaces per level, anything else lands in the envelope as ok:false.
     * @returns {object} the parsed result envelope
     */
    run(program, input, opts = {}) {
        const {
            input: inputFormat = 'json',
            output: outputFormat = 'json',
            indent = 0,
            flags = 0,
            slurp = false,
        } = opts;
        let bytes;
        let effectiveFlags = flags | 0;
        if (input === null || input === undefined) {
            bytes = encoder.encode('');
            effectiveFlags |= FLAGS.NULL_INPUT;
        } else if (typeof input === 'string') {
            bytes = encoder.encode(input);
        } else {
            bytes = input;
        }
        return JSON.parse(
            jqf_run(program, bytes, inputFormat, outputFormat, indent, effectiveFlags, slurp),
        );
    }

    /** The supported formats: [{name, format, in, out}]. */
    formats() {
        return JSON.parse(jqf_formats());
    }

    /** Version string like "jqf-wasm 0.1.0". */
    version() {
        return jqf_version();
    }
}

export default loadJqf;
