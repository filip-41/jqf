/* tslint:disable */
/* eslint-disable */

/**
 * The ABI version as a number, for the JS wrapper's load-time check.
 */
export function jqf_abi_version(): number;

/**
 * The supported formats and their default dialects, as a JSON array of `{"name","format","in","out"}` rows.
 */
export function jqf_formats(): string;

/**
 * Runs `program` over `input` and returns the result ENVELOPE as a JSON string (never fails across the boundary —
 * every failure lands inside the envelope as `ok:false` with the diagnostic records).
 *
 * `indent` follows jq's `--indent` law: -1 = tabs, 0 = compact, 1..=7 = spaces per level; anything else lands in the
 * envelope as `ok:false`. `flags` is a bitmask of the `FLAG_*` constants; `slurp` is `-s`.
 *
 * # Envelope shape
 *
 * ```text
 * {"ok":true,"output":"...","value_errors":[],"records":[...]}
 * {"ok":false,"output":"partial","error":"...","records":[...],"halt_status":5}
 * ```
 *
 * Binary output (CBOR/MessagePack targets) arrives base64 under `output_base64` with `"binary":true`.
 *
 * # Panics
 *
 * Only if the built-in format table itself names an unknown format — impossible for the committed table; user input
 * never reaches a panic (every caller error lands inside the envelope).
 */
export function jqf_run(program: string, input: Uint8Array, input_format: string, output_format: string, indent: number, flags: number, slurp: boolean): string;

/**
 * The ABI version.
 */
export function jqf_version(): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly jqf_abi_version: () => number;
    readonly jqf_formats: (a: number) => void;
    readonly jqf_run: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: number) => void;
    readonly jqf_version: (a: number) => void;
    readonly __wbindgen_export: (a: number) => void;
    readonly __wbindgen_add_to_stack_pointer: (a: number) => number;
    readonly __wbindgen_export2: (a: number, b: number, c: number) => void;
    readonly __wbindgen_export3: (a: number, b: number) => number;
    readonly __wbindgen_export4: (a: number, b: number, c: number, d: number) => number;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
