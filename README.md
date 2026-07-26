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

On the same model, this builds in 0.104 s on 1.50 GB where linopy takes 200.8 s
on 22.4 GB. Stated on its own, that ratio implies more than actually follows
from it, so the qualification belongs here rather than five hundred lines
further down.

**It does not make a hard solve tractable.** On a full year at hourly
resolution the solve dominates completely: construction is 0.0–0.1% of runtime.
A model that takes HiGHS four minutes still takes four minutes. If your model
already solves and you build it once, this buys you nothing you could measure,
and you should use PyPSA, which has a decade of features this does not.

**Nor is 22.4 GB alarming on its own.** On a workstation it is unremarkable,
and 200 seconds is an annoyance rather than an obstacle. Anyone sceptical that
those two numbers amount to a crisis is right to be.

What the difference buys is narrower, and it is about *where* and *how often*
rather than how fast:

- **Whether the model fits at all.** 1.50 GB runs on a laptop, a CI runner or
  a browser tab; 22.4 GB needs a machine you have to book. PyPSA-Eur's advice
  to cluster Europe down to a couple of hundred nodes is a memory decision
  before it is a time decision.
- **Building stops being a per-iteration tax.** One build is not the case that
  matters. A rolling horizon over a year takes 122 builds, a scenario sweep
  takes hundreds, and an interactive edit takes one per change. At 200.8 s
  each, 122 windows come to **6.8 hours of pure assembly** before any solving
  happens; at 0.104 s each they come to 13 seconds. That is where the ratio
  stops being a curiosity.
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
project for everywhere HiGHS cannot go. The second exists for one reason: the
engine needs to run in a browser, HiGHS is C++, and the pure-Rust alternatives
do not expose duals. A nodal balance row's dual *is* the price of energy at that
bus, so a browser build without duals would be a model that cannot answer the
question people run it to ask. Ours returns them, compiles to
`wasm32-unknown-unknown` with a single dependency, and is checked against HiGHS
on every IEEE network and on all 118 prices in case118.

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
| 256 bus × 8760 h | 16,258,560 | 6,167,040 | 29,153,280 | **~100 ms** |
| 512 bus × 8760 h | 32,517,120 | 12,334,080 | 58,306,560 | **190 ms** |

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

**What has not been measured yet:** a head-to-head against `linopy` on an
identical problem. Until that exists the claim here is "builds a 16 million
variable model in under a tenth of a second", which is a fact, and *not*
"faster than linopy", which is so far only an expectation. That benchmark is
the next thing to build, and its numbers get published either way.

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

Scaling benchmarks still use synthetic topologies, and are labelled as synthetic
wherever they appear. The reason is time series rather than topology: real
networks of this size are published, and a real network *with a year of hourly
data attached* in one file is not.

## What would make this convincing

An honest account of what the evidence here does not yet cover, because the
answer to "is this actually useful" is currently "the measurements do not
settle it". Four gaps, in the order they matter.

**1. Every scaling number is one synthetic ring.** A ring with chords has
regular structure, uniform line ratings and identical plant at every bus. Real
networks have long radial spurs, wildly unequal impedances and a handful of
heavily meshed cores. That shape decides how sparse the basis stays, which is
most of what a simplex spends its time on, so a ring may well flatter the solve.
Correctness is already validated against real published networks, and the
largest PGLib cases up to 13,659 buses are in the repository, so what is missing
is specifically the *scaling* measurement on real topology rather than any
question of whether real topology works.

**2. There is no real year of time series on a real network.** This is the
actual blocker behind gap 1, and it is a data problem rather than an engineering
one: real networks of this size are published, and a real network *with a year
of hourly data attached* in one file is not. Open Power System Data and the
ENTSO-E transparency platform publish the series; mapping zone-level series onto
buses is normal practice and needs to be done and stated rather than assumed
away.

**3. ~~The head-to-head is against the wrong tool.~~ Measured, and it cost us
a headline.** The founding quote recommends Julia, and this project had only
ever measured itself against `linopy`, which is Python. JuMP has now been
measured: gridwright is about a hundred times faster on identical counts, so
that comparison went the way the project hoped. The quote did not: Python built
a larger matrix per second than Julia did. And the measurement turned up that
our own linopy script was unfair, which narrows the published ratio from two
thousand to about a hundred. See
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

The premise was a quotable claim from the energy modelling literature: that
Python is "non-competitive" at *building* optimisation problems. Quoting it is
not measuring it, and it went unmeasured here for a long time.

Same model, same machine, same session. A synthetic 256-bus ring over a year at
hourly resolution, built in `linopy` 0.9.0 and in gridwright. Only construction
is timed; both hand the result to the same solver afterwards and that part is
not in dispute.

This table was published with the claim that the linopy side used it "the way
linopy is meant to be used". That claim was wrong, and what follows the table
is the correction rather than a caveat.

| | gridwright | linopy 0.9.0 |
| --- | --- | --- |
| Variables | 16,258,560 | 16,258,560 |
| Constraints | 6,167,040 | 6,167,040 |
| Nonzeros | 29,153,280 | 29,153,216 |
| Construction, to a matrix | **0.104 s** | **200.8 s** |
| Peak memory | **1.50 GB** | **22.4 GB** |

**That linopy column is unfair, and the unfairness is ours.** It has since been
measured against JuMP and against PyPSA, and the second of those showed that
`benchmarks/linopy_build.py` drives linopy down a path no linopy user would
take: it writes `(p * g_at).sum("gen")` against a dense incidence array, which
materialises a generator by bus by snapshot intermediate. PyPSA drives the
*same linopy 0.9.0* to a larger matrix in **16.7 s on 12.1 GB**.

So the honest comparison against a Python path that is actually in production
use is about **100× on time and under 4× on memory per nonzero**, not two
thousand and fifteen. The 200.8 s figure is reproducible and is a real
measurement of the script; it is not a fair measurement of linopy, and the
script is being fixed.

| | gridwright | JuMP `Model()` | PyPSA | linopy, via our script |
| --- | --- | --- | --- | --- |
| Construction | **0.102 s** | 10.34 s | 10.39 s | 98.39 s |
| Peak memory | **1.58 GB** | 5.08 GB | 12.13 GB | 23.02 GB |
| M nonzeros/s | **285** | 2.82 | 5.59 | 0.30 |

Two things follow, and the second is uncomfortable enough that it belongs
directly under the table rather than in a footnote.

**gridwright is about a hundred times faster than JuMP**, on three counts that
match exactly, with the nonzero count read out of HiGHS rather than predicted,
and with three different JuMP construction styles agreeing within 3% so that it
is not a strawman. That is the comparison this project should always have led
with, because JuMP is the tool the founding quote recommends.

**And the founding quote did not survive the test.** It says Python is
non-competitive at building optimisation problems "compared to tools based on
Julia or C++". On this model Python built a *larger* matrix at 5.59 M nonzeros
per second against Julia's 2.82 M: about twice as fast. The real distinction is
not the language. It is a purpose-built assembler against a general-purpose
modelling layer, and JuMP and linopy are both the latter. The quote is left in
the [Why](#why) section because it is what prompted the project, with this
result noted there.

Full method, counts and caveats in
[`benchmarks/head_to_head.md`](benchmarks/head_to_head.md). The machine was not
idle for that run, which is recorded there; the bias runs against gridwright,
since it is the only parallel builder of the four.

**The memory number matters more than the speed one.** Two hundred seconds is an
annoyance; 22 GB is where a laptop stops and the model does not get run at all.
That is the same conclusion the [Scale](#scale) section reaches from the other
direction: the fast build does not make the *solve* tractable, and what it
actually buys is that the model fits, that it can be rebuilt interactively, and
that a scenario sweep assembling it hundreds of times is not absurd.

Caveats, since the number is flattering. This is linopy 0.9.0 and a later
version may differ. linopy also does more than construct: it keeps a symbolic
model you can inspect and modify afterwards, which gridwright deliberately does
not. And the topology is synthetic, for the reason given below.

## Scale

Measured, on synthetic topologies, and labelled as such wherever it appears.
Correctness is validated against real networks; these say only how the problem
grows.

A full year at hourly resolution, solved whole, every rung run to completion.
Best of two runs, on an idle machine:

| Buses | Columns | Build | Solve | Growth |
| --- | --- | --- | --- | --- |
| 8 | 402,960 | 4.5 ms | 3.5 s | |
| 16 | 805,920 | 4.7 ms | 10.3 s | 2.9× |
| 32 | 1,611,840 | 8.5 ms | 31.0 s | 3.0× |
| 64 | 3,223,680 | 15.0 ms | **110.7 s** | 3.6× |
| 128 | 6,447,360 | 50.4 ms | **561.8 s** | 5.1× |

**This table replaces an earlier one that was wrong in every row, and the
correction is larger than the row that prompted it.** The 64-bus case was
published as "did not finish in seven minutes". It solves in under two minutes.
The 128-bus case, never attempted, solves in nine and a half.

Re-running moved everything else too: 16 buses was published as 20 s and is
10.3 s, 32 as 194 s and is 31.0 s, and the variable counts were about a quarter
high. Growth was published as a flat 9.5× per doubling; it is 3× at the small
end rising to 5× at the large one, which is a different shape as well as a
different number.

The rolling-horizon table below re-measured to within 6% of its published
values, so the fault was specific to this table rather than general.
The most likely cause is machine load, which has now corrupted a measurement in this project
three separate times. That is why every timing here is best-of-N on an idle
machine, and why the run-to-run spread is worth stating: the two 64-bus runs
differed by 16%.

Construction is 0.009% of runtime at 128 buses. **At full resolution the fast
build still buys almost nothing**, and that conclusion is untouched by the correction:
it never rested on the solve being intractable.

The same year through a rolling horizon of 96-hour windows keeping 72:

| Buses | Windows | Rolling | Solved whole | |
| --- | --- | --- | --- | --- |
| 16 | 122 | 4.0 s | 10.3 s | 2.6× |
| 32 | 122 | 8.7 s | 31.0 s | 3.6× |
| 64 | 122 | 23.6 s | 110.7 s | 4.7× |
| 128 | 122 | 76.3 s | 561.8 s | 7.4× |

The previous version claimed **23× at 32 buses, and that the rolling horizon
"finishes where the monolithic solve does not"**. Neither is true. It is 3.6× at
32 buses, and the monolithic solve finishes at every size tried, including 128.

What is true, and is the better argument anyway, is that the advantage
*compounds*: 2.6× at 16 buses, 7.4× at 128, because the whole-horizon solve
grows superlinearly while the rolling one grows nearly linearly in the number
of windows. Extrapolating that trend is how a continental model becomes
tractable, and it is a claim about decomposition rather than about this
builder.

Note that the rolling horizon performs **122 builds instead of one**, and
construction still does not register. What the fast build actually buys is set
out in [What that actually buys](#what-that-actually-buys-and-what-it-does-not),
and none of it is "the solve gets faster".

The pure-Rust solver, which is what a browser has:

| Rows | Time |
| --- | --- |
| 864 | 23 ms |
| 3,456 | 0.27 s |
| 13,824 | 5.1 s |
| 20,736 | 11.2 s |

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
