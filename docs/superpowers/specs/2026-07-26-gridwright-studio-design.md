# gridwright studio: a serverless in-browser modelling environment

**Date:** 26 July 2026
**Status:** design, awaiting review
**Scope:** the interface layer described in `TODO.md` § Interface, replacing the
`wasm-bindgen` wrapper and Dioxus items with a design grounded in measurement.

## What this is for

An interactive environment for building, editing and solving energy system
models — closer to a design studio than a batch tool. Sketch a network, change a
parameter, see prices and flows move. It runs in a browser tab with **no server
at all**, hosted as static files, and the same source builds a native desktop
application for work that outgrows a tab.

The claim that makes this possible is not speed. It is the memory figure:
gridwright builds a 16.3 M variable model in **1.50 GB** where PyPSA takes
**12.13 GB** on the same network. A browser tab has the former and not the
latter, so this is a thing a Python modelling stack cannot follow us into.

## What was measured before designing anything

Every number here was taken on 26 July 2026, warm, n=5, on an M3 Max, against
the real crates rather than a toy. The wasm figures come from a `cdylib` loaded
through Node's raw `WebAssembly` API — no `wasm-bindgen`, so nothing in the
measurement depends on the binding layer this document proposes.

### The engine runs in wasm today, correctly

`gridwright-net`, `-model`, `-build` and `-simplex` all compile for
`wasm32-unknown-unknown`. `gridwright-solve` compiles with
`--no-default-features --features simplex`. The full-year model — 16,258,560
variables, 29,153,280 nonzeros — **builds inside a wasm32 module** and returns a
nonzero count identical to the native build.

Payload for the whole engine including the solver: **0.56 MB raw, 110 KB
brotli.** Vercel's static limits are not a consideration at that size.

### Construction under wasm, and what rayon costs

| Case | Native | wasm | ratio |
| --- | --- | --- | --- |
| 256 × 168 | 2.4 ms | 5.0 ms | 2.1× |
| 128 × 720 | 3.7 ms | 10.7 ms | 2.9× |
| 256 × 720 | 6.2 ms | 21.7 ms | 3.5× |
| 256 × 2190 | 16.5 ms | 65.8 ms | 4.0× |
| 256 × 8760 | 50.5 ms | 265.6 ms | 5.3× |

`rayon` does **not** trap on `wasm32-unknown-unknown`; it falls back to
sequential execution and the results stay correct. The penalty grows with size
because parallelism is what is lost, and that growth is the whole of the rayon
question.

### The solver under wasm, which is the real ceiling

| Rows | Native | wasm | ratio |
| --- | --- | --- | --- |
| 432 | 3.9 ms | 10.0 ms | 2.6× |
| 864 | 14.8 ms | 48.5 ms | 3.3× |
| 2,592 | 152.6 ms | 399.9 ms | 2.6× |
| 3,456 | 267.6 ms | 750.3 ms | 2.8× |
| 7,776 | 1.68 s | 3.86 s | 2.3× |
| 13,824 | 5.03 s | 14.37 s | 2.9× |

The ratio is **flat at about 2.7×**, and flatness is the finding. The simplex
does not use rayon, so it loses no parallelism and pays only wasm's interpreter
overhead. Construction degrades with size; the solver does not.

## The decision that follows: no threads in v1

Threads were investigated thoroughly and are **deliberately not used**, on the
evidence rather than on difficulty.

At an interactive size of roughly 2,600 rows, construction costs **under 1 ms**
in wasm and the solve costs **400 ms**. Construction is about 0.2% of the loop,
and it is the only part `rayon` touches. Threads would optimise the 0.2% while
the 99.8% is a single-threaded simplex that gains nothing from them.

What that decision removes, all at once:

- a pinned nightly toolchain and `-Z build-std` indefinitely — wasm atomics
  stabilisation (rust-lang/rust#77839) is open with no stabilisation PR
- `wasm-bindgen-rayon`, whose last crates.io release is 19 months old
- COOP/COEP cross-origin isolation and its hosting requirements
- `SharedArrayBuffer` availability questions
- an unfixed `Atomics.wait` trap in `initThreadPool` (wasm-bindgen-rayon#36)
- `egui-wgpu`'s default `fragile-send-sync-non-atomic-wasm`, which is
  incompatible with `+atomics`
- two wasm modules built with different `RUSTFLAGS`, an integration for which no
  prior art could be found

The work is not wasted and is recorded below, because the desktop build gets
real threads for free and the web build can adopt them later if construction
ever becomes the bottleneck. Today's measurements say it is not.

**Verified, for the record:** the engine does compile with
`+atomics,+bulk-memory`, `--shared-memory`, `--import-memory`, `--max-memory`
and the four `__tls_*` exports, producing a module with imported shared memory
and correct results. Should threads ever be wanted, that path is known to work
and the flags are known to be these — `+atomics` stopped implying shared memory
in Rust 1.92 (rust-lang/rust#147225), which is easy to get wrong.

## The lever that actually raises the ceiling

The interactive operation is not "solve a model". It is **edit one parameter and
re-solve**, and those are different problems that this engine currently treats
identically.

`crate::solve` builds a tableau, crashes a basis, runs phase one and then phase
two, every time. `lib.rs` records phase one as consistently about three quarters
of iterations. But after an edit the previous basis is nearly right: change a
cost and it stays primal-feasible with only reduced costs moving; change a bound
and it stays dual-feasible. Either is a handful of pivots from optimal.

So **warm starting is the single highest-leverage piece of work for this
interface**, plausibly 10–100× on the edit→resolve loop, and it needs no
external dependency, no threads and no nightly. It is tracked in `TODO.md` under
the Solver section; this document is the reason its priority should rise.

The ~2,000-row figure quoted as the in-browser ceiling is the *cold-solve*
ceiling. With warm starts the interactive ceiling is a different and higher
number, which should be measured rather than guessed once the work lands.

## Architecture

```
                    main thread                     Web Worker
              ┌──────────────────────┐        ┌──────────────────────┐
              │  gridwright-studio   │  RPC   │  gridwright-worker   │
              │  egui / eframe       │◀──────▶│  engine + solver     │
              │  render, edit, plot  │        │  build, solve        │
              └──────────────────────┘        └──────────────────────┘
                         │                              │
                         └────────── same crates ───────┘
                                  gridwright-net
                                  gridwright-build
                                  gridwright-solve
```

Four decisions, each with its reason.

**The engine runs in a Web Worker, not on the main thread.** Not for
parallelism — for responsiveness. The main thread may not block, and a 400 ms
solve on it is a frozen tab. This is required even though the solver is
single-threaded, and it is what makes a multi-second solve survivable.

**egui/eframe for the interface.** It is the one mainstream Rust UI that
compiles the same source to a browser canvas and to a native window, which is
the requirement that a desktop build not be a second codebase. It is
immediate-mode and dense-tool-shaped rather than document-shaped. Node-graph
editing comes from `egui-snarl`, plots from `egui_plot`, and a `wgpu` viewport is
available for 3-D without adopting a second renderer.

**The solver is a trait with per-target implementations.** The web build gets
the pure-Rust simplex; the native build can additionally link HiGHS, which
cannot compile to wasm at all — verified twice, since `highs-sys` and `osqp`
both fail with the same CMake error against `wasm32-unknown-unknown`.

**Static hosting, no server.** The deployment is a directory of files.

## Components

### `gridwright-worker`

A `cdylib` exposing the engine across the worker boundary. Responsibilities:
receive a network description, build, solve, return results, report progress,
and observe cancellation. It owns no UI concepts.

The API is coarse deliberately — whole operations rather than chatty accessors —
because every call crosses a serialisation boundary.

### `gridwright-studio`

The eframe application: rendering, editing, plotting, and the state machine
around an in-flight solve. Builds to `wasm32-unknown-unknown` for the browser and
natively for desktop, from the same source.

### `gridwright-solver-api`

A small crate holding the `Solver` trait so that neither implementation crate is
a dependency of the other, and so the wasm dependency graph never contains a
C++ toolchain requirement.

## Data flow, and one trap in it

Edit → studio updates the network → posts a build+solve request → worker builds,
solves, posts results → studio renders.

**`WebAssembly.Memory`'s buffer is not transferable.** It carries an
`[[ArrayBufferDetachKey]]`, so `postMessage` transfer of it fails: Firefox throws
and **Chrome silently copies** (whatwg/html#4601). Results must therefore be
copied into a fresh `ArrayBuffer`, which is transferable, and that buffer moved.
One memcpy of a few MB is cheap; the silent-copy failure mode is not, because it
looks like it works.

## Error handling

Three classes, deliberately distinguished:

**Model errors** — infeasible, unbounded, malformed. These are results, not
faults, and the interface must show them as such. The engine already prices load
shedding so that an unservable system reports *where and when* rather than the
word INFEASIBLE, and the interface should surface that rather than hide it.

**Solver limits** — a branch-and-bound that stopped on its node budget returns an
incumbent, not a proved optimum. `TODO.md` already asks that status be shown
rather than just the number, and this is where that gets honoured. An answer
whose provenance is hidden is worse than a slow one.

**Worker faults** — out-of-memory kills the instance outright: `memory.grow`
returns −1 and Rust's allocator aborts. The studio must treat a dead worker as
recoverable, reporting it and re-instantiating rather than hanging.

Cancellation is cooperative: an atomic flag polled in the solver's outer loop.
No crate packages this; it is code we write. Without cross-origin isolation the
flag cannot be shared, so `worker.terminate()` plus re-instantiation is the
fallback, and it is the always-available one.

## Testing

The engine's 644 tests already cover correctness and are unaffected; this layer
must not become a way to regress them.

- **wasm build guard.** There is no CI in this repository at all, and the entire
  interface plan rests on a target nothing verifies. A workflow building
  `wasm32-unknown-unknown` and running the suite is a prerequisite, not a
  follow-up.
- **Worker protocol tests** against the message contract, without a browser.
- **Numerical agreement**: a model solved in wasm must match the native solve.
  The spike already showed identical nonzero counts; objectives should be pinned
  the same way.
- **Feature-unification guard.** A build that accidentally pulls HiGHS into the
  wasm graph fails loudly rather than at link time.

## Scope for v1

Load a network, render it, edit a parameter, re-solve, see flows and prices
update. That is the thesis end to end and the smallest thing that tests it.

Explicitly not in v1: 3-D views, multi-user anything, capacity expansion
authoring UI, and threads.

## Alternatives rejected, with reasons

**Everything on `wasm32-unknown-emscripten`** — would give threads and a
directly linkable HiGHS in one move, but costs the entire `wasm-bindgen`
ecosystem, and with it egui, Dioxus and Leptos on the web.

**HiGHS as a second Emscripten module bridged through JS** — deferred rather
than rejected; see the solver section. `highs-js` is real and maintained, and
the marshalling cost turned out to be negligible. It waits on the `@next` CSC
API, because the stable text API cannot carry our matrices.

(A note for anyone following the trail: `fuglede/highs-wasm` is frequently cited
as a second option and **no longer exists** — the repository and its demo both
return 404, with only a 2022 archive snapshot remaining. Search engines still
index it. `lovasoa/highs-js` is the only live one.)

**Server-side solving** — rejected by requirement. It would handle any size, and
it would also discard the one property that distinguishes this from every
existing tool.

**Leptos** — its maintainer stated in May 2026 that it is "lightly maintained"
and that he considers it complete (leptos-rs/leptos#4707).

**Bevy** — still single-threaded on the web by its own documentation, and its UI
is game-shaped rather than tool-shaped.

## The web solver, resolved

Nodal prices are the reason this engine has its own simplex, so **exact row
duals are non-negotiable**. That single requirement eliminates most of the
field.

| Option | Duals | Integers | wasm | Licence | Verdict |
| --- | --- | --- | --- | --- | --- |
| our simplex | yes | yes (B&B) | native | AGPL, ours | the baseline |
| `clarabel` 0.11.1 | **yes** | **no** | yes | Apache-2.0 | continuous only |
| `microlp` 0.5 | **no** | yes | yes | Apache-2.0 | disqualified |
| `ellp`, `ripped`, `rustplex`, `minilp` | no | — | yes | mixed | disqualified |
| `highs-js` (Emscripten) | yes | yes | separate module | MIT | see below |
| `glpk.js` | yes | yes | separate module | **GPL-3.0** | disqualified |

Two findings decide it.

**Only Clarabel returns duals among the pure-Rust crates**, and it has no
integer variables, so it cannot do unit commitment. It is a partial answer at
best: a faster continuous path, never a replacement.

**`glpk.js` is disqualified on licence, not merit.** It is small (294 KB) and
returns duals, but it is GPL-3.0. This project is AGPL-3.0-only *with commercial
dual-licensing* (`COMMERCIAL.md`), and dual-licensing requires that every
component be ours or permissive. A GPL dependency forecloses that permanently.

**`highs-js` is the only credible external option** — MIT, actively maintained
(last commit 26 July 2026), single-threaded, 826 KB brotli. But its **stable API
takes CPLEX LP *text***, and generating that text costs ~68 ms and 20 MB of
characters per million nonzeros before HiGHS parses anything. That is
disqualifying at our sizes. The `@next` prerelease adds a `createModel` CSC path
taking `Int32Array`/`Float64Array` directly, which is the one worth having — and
it is a prerelease with no announced stable date.

Marshalling itself is **not** the obstacle it was assumed to be: copying between
two wasm heaps runs at ~47–55 GB/s, so 10 M nonzeros costs 2.57 ms each way.
Two heaps cannot be shared — two allocators over one address space collide, and
`highs-js` explicitly declines to expose raw pointers — so it is always a copy,
and the copy is cheap.

**Decision for v1: our own simplex, with warm starting.** It already returns
duals and handles integers, it needs no JS bridge and no prerelease dependency,
and warm starting is a larger win on the edit→resolve loop than swapping
solvers. `highs-js@next` behind the `Solver` trait is the planned escape hatch
once its CSC path stabilises; the trait exists so that swap is not a rewrite.

**Ruled out entirely:** WebGPU compute for LP. WGSL has no `f64` — only `f32`
and `f16` — and single-precision shadow prices are not usable for nodal pricing.
No Rust PDLP/PDHG implementation exists at all.

## Open questions
2. **The warm-start design.** Whether to expose a basis in `Solution`, and
   whether a dual simplex is required or a primal re-solve from the parent basis
   suffices for the common edits.
3. **Vercel header behaviour**, if isolation is ever wanted. Two claims remain
   unverified: whether `.wasm` is served as `application/wasm` by default, and
   whether the Edge Network preserves COOP/COEP. Both are settled by one
   `curl -I` against a test deployment.
4. **Practical wasm memory ceiling in 2026 browsers.** Only 2020-era V8 figures
   (2 GB default, 4 GB opt-in) could be sourced. Our full-year model needs
   ~1.5 GB and is verified to run, but the headroom above it is unknown.
