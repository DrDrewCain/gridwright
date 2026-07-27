// The engine, on a thread that is allowed to stop responding.
//
// This worker exists for one reason: a solve of a few thousand rows takes
// hundreds of milliseconds and a large one takes seconds, and anything that
// long on the main thread is a frozen tab. It is not an optimisation and it is
// not about parallelism — the solver is single-threaded on purpose — it is
// about which thread is permitted to block.
//
// Protocol, over `postMessage`:
//
//   in   { id, op: "load",  name, bytes }   bytes: ArrayBuffer | Uint8Array
//   in   { id, op: "solve", network }       network: object | JSON string
//   out  { id, ok: true,  result, ms }
//   out  { id, ok: false, error }
//
// `id` is echoed and otherwise ignored, so a caller can have several requests
// outstanding. Requests are handled in arrival order because there is one wasm
// instance and it is not reentrant.
//
// This is a module worker (`new Worker(url, { type: "module" })`), because the
// wasm-bindgen `--target web` output is an ES module and importing it needs
// `import`, not `importScripts`.

let engine = null; // the wasm-bindgen module namespace, once initialised
let ready = null; // the in-flight init, so concurrent messages await one load
let lastNetwork = null; // JSON text of the last successful load, see "solve"
let poisoned = null; // set if the wasm instance died; every later call fails

// The engine is built with `panic = "abort"`. A Rust panic therefore takes the
// whole wasm instance with it and every subsequent call throws
// "unreachable executed" with no useful detail. Once that has happened the
// worker cannot recover — only a fresh instance can — so it says so plainly
// rather than emitting a stream of mystery errors.
function poison(err) {
  poisoned =
    `the wasm instance aborted and cannot be reused (${err}); ` +
    "reload the page to get a new one";
}

function initEngine() {
  if (!ready) {
    ready = (async () => {
      // The manifest is the only file here fetched fresh; it names the current
      // content-hashed build. Everything it points at is immutable, which is
      // why it must not itself be cached.
      const manifestUrl = new URL("./pkg/manifest.json", import.meta.url);
      const res = await fetch(manifestUrl, { cache: "no-cache" });
      if (!res.ok) {
        throw new Error(
          `no pkg/manifest.json (HTTP ${res.status}) — run web/build.sh first`,
        );
      }
      const manifest = await res.json();

      const moduleUrl = new URL(manifest.module, manifestUrl);
      const wasmUrl = new URL(manifest.wasm, manifestUrl);

      const mod = await import(moduleUrl.href);
      // Passed explicitly rather than left to the default so the URL comes from
      // the manifest in both places and the two cannot drift apart.
      await mod.default({ module_or_path: wasmUrl });
      engine = mod;
    })().catch((err) => {
      // A failed init must not be cached as a permanent failure: the usual
      // cause is a missing build, and the usual fix is to run build.sh and
      // reload, but a retried message should get a real second attempt.
      ready = null;
      throw err;
    });
  }
  return ready;
}

// Every engine entry point returns JSON that is either the success type or a
// `Failure` `{ kind, message }`. They are distinguished structurally, since
// the engine has no room for a tag byte in a string return.
function unwrap(json, successField) {
  let value;
  try {
    value = JSON.parse(json);
  } catch (err) {
    throw new Error(`the engine returned text that is not JSON: ${err}`);
  }
  const isFailure =
    value !== null &&
    typeof value === "object" &&
    typeof value.kind === "string" &&
    typeof value.message === "string" &&
    !(successField in value);
  if (isFailure) {
    throw new Error(`${value.kind}: ${value.message}`);
  }
  return value;
}

function asBytes(bytes) {
  if (bytes instanceof Uint8Array) return bytes;
  if (bytes instanceof ArrayBuffer) return new Uint8Array(bytes);
  if (ArrayBuffer.isView(bytes)) {
    return new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }
  throw new Error("load needs `bytes` as an ArrayBuffer or a typed array");
}

function handle(msg) {
  switch (msg.op) {
    case "load": {
      const bytes = asBytes(msg.bytes);
      // `name` only hints at the format; the readers sniff content when it is
      // absent or unhelpful. `undefined` maps to Rust's `None`.
      const loaded = unwrap(
        engine.load_bytes(msg.name ?? undefined, bytes),
        "network",
      );
      // Kept so a caller can solve without shipping the whole network back
      // across the boundary. A large network is megabytes of JSON and the
      // round trip is pure waste when nothing edited it.
      lastNetwork = JSON.stringify(loaded.network);
      return loaded;
    }

    case "solve": {
      let json;
      if (typeof msg.network === "string") {
        json = msg.network;
      } else if (msg.network != null) {
        json = JSON.stringify(msg.network);
      } else if (lastNetwork != null) {
        json = lastNetwork;
      } else {
        throw new Error("solve needs `network`, or a previous successful load");
      }
      return unwrap(engine.solve_json(json), "status");
    }

    default:
      throw new Error(`unknown op ${JSON.stringify(msg.op)}`);
  }
}

self.onmessage = async (event) => {
  const msg = event.data ?? {};
  const id = msg.id;
  const started = performance.now();

  try {
    if (poisoned) throw new Error(poisoned);
    await initEngine();

    const result = handle(msg);
    self.postMessage({ id, ok: true, result, ms: performance.now() - started });
  } catch (err) {
    const text = err instanceof Error ? err.message : String(err);
    // RuntimeError is what a wasm trap surfaces as, and after `panic = "abort"`
    // it means the instance is gone rather than that this one call failed.
    if (engine && err instanceof WebAssembly.RuntimeError) poison(text);
    self.postMessage({
      id,
      ok: false,
      error: poisoned ?? text,
      ms: performance.now() - started,
    });
  }
};
