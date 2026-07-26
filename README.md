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
unlike everything else here — one row millions of entries wide instead of many
narrow ones — and the lever most decarbonisation questions are asked through.

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
area gets its own angle reference** — pinning only one leaves every other area
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
reported rather than quietly ignored — that is a real vulnerability, just not
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

It declines integer problems rather than returning the relaxation, so unit
commitment still needs HiGHS. A commitment answer with fractional on/off states
is not an answer.

**AC power flow.** DC flow is a linearisation, and the things it drops —
losses, voltage magnitudes, reactive power — are often the binding constraints.
So there is a genuine AC formulation too, through the Jabr second-order-cone
relaxation solved with `clarabel`.

Being precise about what that means, because it is easy to overclaim: AC-OPF is
nonconvex and this does not solve it exactly. It solves a convex relaxation
whose optimum is a rigorous lower bound. The relaxation is provably exact on
radial networks, and can loosen on meshed ones. **Whether it came out tight is
reported rather than assumed** — an inexact relaxation returns voltages that
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
`W_ij = R + iI = V_i·conj(V_j)` turns it into `Im(W₁W₂W₃) = 0`, a trilinear
equality, which McCormick envelopes relax convexly.

The tests check the property that matters: adding the cuts must never *lower*
the bound. A bound that falls means the cuts are invalid, which would turn a
rigorous number into a wrong one.

**Hydraulic head.** Power is proportional to the height water falls through, so
a reservoir near empty cannot reach its rating whatever the gates do. Taken at
the start of each period rather than the end — using the end level makes the
constraint self-limiting, since discharging lowers the level that permits the
discharge, and a brim-full reservoir could never reach its rating.

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
CGMES model split across its profiles. Every reader is pure Rust — including
netCDF, via a pure-Rust HDF5 implementation, and Parquet, using only codecs
that cross-compile — so the whole format layer builds for
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
| 512 bus × 8760 h | 32,517,120 | 12,334,080 | 58,306,560 | **174 ms** |

Peak resident memory for the 256 × 8760 case is **1.93 GB**. That row was 89 ms
before commitment, losses, cascades and multi-period capacity were added; the
extra machinery costs about 12%, which seems a fair price and is reported rather
than quietly rebaselined. Scaling is linear:
20.9 → 44.8 → 84.3 → 173.7 ms across 64 → 128 → 256 → 512 buses, about 2.05×
per doubling.

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
that end rather than the point, and a build that finishes in 89 ms is really a
claim about the 1.93 GB it did not need.

## How

Two phases with a hard line between them.

**Allocate.** Every variable block is handed out up front, sequentially. One
contiguous block per component spanning all snapshots, so a component's whole
trajectory is a slice rather than a gather, both going in and coming out.

**Assemble.** Every constraint family is generated in parallel into per-thread
row batches, then merged once. This works because after allocation every
variable index is a pure function of its block and offset: a thread building
balance rows for bus 400 needs no coordination to know where generator 12's
dispatch at snapshot 900 lives.

Three decisions do most of the work.

**Time series are component major.** Availability for generator `g` is a
contiguous run, exactly the order the bounds vector needs it in. Nodal balance
appears to want the opposite layout, so balance is parallelised over *buses*
rather than snapshots — equally valid, since both axes are independent, and it
keeps every read sequential.

**The CSR→CSC transpose is a parallel counting sort.** Column indices are
already integers, so there is nothing to compare. Per-thread histograms, then a
two-dimensional scan telling each thread where its own slice of each column
begins, then a scatter that needs no atomics because the destinations are
provably disjoint.

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
| `gridwright-simplex` | Our own bounded-variable simplex. Pure Rust, returns duals, compiles to WASM. |
| `gridwright-acopf` | AC optimal power flow, via the Jabr second-order-cone relaxation. |
| `gridwright-solve` | Solver trait, with HiGHS and pure-Rust backends. |
| `gridwright-io` | Every data format, and result export. |
| `gridwright-emissions` | Production and consumption carbon accounting, average and marginal. |
| `gridwright-cli` | The `gw` binary. |

## Build

Needs a Rust toolchain and `cmake`, since HiGHS is built from source.

```bash
cargo build --release
cargo test --workspace --all-features   # 429 tests
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
  the two-hop path — the DC power flow physics, not merely its plumbing
- storage covers a generator outage by having charged beforehand
- the parallel transpose agrees with the serial one byte for byte at scale
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

Early, but the formulation now covers dispatch, capacity expansion, unit
commitment, sector coupling, hydro with inflow and spill, multi-period
investment, emissions budgets and planning reserve.

Since that list was written: bus shunt admittances and transformer phase shifts
are in, and so is the spatial branch and bound. The last of those closes the AC
relaxation gap properly, which is what `TODO.md` said was needed. On IEEE 57 the
relaxation is only a bound at the root and 33 nodes prove the optimum; on
IEEE 118 the cone gap falls twenty-five fold.

Absent: head's effect on energy *conversion*, as opposed to on available
capacity which is implemented. A full reservoir yields more megawatt-hours from
the same volume, and that part is bilinear in flow and volume rather than
linear.

Also absent: cycle constraints for fundamental cycles longer than three,
apparent-power line limits, and demand that can be shifted in time rather than
only shed.

Scaling benchmarks still use synthetic topologies, because no public dataset is
conveniently available at 8760 snapshots and hundreds of buses in one file; they
are labelled as synthetic wherever they appear. Correctness is validated against
real networks, which is the half that matters.

## Scale

Measured, on synthetic topologies, and labelled as such wherever it appears.
Correctness is validated against real networks; these say only how the problem
grows.

A full year at hourly resolution, solved whole:

| Buses | Variables | Build | Solve |
| --- | --- | --- | --- |
| 16 | 1.0M | 11 ms | 20 s |
| 32 | 2.0M | 14 ms | 194 s |
| 64 | 4.1M | — | did not finish in seven minutes |

The solve grows about 9.5× for a doubling and construction is 0.0–0.1% of
runtime. **At full resolution the fast build buys almost nothing.** The same
year through a rolling horizon of 96-hour windows keeping 72:

| Buses | Windows | Total |
| --- | --- | --- |
| 32 | 122 | 8.5 s |
| 64 | 122 | 23 s |
| 128 | 122 | 72 s |

Twenty-three times faster at 32 buses, and it finishes where the monolithic
solve does not. Note that this performs **122 builds instead of one** and
construction still does not register: decomposition is what makes a year
tractable, not the builder. What the fast build actually buys is interactive
rebuild, scenario sweeps that assemble the same network hundreds of times, and
the memory ceiling. That is a narrower claim than "construction is the
bottleneck", and it is the true one.

The pure-Rust solver, which is what a browser has:

| Rows | Time |
| --- | --- |
| 864 | 57 ms |
| 3,456 | 1.1 s |
| 13,824 | 36 s |

About `m^1.9`. Before the sparse factorisation replaced the dense inverse this
was `m^2.7`, and 2,592 rows would not finish inside ten minutes.

## Licence

**AGPL-3.0** ([LICENSE](LICENSE)). Open source: use it, modify it, run it in
production, no permission needed. The AGPL asks that if you modify it and offer
it over a network, you publish your modified source too.

Research, laboratories, regulators, NGOs and anyone publishing their modelling
are unaffected, because they were going to show their working anyway. Companies
that need to keep a surrounding stack closed need a commercial licence instead;
see [COMMERCIAL.md](COMMERCIAL.md). The code is identical either way and nothing
is withheld from the open version.
