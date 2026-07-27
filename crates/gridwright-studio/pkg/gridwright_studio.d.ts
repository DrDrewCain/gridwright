/* tslint:disable */
/* eslint-disable */

/**
 * Read a file. Returns `Loaded` as JSON, or `Failure` as JSON.
 */
export function load_bytes(name: string | null | undefined, bytes: Uint8Array): string;

export function mount(canvas_id: string): void;

/**
 * Solve a network given as JSON. Returns `Solved` or `Failure` as JSON.
 */
export function solve_json(network_json: string): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly mount: (a: number, b: number) => [number, number];
    readonly load_bytes: (a: number, b: number, c: number, d: number) => [number, number];
    readonly solve_json: (a: number, b: number) => [number, number];
    readonly wasm_bindgen_1cd6b3e0a26d7e04___convert__closures_____invoke___wasm_bindgen_1cd6b3e0a26d7e04___JsValue__core_9b3796e30d99ddb7___result__Result_____wasm_bindgen_1cd6b3e0a26d7e04___JsError___true_: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen_1cd6b3e0a26d7e04___convert__closures_____invoke___js_sys_6a91aa343af0b9d9___Array______true_: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen_1cd6b3e0a26d7e04___convert__closures_____invoke___js_sys_6a91aa343af0b9d9___Array______true__2: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen_1cd6b3e0a26d7e04___convert__closures_____invoke___core_9b3796e30d99ddb7___result__Result_____wasm_bindgen_1cd6b3e0a26d7e04___JsValue___true_: (a: number, b: number) => [number, number];
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_destroy_closure: (a: number, b: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
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
