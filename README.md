# gridwright

A fast engine for building and solving cross-border energy system optimisation
models, written in Rust.

A *wright* is a builder, and that was the original distinction: HiGHS and Gurobi
already solve linear programs well, and the bottleneck the literature complains
about is **building** them, not solving them.

That is still the main claim, and it is no longer the whole of it. There is now
a bounded-variable revised simplex here too, because the interface this is
heading towards runs in a browser and every mature LP solver is a C or C++
library. And the AC formulation needed a spatial branch and bound of its own,
because the tightening it does is specific to the relaxation rather than
something a general solver could offer. So: mostly a builder, with the two
pieces of solver that had to be written because nothing else would do.

## Why

Models that inform national decarbonisation policy are routinely made *less
accurate on purpose*. PyPSA-Eur's own guidance recommends clustering the
European network down to a couple of hundred nodes, and realistic resolution
needs a commercial Gurobi licence. The reason is not the solver. From the
energy modelling literature:

> Python is well known for being user-friendly, but when analyzing memory
> consumption and speed for **building optimization problems**, it was
> considered **non-competitive** compared to tools based on Julia or C++ — a
> bottleneck which also hinders large-scale optimization.

So spatial fidelity gets traded away, in models that decide where grids and
renewables get built. gridwright attacks the stated bottleneck: construction,
not solving.

**That quote has since been tested here, and it did not hold.** Building the
same model, Python through PyPSA reached 5.59 M nonzeros per second against
JuMP's 2.82 M: Python was about twice as fast as Julia, not non-competitive
with it. What survives is the observation underneath it, that a general-purpose
modelling layer is slow at this regardless of the language it is written in,
and that is what this engine is not. The numbers are in
[the comparison](#the-comparison-this-project-was-founded-on).

## What that actually buys, and what it does not

On the same model, this builds in 0.096 s on 1.50 GB where linopy takes 1.54 s
on 3.45 GB and JuMP takes 10.34 s on 5.08 GB. That is sixteen times faster than
linopy and a hundred times faster than JuMP, which is worth having and is less
than this section used to claim: the earlier figure of two thousand times came
from a benchmark script of ours that used linopy badly, and
[the comparison](#the-comparison-this-project-was-founded-on) sets out what
went wrong.

**It does not make a hard solve tractable.** On a full year at hourly
resolution the solve dominates completely: construction is 0.0–0.1% of runtime.
A model that takes HiGHS four minutes still takes four minutes. If your model
already solves and you build it once, this buys you nothing you could measure,
and you should use PyPSA, which has a decade of features this does not.

**Nor are the comparison figures alarming on their own.** Ten seconds and five
gigabytes, which is what JuMP costs, are unremarkable on a workstation. Anyone
sceptical that they amount to a crisis is right to be. The case for this engine
does not rest on any single build being painful.

What the difference buys is narrower, and it is about *where* and *how often*
rather than how fast:

- **Whether the model fits at all.** 1.50 GB runs on a laptop, a CI runner or
  a browser tab. The comparable Python path through PyPSA takes 12.13 GB on the
  same model, which is the difference between a machine you have and a machine
  you request. PyPSA-Eur's advice to cluster Europe down to a couple of hundred
  nodes is a memory decision before it is a time decision.
- **Building stops being a per-iteration tax.** One build is not the case that
  matters. A rolling horizon over a year takes 122 builds, a scenario sweep
  takes hundreds, and an interactive edit takes one per change. At 1.55 s each,
  122 windows come to **just over three minutes of pure assembly** before any
  solving happens; at 0.096 s each they come to 12 seconds. Three minutes is
  not a crisis, which is the honest way to put it. It is the difference between
  an edit that redraws and an edit you wait for.
- **It runs where a Python stack cannot.** The whole engine, the format layer
  and a solver compile to `wasm32-unknown-unknown`, so the model can be built
  and solved in a browser tab with no server at all.

That is a much narrower claim than "construction is the bottleneck", and it is
the one the measurements in [Scale](#scale) actually support. Those
measurements are on synthetic topologies and say only how the problem grows;
correctness is validated against real published networks, which is a different
question and is treated as one.

## What it does today

**Dispatch.** Linear optimal power flow across a multi-country network:

- nodal energy balance at every bus in every snapshot
- DC power flow for AC lines, transport limits for controllable HVDC links
- storage with round-trip efficiency and cyclic state of charge
- variable renewable availability profiles
- load shedding priced at the value of lost load, so an unservable system
  reports *where and when* it failed instead of the word `INFEASIBLE`
- nodal marginal prices, which are what make a model actually useful

**Capacity expansion.** Not just "how should today be run" but "what should we
build", which is the question energy policy actually asks. Generators, storage
and transport links can be extendable, with a capital cost, a floor at the
existing fleet and a ceiling for land and grid connection. Expanding an AC line
is *refused* rather than linearised, because widening a conductor changes its
impedance and the DC flow equation would become bilinear.

**Emissions.** A system-wide CO2 budget as a single constraint. Structurally
unlike everything else here (one row millions of entries wide instead of many
narrow ones), and the lever most decarbonisation questions are asked through.

**Unit commitment.** Thermal plant that cannot run below a stable minimum, with
start-up costs and minimum up and down times. This makes the problem a MILP,
which is why it is opt-in per generator: a continuous relaxation will happily
idle a coal unit at 8% of rating, which understates both cost and emissions.

**Sector coupling.** Buses carry a *carrier*, and a link moves energy between
two of them at an efficiency. An electrolyser is a link from electricity to
hydrogen at 70%; a heat pump is a link to heat at 300%, because it moves heat
rather than making it. One component, because they are the same equation.

**Hydro.** Reservoirs take natural inflow, which arrives whether or not anyone
wanted it, and can spill. Without spill a wet week is simply infeasible, which
is exactly the week a hydro study exists to look at.

**Multi-period investment.** Capacity built in one period is available in every
later one, and costs are discounted per period. Without discounting a model
defers every decision to the final period, which is an artefact of the
arithmetic rather than a finding.

**Beyond Europe.** Buses belong to a *synchronous area*. The United States has
three asynchronous interconnections, Japan has two frequencies, Indonesia has
600+ isolated systems, and the Philippines has three grids. Two things follow
and both are enforced: an AC line may not cross an area boundary, and **each
area gets its own angle reference**. Pinning only one leaves every other area
with a free constant. Asynchronous grids join through HVDC ties, which carry
losses: China's UHVDC loses about 3% per 1,000 km, so 2,000 km arrives 6%
short, and that gap changes where it is worth building anything.

**Ramp limits.** A nuclear station cannot go from quarter load to full in an
hour. Without this every unit is infinitely flexible, which understates both
what it costs to follow a renewable ramp and how much flexible plant a system
needs. The interesting behaviour is that a down-ramp limit binds *before* the
problem appears: rather than run high and be stranded above demand later, the
optimiser produces less earlier.

**Transmission losses.** Real losses go as the square of current, which is not
linear and cannot appear in a linear program at all. What is available is a
marginal rate on the magnitude of the flow, which is what production planning
models use. Absolute value is not linear either, but it is the maximum of two
linear functions, and since loss only ever removes energy the optimiser drives
it down to exactly that bound.

**Hydro cascades.** What an upper station releases becomes the lower station's
inflow, after a travel time. Modelling reservoirs on one river independently
counts the same water twice, which flatters the system's flexibility precisely
when it is scarce.

**Stochastic scenarios.** Two-stage planning: several futures share one
investment decision, operating costs weighted by probability, capital not,
because you build once and then find out which weather year you got.

**N-1 security.** The dispatch must survive losing any single line. Formulated
through line outage distribution factors, which is what makes it affordable: the
obvious approach duplicates every flow variable once per contingency, while LODF
turns each outage into constraints on the flow variables that already exist.
Security costs rows, not columns. Lines whose loss would island the network are
reported rather than quietly ignored. That is a real vulnerability, just not
one flow limits can describe.

**Rolling horizon.** A year of hourly commitment is a MIP with tens of thousands
of binaries and nobody solves it whole. Overlapping windows carry reservoir
levels and commitment states forward, because a window that assumes every unit
starts cold invents start-up costs that were already paid.

**Two solver backends.** HiGHS for scale, and a simplex written for this
project for everywhere HiGHS cannot go.

The precise version of that claim, since the loose one invites an obvious and
correct objection. HiGHS *does* compile to WebAssembly:
[highs-js](https://github.com/lovasoa/highs-js) is an actively maintained
Emscripten build, MIT-licensed, tracking upstream within weeks. What it cannot
do is compile into *this* wasm module — it targets
`wasm32-unknown-emscripten`, which cannot be linked into a
`wasm32-unknown-unknown` Rust binary. Using it means shipping a second wasm
module and marshalling the matrix across a JavaScript boundary between two
separate linear memories.

That turns out to be a good trade rather than a grudging one, and the numbers
are measured rather than assumed. HiGHS under wasm runs at **1.1 to 1.4× its
native time** on LPs of 240k and 960k nonzeros, with identical iteration counts
and objectives, and it returns every row dual. The marshalling that sounds
expensive is not: copying a 10M-nonzero CSC matrix between two wasm heaps takes
about **2 ms**. The real ceiling is Emscripten's 2 GiB heap, which bites around
2.5–3M nonzeros.

The second backend exists because the pure-Rust alternatives that *can* live in
this module do not expose duals. A nodal balance row's dual *is* the price of
energy at that bus, so a browser build without duals would be a model that
cannot answer the question people run it to ask. Ours returns them, compiles to
`wasm32-unknown-unknown` with a single dependency, and is checked against HiGHS
on every IEEE network and on all 118 prices in case118.

Measured in the browser target rather than assumed: the simplex costs **1.3×**
its native time under wasm, consistently across three orders of magnitude, and
construction of a full year at 256 buses — 16.3M variables — takes **266 ms**
inside a wasm module and fits in wasm32's address space.

It used to decline integer problems, which was honest and left a browser build
unable to run unit commitment at all. It now branches: the same relaxation, with
branching by bounds alone, returning the incumbent and the bound separately and
saying whether they met. Verified against HiGHS on a commitment problem
constructed so its relaxation is provably fractional, because many commitment
relaxations come out integral on their own and a test built on one of those
exercises the branching not at all.

**AC power flow.** DC flow is a linearisation, and the things it drops
(losses, voltage magnitudes, reactive power) are often the binding constraints.
So there is a genuine AC formulation too, through the Jabr second-order-cone
relaxation solved with `clarabel`.

Being precise about what that means, because it is easy to overclaim: AC-OPF is
nonconvex and this does not solve it exactly. It solves a convex relaxation
whose optimum is a rigorous lower bound. The relaxation is provably exact on
radial networks, and can loosen on meshed ones. **Whether it came out tight is reported rather than assumed**:
an inexact relaxation returns voltages that
correspond to no physical operating point, and a model that does not say so is
worse than one that cannot do AC at all.

Real IEEE networks solve, with voltages inside their declared bands and
generation exceeding demand by the resistive losses a DC model structurally
cannot see.

**Cycle constraints** tighten it further on meshed networks, following Riccardi,
Bernardelli and Gualandi ([arXiv:2604.00664](https://arxiv.org/abs/2604.00664)).
Jabr constrains each line independently, which is enough on a tree and not on a
loop: around a cycle the relaxation can pick angle differences that do not add
up. The exact fix is that those differences sum to zero around every cycle,
which as written is a sum of arctangents and hopeless. Writing
`W_ij = R + iI = V_i·conj(V_j)` turns it into `Im(W₁W₂…W_k) = 0`, which
McCormick envelopes relax convexly.

Any cycle length, not only triangles. Expanding that identity gives `2^(k-1)`
terms, which is why triangles were the limit; building the product one factor at
a time costs six auxiliary variables per additional line, which is linear. It
matters more than it sounds: a five-bus ring is meshed, has exactly one cycle,
and a triangle-only formulation constrains nothing in it whatsoever. Cycles come
from a spanning forest, so they are a basis:
constraining them constrains every
cycle, because any other is a combination and the identity is additive around
combinations.

The tests check the property that matters: adding the cuts must never *lower*
the bound. A bound that falls means the cuts are invalid, which would turn a
rigorous number into a wrong one.

**Spatial branch and bound** closes the rest. Over a box, `R² + I²` lies under
the affine function through the corners and `uᵢuⱼ` lies over its McCormick
underestimator, so requiring `secant ≥ McCormick` is *implied* by the equality
Jabr threw away: no feasible point is cut off, and both sides collapse onto the
truth as the box closes. On IEEE 57 the relaxation is only a bound at the root
and 33 nodes prove the optimum; on IEEE 118 the cone gap falls twenty-five fold.

One finding worth stating, because it is easy to get wrong in the reassuring
direction: **a small cone gap does not mean a solution is physical.** The cone is
a per-branch statement and says nothing about angles closing around a loop. Both
are measured, and both are folded into the reported status.

**Hydraulic head**, both ways it acts. Power is proportional to the height water
falls through, so a reservoir near empty cannot reach its rating whatever the
gates do; that part is linear in the stored level. The other part is not: a full
reservoir yields more megawatt-hours from the same *volume*, because the volume
drawn per megawatt-hour goes as `1/head` and head depends on the level.

That bilinear half is modelled two ways. Exactly, over bands of reservoir level
with a binary picking the band, following Borghetti, D'Ambrosio, Lodi and
Martello (2008). And approximately, by holding head fixed, solving an ordinary
linear program, recomputing head from the levels that came out and going round again,
which gives up the optimality guarantee and is the only one that scales,
since the exact form is 35,040 binaries for one reservoir over a year.

Both take the level at the *start* of each period. Using the end level makes the
constraint self-limiting, since discharging lowers the level that permits the
discharge, and a brim-full reservoir could never reach its rating.

**Demand that can do more than fail.** Load used to be served or shed, and
shedding is priced at the value of lost load,
a number in the thousands chosen
to mean "never do this". All four ways demand can fail to be served are now
distinct, because they answer different questions and cost different amounts:
shed, shifted to another snapshot with the energy conserved, declined on a
willingness-to-pay curve, or curtailed under an interruptible contract a bounded
number of times.

**Data.** Someone with a network to model has a file, not a format. `load_any`
takes a path, works out what it is from its content and its name, and returns a
network:

| Format | Where it comes from |
| --- | --- |
| CSV directory | The layout PyPSA writes. Reads and writes. |
| Parquet directory | Same layout, columnar. Reads and writes. |
| MATPOWER `.m` | IEEE test cases, PGLib-OPF, RTE's French network, PEGASE. |
| PSS/E RAW, v29–v35 | What North American and most Asian utilities actually run. |
| PSS/E RAWX | The JSON reformulation v35 introduced. |
| PowerModels JSON | The Julia optimisation ecosystem. |
| Native JSON | Lossless both ways, for handing a network to a browser. |
| Spreadsheets | `.xlsx`, `.xls`, `.xlsb`, `.ods`. How much of the world publishes. |
| PyPSA netCDF | The largest open energy modelling ecosystem there is. |
| CIM / CGMES | What European TSOs exchange grids in. |

Every reader returns the network **and a list of what it had to drop**, because
each format carries more than a linear model can hold and each carries a
different more. Nothing is discarded silently.

The conversions that decide whether a reader is useful are the unit
conventions, and they are the ones that fail quietly rather than loudly. A
PowerModels case states a 47.8 MW load as `0.478`; PyPSA and CIM state line
impedance in ohms where the optimisation wants per unit; PSS/E moved its
transformer section between revisions and states winding voltages three
different ways. Each of those produces a network that loads without complaint
and is wrong, so each has a test that pins the number.

The IEEE 14-bus system is carried in five encodings written by five different
tools, and a test asserts they all read to the same network.

The binary formats sit behind feature flags, and detection does not: a build
without Parquet says "this is Parquet and this build cannot read it" rather
than claiming not to recognise the file.

**None of it needs a filesystem.** `load_bytes` takes a name and a buffer;
`load_files` takes a set of them, which is what a multiple-selection picker or
a dropped folder produces, and is the only way to express a CSV directory or a
CGMES model split across its profiles. Every reader is pure Rust
(including netCDF, via a pure-Rust HDF5 implementation, and Parquet, using only
codecs that cross-compile), so the whole format layer builds for
`wasm32-unknown-unknown` alongside the engine and the solver:

```bash
cargo build --target wasm32-unknown-unknown \
    -p gridwright-io -p gridwright-build -p gridwright-simplex \
    --features gridwright-io/all-formats
```

That is what makes the planned interface possible rather than aspirational: a
browser has no filesystem to open, and a library that can only read from disk
could not be the one it imports.

```
$ gw run examples/eu-mini
loaded 4 buses, 4 lines, 7 generators, 4 loads, 1 storage, 24 snapshots
  531 cols, 336 rows, 1128 nonzeros
  build 0.752 ms, solve 2.741 ms
  status Optimal, objective 6043793.50

  capacity built:
    de_solar_new           14240.35 MW
    dk_wind_new               31.63 MW
    de_batt                 3334.06 MW
```

Tighten the carbon budget on that example and the investment shifts, which is
the whole point of having the lever:

| CO2 budget | Solar built | Wind built | Battery built | Cost |
| --- | --- | --- | --- | --- |
| none / 300 kt | 14,240 MW | 32 MW | 3,334 MW | 6.044 M |
| 50 kt | 14,666 MW | 278 MW | 3,760 MW | 6.086 M |

```
$ gw demo
two countries, one 50 MW interconnector, 80 MW of German demand

  status:     Optimal
  total cost: 6800

  de_coal        30.0 MW  at  40.0/MWh
  fr_nuclear     50.0 MW  at  10.0/MWh

  flow DE->FR:  -50.0 MW  (negative means power arriving in DE)

  marginal price by country:
    DE     40.0/MWh
    FR     10.0/MWh
```

## Validated against real networks

`examples/pglib` holds the IEEE 14, 30, 57, 118 and 300 bus cases as
distributed by the IEEE PES Power Grid Library under CC BY 4.0. They are not
decoration: a synthetic topology cannot tell you the physics is wrong, because
it was generated by the same assumptions being tested.

```
$ gw case examples/pglib/case300_ieee.m
case300_ieee
  300 buses, 411 branches, 69 generators, 199 loads, 1 synchronous areas
  note: baseMVA 100; reactive power and voltage limits ignored (DC model)
  1080 cols, 710 rows, 2421 nonzeros
  build 1.790 ms, solve 7.403 ms
  status Optimal
  DC-OPF cost 516262.58
  demand 23525.8 MW, generation 23525.9 MW
```

What the tests check on every one of those networks: generation balances demand
exactly, no branch exceeds its rating, no generator violates its limits, each
synchronous area has exactly one pinned angle, repeated solves agree, and
`f = B(θ₀ − θ₁)` holds on **every DC branch** against the solved angles. That
last one is the real check. On a network with reactances spanning orders of
magnitude, parallel branches and radial spurs, a sign error or a transposed
index cannot survive it, where it would pass unnoticed on a symmetric triangle.

**Not claimed:** agreement with published AC-OPF objectives. This is a DC model
and generator costs are the linear term of a published quadratic, so the
numbers are not comparable and saying otherwise would be worse than saying so.

## Measured

MacBook, release build. Synthetic networks: ring plus chords, three generators
per bus, storage on every fourth bus, DC power flow throughout.

| Network | Columns | Rows | Nonzeros | Construction |
| --- | --- | --- | --- | --- |
| 256 bus × 168 h | 311,808 | 118,272 | 559,104 | **3.3 ms** |
| 256 bus × 8760 h | 16,258,560 | 6,167,040 | 29,153,280 | **96 ms** |
| 512 bus × 8760 h | 32,517,120 | 12,334,080 | 58,306,560 | **190 ms** |

The 256 × 8760 row is the median of five runs: 119.7, 94.6, 96.6, 96.2, 94.6 ms.
It had been quoted variously as 89, 96, 100 and 102 ms in different places in
this repository — one measurement wearing four numbers, each a single reading.
Note the first run again, 119.7 ms against a 96.2 ms median: that is the
construction first-run penalty described under [Scale](#scale). The other two
rows are single readings and are not promoted to more than that.

Peak resident memory for the 256 × 8760 case is **1.50 GB**, and construction
includes the transpose: the model is assembled straight into the column major
form the solvers take, so there is no second pass to charge for. It used to be
1.95 GB and a further 79 ms, before the merged row major matrix was removed.
Nothing downstream had ever read it.

That row has been rebaselined twice, and both are reported rather than quietly
absorbed. It was 89 ms before commitment, losses, cascades and multi-period
capacity were added, which cost about 12% and seems a fair price. Folding the
transpose in adds 6 ms here and removes a separate 79 ms pass, so the comparable
figure is now what it takes to reach a matrix a solver can read.

Measured both ways on the same machine, that is what the change was worth:

| Buses | Build, before | Transpose, before | **Total before** | **Now** |
| --- | --- | --- | --- | --- |
| 64 | 27.5 ms | 19.4 ms | **46.9 ms** | **28.8 ms** |
| 128 | 48.8 ms | 42.8 ms | **91.6 ms** | **53.6 ms** |
| 256 | 94.2 ms | 79.1 ms | **173.3 ms** | **100.0 ms** |
| 512 | 188.8 ms | 173.5 ms | **362.3 ms** | **189.6 ms** |

Between 1.6× and 1.9× faster to a solver-ready matrix at every size, and the
scaling stays close to linear at about 1.88× per doubling. Best of three runs,
and best-of-N rather than a single reading is the method to use here: the
assembly is several full-width parallel regions and each ends when its slowest
thread does, so one busy core elsewhere on the machine moves the number more
than any of these changes did.

**This has since been measured against linopy, JuMP and PyPSA** on a matrix
whose counts match exactly, and the numbers are in
[The comparison this project was founded on](#the-comparison-this-project-was-founded-on)
with the method in [`benchmarks/head_to_head.md`](benchmarks/head_to_head.md).
They did not come out the way this section originally anticipated: the ratio
against linopy fell from a published two thousand to about fifteen once the
benchmark script was fixed. This paragraph previously said the comparison did
not exist, which stopped being true and is corrected here.

**When this does not matter.** Run `gw bench --solve` at a modest size and the
build share of total runtime is around 0.1%. HiGHS takes seconds; construction
takes milliseconds. If your model already solves comfortably, making
construction faster buys you nothing, and you should use PyPSA, which has a
decade of features gridwright does not.

The case for this engine is narrower and worth stating precisely. It is for
the models that are currently *not being run*: the ones clustered down from
thousands of nodes to hundreds because the full problem cannot be built in
available memory, or cannot be built at all. There, construction is not 0.1%
of the runtime; it is the reason the run does not happen. Fast is a means to
that end rather than the point, and a build that finishes in 100 ms is really a
claim about the 1.50 GB it did not need.

## How

Two phases with a hard line between them.

**Allocate.** Every variable block is handed out up front, sequentially. One
contiguous block per component spanning all snapshots, so a component's whole
trajectory is a slice rather than a gather, both going in and coming out.

**Assemble.** Every constraint family is generated in parallel into per-thread
row batches, then transposed once, directly into the model's column major
matrix. This works because after allocation every variable index is a pure
function of its block and offset: a thread building balance rows for bus 400
needs no coordination to know where generator 12's dispatch at snapshot 900
lives.

Three decisions do most of the work.

**Time series are component major.** Availability for generator `g` is a
contiguous run, exactly the order the bounds vector needs it in. Nodal balance
appears to want the opposite layout, so balance is parallelised over *buses*
rather than snapshots. That is equally valid, since both axes are
independent, and it
keeps every read sequential.

**The transpose is a parallel counting sort, and it reads the batches
directly.** Column indices are already integers, so there is nothing to compare:
count, prefix sum the counts into offsets, scatter. The batches are the unit of
parallelism, since they are already one per builder thread. There is no merged
row major matrix in between, because nothing would read it:
375 MB of a large
model, and a serial 79 ms pass, spent on a representation with no consumer.

**The matrix reaches HiGHS as three pointers.** `Highs_passModel` accepts CSC
directly. The safe `highs` wrapper crate was deliberately not used: its builder
takes rows one at a time, which would mean disassembling the matrix we just
assembled in order to rebuild it inside someone else's representation.

## Layout

| Crate | Purpose |
| --- | --- |
| `gridwright-model` | Sparse LP core. Variable blocks, row batches, CSC transpose. |
| `gridwright-net` | Network domain: buses, lines, generators, storage, loads. |
| `gridwright-build` | Parallel LP assembly. |
| `gridwright-simplex` | Our own bounded-variable simplex: sparse LU, branch and bound, duals, compiles to WASM. |
| `gridwright-acopf` | AC optimal power flow: Jabr relaxation, cycle constraints, spatial branch and bound. |
| `gridwright-solve` | Solver trait, with HiGHS and pure-Rust backends. |
| `gridwright-io` | Every data format, in and out, and result export. |
| `gridwright-emissions` | Production and consumption carbon accounting, average and marginal. |
| `gridwright-cli` | The `gw` binary. |
| `gridwright-worker` | Reading and solving behind one bytes-in interface, so the browser and the native app share a path. |
| `gridwright-studio` | The interactive shell: a network view and a solve, in a window or a tab. |

## The studio

There is an interface now, and it runs in a browser with no server: the model is
built and solved in the tab, which is the whole reason the simplex in
`gridwright-simplex` exists.

```sh
cargo run -p gridwright-studio -- examples/pglib/case14_ieee.m   # a window
./crates/gridwright-studio/build-web.sh                          # a tab
```

It draws buses as busbars with circuits tapping onto them, generators and loads
as their IEC symbols, and — once solved — nodal price on the busbars and
utilisation on the corridors, with a scrubber for networks that have a horizon.
`⌘K` goes to any bus by name.

See [`crates/gridwright-studio/README.md`](crates/gridwright-studio/README.md),
which also lists what it does not do yet.

## Build

Needs a Rust toolchain and `cmake`, since HiGHS is built from source.

```bash
cargo build --release
cargo test --workspace --all-features   # 474 tests
./target/release/gw demo
./target/release/gw run examples/eu-mini --out results/
./target/release/gw case examples/pglib/case118_ieee.m
./target/release/gw bench --buses 256 --hours 8760 --solve
```

## Correctness

The tests are arithmetic rather than snapshots, because a dispatch model that
is fast and wrong is worthless:

- cheap imports displace expensive local generation, to the exact MW
- prices separate across a saturated interconnector, 40 against 10 per MWh,
  which is what market splitting means
- on a triangle of equal susceptance, power divides 2:1 between the direct and
  the two-hop path, which is the DC power flow physics rather than
  merely its plumbing
- storage covers a generator outage by having charged beforehand
- the parallel transpose agrees with the serial one byte for byte at scale, and
  transposing the batches directly agrees with merging them first and transposing that, the operation it replaced
- repeated builds of the same network produce identical matrices, so results
  never depend on how the thread pool happened to schedule
- capacity is built exactly to the analytic break-even and not a MW past it,
  checked by straddling the threshold from both sides
- a carbon cap substitutes clean for dirty by precisely the binding amount, and
  a slack cap changes nothing, so the test can tell the two apart

Two of these tests were wrong before the code was. A transport loop turned out
to have no unique flow solution, and a carbon cap turned out never to bind
because the clean option was cheap enough to build on economics alone. Both
now assert what is actually determinate, and say why in the test itself.

## Status

Early, and the formulation now covers rather a lot: dispatch, capacity
expansion, unit commitment over a rolling horizon, sector coupling, hydro with
inflow and spill, multi-period investment, N-1 security, planning reserve, and
budgets for carbon, water and land.

Everything the previous version of this section listed as absent is now in.
Head's effect on energy *conversion*
(the bilinear half, where a full reservoir yields more megawatt-hours from the
same volume) is modelled two ways: exactly, over bands of reservoir level with a binary picking the band, and
approximately, by holding head fixed and iterating to a fixed point, which is
what scales. Cycle constraints run to any length, since building the product one
factor at a time costs six variables per additional line where writing the
identity out costs `2^(k-1)` terms. Apparent-power line limits are second-order
cones, and the AC model had carried no thermal limits at all before them.

Demand is no longer served-or-shed. All four ways it can fail to be served are
distinct now, and they answer different questions: shed at the value of lost
load, shifted to another snapshot with the energy conserved, declined on a
willingness-to-pay curve, or curtailed under an interruptible contract a bounded
number of times.

**What is absent.** Nothing writes CIM, and a non-conformant CGMES file is worse
than none. Time series are read whole into memory rather than streamed.

The larger gaps are in evaluation rather than in features, and they are set out
in [What would make this convincing](#what-would-make-this-convincing). The
short form: every scaling number here is a synthetic ring, and the only
head-to-head is against the tool this project's founding quote criticises
rather than against the tools it recommends.

**What has been measured and deliberately not built.** Forrest-Tomlin updates,
which address 2.3% of a solve. A fill-reducing column ordering, which halves the
fill and costs more than it saves. Partial pricing, which is implemented and
switched off because a cheaper scan buys a worse entering variable and the two
cancel. Each is recorded with its numbers in `TODO.md`, so nobody repeats them.

**Validation.** Correctness is checked against networks nobody here designed:
the IEEE cases from PGLib, and PEGASE 1354, a real European system four times
the size of the largest of them. The from-scratch simplex agrees with HiGHS on
every one, including all 118 nodal prices of case118.

Most scaling benchmarks use synthetic topologies and are labelled as synthetic
wherever they appear. That is no longer the only evidence: real PGLib topologies
carrying a real year of hourly data have since been measured against
size-matched rings, and the ring turns out to flatter the solve by between 1.3
and 7 times. See gap 1 below.

## What would make this convincing

An honest account of what the evidence here does not yet cover, because the
answer to "is this actually useful" is currently "the measurements do not
settle it". Four gaps, in the order they matter. Three have since been closed
and are kept with their results rather than deleted, because what a gap turned
out to contain is more informative than the fact that it once existed — and in
two of the three cases the answer went against this project.

**1. ~~Every scaling number is one synthetic ring.~~ Measured, and the ring was
flattering the solve.** The worry was that a ring's regular structure, uniform
ratings and identical plant at every bus keep the basis sparser than a real
network's radial spurs, unequal impedances and meshed cores would. It does. Real
PGLib topologies against rings matched to the same column count:

| Case | Real solve | Ring solve | Ratio | nnz/col real | nnz/col ring |
| --- | --- | --- | --- | --- | --- |
| IEEE 14 | 7.2 s | 5.7 s | 1.27× | 2.21 | 1.66 |
| IEEE 57 | 305.3 s | 42.0 s | 7.27× | 2.26 | 1.65 |
| IEEE 118 | 787.3 s | 191.6 s | 4.11× | 2.27 | 1.65 |

Real networks carry about 2.25 nonzeros per column against the ring's 1.65, and
cost between 1.3 and 7 times more to solve at matched size. Every synthetic
scaling number elsewhere in this document should be read as optimistic by that
much.

**This is n = 2, and it is the one core table not on the n=5 standard the rest
of this document now holds itself to.** One pass is fifty minutes, so five is
over four hours; it is deferred rather than skipped. The test reports its own
spread, 0 to 1% on the solve figures, but two agreeing samples were mistaken for
a tight distribution twice elsewhere in this project and both dissolved at n=5,
so treat that as an absence of evidence rather than as precision. Two rows were
also taken while the solve itself drove the machine's load above the threshold
the test sets for itself, making them upper bounds. The ratio is not monotone
either. Read these as "several times", to one significant figure, and not as a
trend.

**2. ~~There is no real year of time series on a real network.~~ Assembled.**
Open Power System Data's 2019 hourly series for the four German control zones
are fetched by [`benchmarks/fetch_opsd_time_series.py`](benchmarks/fetch_opsd_time_series.py)
and mapped onto PGLib buses by zone, which is normal practice and is stated
rather than assumed away. The distilled file is not committed: the upstream
source is 130 MB, and the derived series comes from ENTSO-E transparency data
whose redistribution terms are less explicit than the CC-BY 4.0 the PGLib cases
carry. Run the script once and the measurement above reproduces.

**3. ~~The head-to-head is against the wrong tool.~~ Measured, and it cost us
a headline.** The founding quote recommends Julia, and this project had only
ever measured itself against `linopy`, which is Python. JuMP has now been
measured: gridwright is about a hundred times faster on identical counts, so
that comparison went the way the project hoped. The quote did not: Python built
a larger matrix per second than Julia did. And the measurement turned up that
our own linopy script was unfair, which narrows the published linopy ratio from
two thousand to about **sixteen**. (An earlier version of this sentence said "to
about a hundred", which conflated two different comparisons: a hundred is the
JuMP ratio, not the corrected linopy one.) See
[the comparison](#the-comparison-this-project-was-founded-on).

**4. Nobody has run a study with it.** Every number here is a microbenchmark or
a property test. The claim that a fast build enables interactive editing,
scenario sweeps and in-browser use is untested as a *workflow*: nobody has sat
down, built a model, changed their mind, and rebuilt. That is the evaluation
that would decide whether the engineering was worth doing, and it needs the
interface, which is why the interface is the next thing.

Two further things would sharpen the picture without settling it: a comparison
against PyPSA's own path to a matrix, which is what a user actually experiences
rather than what linopy does in isolation, and a measurement of how construction
cost behaves as a *fraction of a real study* rather than of a single solve.

## The comparison this project was founded on

The premise was a claim from the energy modelling literature: that Python is
"non-competitive" at *building* optimisation problems. This section used to
report a two-thousand-fold speed-up over `linopy` and support that claim.

**Both the ratio and the claim turned out to be wrong, and the fault was ours.**
What follows is the corrected version. The history is kept because a published
number that moved by two orders of magnitude should not quietly become a
different number.

### What was wrong

`benchmarks/linopy_build.py` wrote the nodal balance as `(p * g_at).sum("gen")`
against a dense generators-by-buses array. That multiplies a `(gen, snapshot)`
variable by a `(gen, bus)` array and builds a `(gen, bus, snapshot)`
intermediate: 768 x 256 x 8760 entries, to express a sum in which all but three
terms per bus are zero. The DC flow constraint had the same shape.

It looks like ordinary xarray, and it is not what the library is for. Writing
the same model with `groupby` for the per-bus sums and indexed `sel` for a
line's two ends, which is what PyPSA does, produces **the identical matrix** and
is 130 times faster. The script now does that by default, asserts both paths
build the same matrix, and keeps the slow one behind `--dense` so the figure
stays reproducible and the trap stays visible.

### The corrected numbers

Same model, same machine: a synthetic 256-bus ring over a year at hourly
resolution. Only construction is timed, to a matrix a solver could read.

| | gridwright | linopy 0.9.0 | JuMP 1.31 | PyPSA 1.2.4 |
| --- | --- | --- | --- | --- |
| Variables | 16,258,560 | 16,258,560 | 16,258,560 | 14,016,000 |
| Nonzeros | 29,153,280 | 29,153,216 | 29,153,280 | 58,148,880 |
| Construction | **0.096 s** | **1.54 s** | 10.34 s | 10.39 s |
| Peak memory | **1.50 GB** | **3.45 GB** | 5.08 GB | 12.13 GB |

**Read this table with its limitations attached.** PyPSA's counts do not match
and were never going to — it formulates transmission through cycle flows rather
than voltage angles, and emits fixed capacity limits as rows where gridwright
puts them in bounds — so its column is a different problem and belongs in a
per-nonzero comparison rather than a wall-clock one. The earlier version of this
table said "matched" for both PyPSA counts, which was wrong;
[`head_to_head.md`](benchmarks/head_to_head.md) has always carried the real
figures and the reconciliation.

Three further caveats, none of which the numbers above carry on their face. The
head-to-head was taken **on a machine that was not idle** — two of fourteen
cores were busy — which biases against gridwright alone, since it is the only
one of the four that builds in parallel, but is still not the standard the rest
of this document now holds itself to. The repetition count **varies by
condition** (best of 5, best of 3, and best of 1 for every peak-memory figure),
and **no spread is reported for any of them**. And they are best-of-N, which is
a floor rather than an expectation. That comparison has not yet been re-run
under `benchmarks/measure.sh` at n=5; until it is, treat one significant figure
as the resolution these ratios support.

Against linopy used properly that is **about 16 times faster on 2.3 times less
memory**, not two thousand times on a fifteenth. Against JuMP it is about a
hundred times, on counts that match exactly — though see the caveat above about
what resolution these ratios actually support.

### What the corrected numbers still support, and what they do not

**They still support the engineering.** Fifteen times is worth having when a
study builds the model repeatedly, and the memory figure is what decides
whether a model fits at all. Both reasons are set out in
[What that actually buys](#what-that-actually-buys-and-what-it-does-not) and
neither depended on the ratio being large.

**They do not support the founding quote.** It says Python is non-competitive
against Julia. Here Python built a larger matrix per second than Julia did:
5.59 M nonzeros per second through PyPSA against 2.82 M through JuMP. The
language is not the variable. A general-purpose modelling layer is slower at
this than a purpose-built assembler, and JuMP and linopy are both the former.

**And they do not fully clear JuMP.** The linopy figure was wrong because this
project wrote the linopy benchmark, and the same could be true of the JuMP one.
Three JuMP construction styles were tried and agreed within 3%, which is some
protection, but linopy was only caught because PyPSA gave an independent
implementation to check against and JuMP has no equivalent here. Treat the 100x
as unconfirmed in the way the 2000x turned out to be.

Method, counts and caveats: [`benchmarks/head_to_head.md`](benchmarks/head_to_head.md).
These timings were taken while the machine was busy. Re-running them under
`benchmarks/measure.sh` at n=5, which is the standard the rest of this document
now meets, is **outstanding work and not yet done** — an earlier version of this
paragraph said the re-run was in progress, which it was not.

**The memory number matters more than the speed one, and it too is smaller than
this section once claimed.** An earlier version argued it as "two hundred seconds
is an annoyance; 22 GB is where a laptop stops" — but 200 s and 22 GB are the
*as-scripted* linopy figures, the ones this very section identifies as an
artefact of the benchmark script and replaces. Arguing from them after
retracting them is the same error as quoting the ratio they produced.

Measured against linopy used properly, the memory advantage is **1.50 GB against
3.45 GB, about 2.3×**. Against JuMP it is 3.4×, and against PyPSA 8× on wall
figures — though PyPSA builds a matrix twice the size, so the like-for-like
comparison there is 54 bytes per nonzero against 209, about 3.9×.

A factor of two to four in memory is worth having and is not a laptop-stopping
difference on this model. Where it becomes one is extrapolation: the same ratio
applied to a problem already near a machine's ceiling is the difference between
a run and no run. That is a claim about headroom rather than a measurement, and
it is stated as such. It is the same conclusion the [Scale](#scale) section
reaches from the other direction: the fast build does not make the *solve*
tractable, and what it actually buys is that the model fits, that it can be
rebuilt interactively, and that a scenario sweep assembling it hundreds of times
is not absurd.

Caveats, since the number is flattering. This is linopy 0.9.0 and a later
version may differ. linopy also does more than construct: it keeps a symbolic
model you can inspect and modify afterwards, which gridwright deliberately does
not. And the topology is synthetic, for the reason given below.

## Scale

Measured on synthetic topologies, and labelled as such wherever it appears.
Correctness is validated against real networks; these say only how the problem
grows.

**The ring flatters the solve, and now by how much.** A synthetic ring has
degree two and a banded matrix; a real network is meshed with variable degree
and carries 2.2 to 2.3 nonzeros per column against the ring's 1.65. Measured
against real topologies at matched column count, with a real year of German
hourly demand and renewable output attached:

| case | columns | real network | matched ring | |
| --- | --- | --- | --- | --- |
| IEEE 14 | 543,120 | 7.2 s | 5.7 s | 1.3× |
| IEEE 57 | 2,049,840 | 303.9 s | 41.8 s | **7.3×** |
| IEEE 118 | 4,826,760 | 788.8 s | 191.0 s | **4.1×** |

So every solve time in this section should be read as **four to seven times
optimistic** for a real network of the same size. The ratio is not monotone, so
that is a range rather than a trend. The ring side of that table reproduces the
figures below to within 5%, which is what makes the comparison a comparison
rather than a story about one afternoon.

Construction is unaffected: it is linear in what is written either way.

These rows have been re-measured on an idle machine and reproduced closely: the
ratios were 1.28, 6.99 and 4.03 then, and are 1.28, 7.27 and 4.13 now, with
solve spreads of 0 to 3%. This is the one measurement in the project that the
machine's state did not distort, which is worth saying because it is the one
carrying the most weight.

A full year at hourly resolution, solved whole, every rung run to completion.
**Five runs on an idle machine, every observation listed**, quoted figure is the
median:

| Buses | Columns | Build | Solve, all five (s) | Median | Growth |
| --- | --- | --- | --- | --- | --- |
| 8 | 402,960 | 4.4 ms | 3.4, 3.4, 3.5, 3.6, 3.8 | **3.5 s** | |
| 16 | 805,920 | 5.2 ms | 9.9, 10.5, 10.7, 10.9, 11.1 | **10.7 s** | 3.1× |
| 32 | 1,611,840 | 8.6 ms | 29.4, 30.0, 30.1, 31.8, 32.7 | **30.1 s** | 2.8× |
| 64 | 3,223,680 | 15.5 ms | 104.9, 106.8, 107.1, 109.5, 117.6 | **107.1 s** | 3.6× |
| 128 | 6,447,360 | 27.9 ms | 331.6, 335.8, 339.1, 358.0, 365.6 | **339.1 s** | 3.2× |

**This table has been wrong three times.** Twice because it was measured on a
busy machine: the first version reported the 64-bus case as "did not finish in
seven minutes", and the second, measured while six agents were running, put 128
buses at 561.8 s.

The third time is subtler and worth more. The corrected version was quoted as
**best of two runs**, and best-of-N is a floor rather than an estimate. Against
five runs the medians above sit 3 to 13% higher on every rung — 3.2 → 3.5,
9.5 → 10.7, 28.3 → 30.1, 101.1 → 107.1, 314.6 → 339.1. Best-of-N is the right
statistic for comparing two implementations, since taking the fastest of each
strips noise that belongs to neither. It is the wrong one for "what will this
cost me", and this table was using one number to answer both questions.

That version also claimed the figures repeat to **within 1.4%**. They do not: at
n=5 the spread is **11 to 12%** on every rung. The 1.4% came from a pair of runs
that happened to agree, which is what two samples cannot be distinguished from a
tight distribution. Every number here now comes through
`benchmarks/measure.sh`, which refuses to start while anything else of ours is
running and records the load before and after each run — and every observation
is printed, because a reader can compute any summary they like from five values
and can recover nothing from one.

Growth is about 3× per doubling and roughly flat, not the 5× rising at the top
that an earlier version showed; that apparent acceleration was the busy 128
row. It was originally published as a flat 9.5×.

Construction is 0.008% of runtime at 128 buses. **At full resolution the fast
build still buys almost nothing**, and that conclusion is untouched by the correction:
it never rested on the solve being intractable.

The same year through a rolling horizon of 96-hour windows keeping 72:

Five runs, same session and same machine as the table above, so the two columns
are comparable rather than assembled from different days:

| Buses | Windows | Rolling, all five (s) | Median | Whole (median) | |
| --- | --- | --- | --- | --- | --- |
| 16 | 122 | 3.92, 3.92, 3.95, 3.99, 4.14 | **3.95 s** | 10.7 s | 2.7× |
| 32 | 122 | 8.08, 8.30, 8.35, 8.36, 8.69 | **8.35 s** | 30.1 s | 3.6× |
| 64 | 122 | 21.82, 22.55, 22.77, 23.60, 23.63 | **22.77 s** | 107.1 s | 4.7× |
| 128 | 122 | 68.68, 68.73, 72.28, 73.62, 74.77 | **72.28 s** | 339.1 s | 4.7× |

The previous version claimed **23× at 32 buses, and that the rolling horizon
"finishes where the monolithic solve does not"**. Neither is true. It is 3.6× at
32 buses, and the monolithic solve finishes at every size tried, including 128.

**It then claimed the advantage compounds to 7.4× at 128 buses, and that was
also wrong.** The 7.4× divided the rolling time by 561.8 s — the very figure
this page identifies four paragraphs earlier as a busy-machine artefact and
discards. Discrediting a number does not discredit what was computed from it,
and nothing here was checking for that.

Measured properly, the advantage **compounds to 64 buses and then flattens**:
2.7×, 3.6×, 4.7×, 4.7×. Rolling still wins by nearly five times at the top,
which is the honest version of the argument and still a claim about
decomposition rather than about this builder. There is no evidence in this
ladder that the ratio keeps climbing, so it should not be extrapolated as
though it does.

On a real topology the advantage is **larger** than the ring suggests, not
smaller: 16.7× and 10.6× on IEEE 57 and 118 with a real year attached, against
3.8× and 4.7× for rings of the same size. Decomposition partly cancels the bad
news above, which is the one place where the ring was pessimistic rather than
optimistic.

Note that the rolling horizon performs **122 builds instead of one**, and
construction still does not register. What the fast build actually buys is set
out in [What that actually buys](#what-that-actually-buys-and-what-it-does-not),
and none of it is "the solve gets faster".

The pure-Rust solver, which is what a browser has. **Medians of n = 5**, spread
1.8 to 5.9%:

| Rows | Time |
| --- | --- |
| 432 | 3.9 ms |
| 864 | 14.8 ms |
| 2,592 | 153 ms |
| 3,456 | 268 ms |
| 7,776 | 1.68 s |
| 13,824 | 5.03 s |
| 20,736 | 11.26 s |

The three largest rungs reproduce their previously published values almost
exactly (0.27 s, 5.1 s, 11.2 s). The 864-row rung does not: it was published as
23 ms and measures 14.8 ms, so that figure had gone stale against a later
improvement rather than being wrong when written. Growth over the whole ladder
is `rows^2.06`.

Two changes account for most of that. Replacing the dense basis inverse with a
sparse LU, where the win was not the sparsity itself but the symbolic step that
finds which earlier pivots actually reach a column. And a structural crash
basis: phase one used to be three quarters of every solve, because the starting
basis was every artificial variable rather than anything to do with the problem.

At the start of this the same 864-row model took 1.2 seconds and 2,592 rows
would not finish inside ten minutes.

## Licence

**AGPL-3.0** ([LICENSE](LICENSE)). Open source: use it, modify it, run it in
production, no permission needed. The AGPL asks that if you modify it and offer
it over a network, you publish your modified source too.

Research, laboratories, regulators, NGOs and anyone publishing their modelling
are unaffected, because they were going to show their working anyway. Companies
that need to keep a surrounding stack closed need a commercial licence instead;
see [COMMERCIAL.md](COMMERCIAL.md). The code is identical either way and nothing
is withheld from the open version.
