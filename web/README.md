# The browser deployment layer

The gridwright engine, compiled to WebAssembly and served as a static site.
Nothing is uploaded: a file dropped on the page is read and solved in the tab
that opened it.

| File | What it is |
| --- | --- |
| `build.sh` | Builds `gridwright-worker` for wasm and writes `pkg/`. The only build step. |
| `worker.js` | The Web Worker. Owns the wasm instance and speaks a four-message protocol. |
| `index.html`, `main.js` | A harness proving the round trip works. Not the interface. |
| `pkg/` | Generated. Content-hashed wasm plus the JS bindings, and a manifest naming them. |
| `../vercel.json` | Static hosting config. Lives at the repo root because that is where Vercel looks. |

The harness is a proof, not a product. It shows a network summary, the reader's
notes, and the objective and status of a solve, in about 250 lines of plain DOM
so that a break is obvious. The real interface is an eframe studio being built
separately.

## Build

Needs a Rust toolchain with the `wasm32-unknown-unknown` target, and
`wasm-bindgen-cli` at exactly the version `Cargo.lock` pins for the
`wasm-bindgen` crate — they are one program split across a build boundary, and a
mismatch fails deep inside the CLI with a schema error. `build.sh` checks both
up front and tells you the command to fix it.

```
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126
./web/build.sh
```

Two steps run: `cargo build --release` produces a raw wasm module whose exports
speak in pointers and lengths, then `wasm-bindgen --target web` writes the
JavaScript that turns those into strings and byte arrays. The cargo artifact
alone is not loadable — its `load_bytes` takes four integers.

Measured on 26 July 2026, aarch64-apple-darwin, wasm-bindgen 0.2.126:

| Artifact | Bytes |
| --- | --- |
| `gridwright_worker_<hash>_bg.wasm` | 2,344,554 (2.24 MiB) |
| the same, `brotli -q 11` — what a CDN sends | 585,935 (0.56 MiB) |
| `gridwright_worker_<hash>.js` | 7,740 |

Then serve the directory over HTTP. `file://` will not work: ES modules, module
workers and `fetch` all require an origin.

```
python3 -m http.server 8080 --directory web
```

### Why the file names carry a hash

`build.sh` names the output after the first 8 hex digits of the sha256 of the
wasm cargo produced, and writes `pkg/manifest.json` naming the current build.
The worker fetches the manifest, then imports what it points at.

That one indirection is what makes `Cache-Control: immutable` in `vercel.json`
honest. A changed engine is a changed URL, so a returning visitor revalidates
one 200-byte JSON file and either reuses a 2 MB module already on disk or
fetches a new one. Without it, the choice is between a stale engine and
revalidating megabytes on every load.

It also makes the build idempotent in the way that matters: the same input
produces the same names, and `pkg/` is emptied and rewritten each run, so a
previous build's files cannot linger and be served.

`pkg/` is generated but deliberately **not** in `.gitignore`. Every deployment
path below needs those files present in the tree that gets uploaded, and whether
the Vercel CLI skips git-ignored files is not something this was able to verify.
If you add the ignore, verify a deploy afterwards rather than assuming.

## The worker protocol

The worker exists because the main thread may not block. A 1,354-bus solve takes
about 670 ms in the browser (measured below); on the main thread that is two
thirds of a second of a frozen tab. This is not about parallelism — see the next
section.

```
in   { id, op: "load",  name, bytes }    bytes: ArrayBuffer | Uint8Array
in   { id, op: "solve", network }        network: object | JSON string
out  { id, ok: true,  result, ms }
out  { id, ok: false, error }
```

`id` is echoed so a caller can have several requests outstanding; they are
handled in arrival order because there is one wasm instance and it is not
reentrant. `name` on a load only hints at the format — the readers sniff content
when it is absent or unhelpful.

`network` may be omitted on a solve, in which case the worker solves the copy it
kept from the last successful load. The harness sends it explicitly because that
is the protocol as written and worth proving; a real interface should omit it,
because once the model is large, re-serialising it on the main thread is exactly
the stall the worker exists to prevent.

Both engine entry points return JSON that is either the success type or a
`Failure` `{ kind, message }`; the worker tells them apart structurally and
turns a failure into `{ ok: false, error: "kind: message" }`.

One caveat worth knowing: the engine is built with `panic = "abort"`, so a Rust
panic destroys the wasm instance and every later call traps. The worker detects
this and replies with a message saying to reload, rather than emitting a stream
of `unreachable executed`.

## Why there are no cross-origin isolation headers

None of `Cross-Origin-Opener-Policy`, `Cross-Origin-Embedder-Policy` or
`SharedArrayBuffer` appear anywhere here, and that is a decision rather than an
omission.

Those headers exist to unlock `SharedArrayBuffer`, which exists to give
WebAssembly threads. **The solver is single-threaded on purpose.** Rayon touches
roughly 0.2% of the solve loop, so threading it buys almost nothing, while
cross-origin isolation costs a great deal: it breaks every cross-origin
`<iframe>`, image and script that does not opt in with CORP, it rules out
embedding the page in documentation or a dashboard, and it means every
deployment target has to be able to set response headers.

Paying that for 0.2% would be a bad trade. If a future profile changes the
arithmetic, the headers can be added then — but they should be added because a
measurement demanded them, not because a tutorial mentioned them.

## Deploying

`vercel.json` at the repo root serves `web/` as the site root. It does two
things that are not defaults.

**`Content-Type: application/wasm` is set explicitly** for `/(.*).wasm`.
`WebAssembly.instantiateStreaming` — which wasm-bindgen's loader uses — refuses
any response whose MIME type is not exactly `application/wasm` and throws rather
than falling back. Whether Vercel gets this right unaided is not verified here,
and the failure mode if it does not is a blank page. Setting it costs one rule.

**Wildcards are `/(.*)`, never `/`.** In a `source`, `/` matches the root path
and nothing else. Vercel's own knowledge base has used `/` in an example of a
site-wide header rule, which is a rule that silently applies to exactly one URL.

The cache rules follow from the hashing: `/pkg/gridwright_worker_(.*)` is
immutable for a year, and `/pkg/manifest.json` is `max-age=0,
must-revalidate` — it is the one file that must always be fresh.

The Vercel build image has no Rust toolchain, so the wasm cannot be built there.
Build locally, then upload what you built:

```
./web/build.sh
vercel build              # check .vercel/output/static/pkg/ has the hashed files
vercel deploy --prebuilt --prod
```

Deploying through the Git integration instead works only if `web/pkg/` is
committed, for the same reason.

Nothing here is Vercel-specific beyond that one file. Any static host works
given the same two response headers.

## Verified

On 27 July 2026, against a local `http.server`, driven in Chromium:

| Case | Read | Solve | Result |
| --- | --- | --- | --- |
| `examples/pglib/case14_ieee.m` | 20.3 ms | 6.3 ms | Optimal, objective 2051.53 |
| `examples/pglib/case1354_pegase.m` | 18.9 ms | 665.6 ms | Optimal, objective 1303437.22 |

The case14 objective is the same to the last digit as `gw case
examples/pglib/case14_ieee.m` natively, which solves the same model with HiGHS
rather than the pure-Rust simplex. Reader notes came through — the MATPOWER
reader reported approximating quadratic generator costs by their linear term,
and that reactive power and voltage limits are read but unused by the DC
formulation. Handing the page a file that is not a network (`vercel.json`)
produced `read: reading JSON case: not valid JSON: missing field 'snapshots' at
line 48 column 1` in the status line and left the worker able to handle the next
file.
