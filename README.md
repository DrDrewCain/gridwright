# gridwright

A fast engine for building and solving cross-border energy system optimisation
models, written in Rust.

A *wright* is a builder. That is the distinction this project is making: it is
not another solver. HiGHS and Gurobi already solve these problems well. It is
the thing that **builds** them, which turns out to be where the time actually
goes.

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

**Data.** Networks load from a directory of CSVs in the layout PyPSA writes, so
existing data mostly already looks right. Results write back the same shape.

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

## Measured

MacBook, release build. Synthetic networks: ring plus chords, three generators
per bus, storage on every fourth bus, DC power flow throughout.

| Network | Columns | Rows | Nonzeros | Construction |
| --- | --- | --- | --- | --- |
| 256 bus × 168 h | 311,808 | 118,272 | 559,104 | **3.3 ms** |
| 256 bus × 8760 h | 16,258,560 | 6,167,040 | 29,153,280 | **89 ms** |
| 512 bus × 8760 h | 32,517,120 | 12,334,080 | 58,306,560 | **174 ms** |

Peak resident memory for the 256 × 8760 case is **1.93 GB**. Scaling is linear:
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
| `gridwright-solve` | Solver trait plus the HiGHS backend. |
| `gridwright-io` | CSV loading and result export, including its own parser. |
| `gridwright-cli` | The `gw` binary. |

## Build

Needs a Rust toolchain and `cmake`, since HiGHS is built from source.

```bash
cargo build --release
cargo test --workspace          # 99 tests
./target/release/gw demo
./target/release/gw run examples/eu-mini --out results/
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

Early, but no longer dispatch-only. Not implemented: unit commitment (needs
binaries, so MILP rather than LP), multi-period investment across decades,
sector coupling, and hydro cascades. Benchmarks still use synthetic topologies
of realistic shape rather than real data, and say so where they appear.

## Licence

MIT or Apache-2.0, at your option.
