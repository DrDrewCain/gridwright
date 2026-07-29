# gridwright

A construction-first engine for cross-border energy system optimisation models,
in Rust. It assembles a linear program roughly **16x faster than linopy and
100x faster than JuMP**, on 2.3x and 3.4x less memory respectively, and solves
it with either HiGHS or a bounded-variable revised simplex written for this
project because HiGHS cannot reach `wasm32-unknown-unknown`.

A *wright* is a builder, and that is the claim: solvers are not the bottleneck
the literature complains about, construction is. Two pieces of solver were
written anyway — the simplex, so the browser build has one at all, and a spatial
branch and bound for the AC relaxation, because its tightening is specific to
that relaxation rather than something a general solver offers.

**Status: early.** The formulation is broad, the correctness evidence is good,
and the scaling evidence is mostly synthetic and says so. The gaps are set out
in [Limitations](#limitations) rather than left to be discovered.

---

## The premise, and how it survived testing

Models informing national decarbonisation policy are routinely made less
accurate on purpose: PyPSA-Eur's guidance recommends clustering Europe to a few
hundred nodes, and full resolution needs a commercial Gurobi licence. The stated
reason, from the energy modelling literature:

> Python is well known for being user-friendly, but when analyzing memory
> consumption and speed for **building optimization problems**, it was
> considered **non-competitive** compared to tools based on Julia or C++ — a
> bottleneck which also hinders large-scale optimization.

**That quote was tested here and did not hold.** Building the same model, Python
through PyPSA reached 5.6 M nonzeros/s against JuMP's 2.8 M — Python was about
twice as fast as Julia. What survives is the weaker observation underneath: a
*general-purpose modelling layer* is slow at this regardless of host language,
and both linopy and JuMP are that. The language is not the variable.

## Method

Every number below was produced under one protocol, stated once here rather than
re-qualified at each table.

| | |
| --- | --- |
| Hardware | MacBook Pro M3 Max, release build |
| Machine state | idle unless a table says otherwise |
| Repetitions | n=5, **every observation listed**, quoted figure is the median |
| Timing boundary | construction ends at a matrix a solver can read, transpose included |
| Topologies | synthetic ring + chords unless stated; real topologies from PGLib |
| Reproduce | `benchmarks/measure.sh`; per-comparison method in [`benchmarks/head_to_head.md`](benchmarks/head_to_head.md) |

**Best-of-N is used only where noted, and is a floor rather than an
expectation.** Where a figure is a single reading it is marked as one and not
promoted. Assembly is several full-width parallel regions, each ending when its
slowest thread does, so one busy core elsewhere moves the number more than most
of the changes measured here did — which is why machine state is reported.

## Results

### Construction

| Network | Columns | Rows | Nonzeros | Construction |
| --- | --- | --- | --- | --- |
| 256 bus × 168 h | 311,808 | 118,272 | 559,104 | **3.3 ms** |
| 256 bus × 8760 h | 16,258,560 | 6,167,040 | 29,153,280 | **96 ms** |
| 512 bus × 8760 h | 32,517,120 | 12,334,080 | 58,306,560 | **190 ms** |


The 256x8760 row is the median of five runs: 119.7, 94.6, 96.6, 96.2, 94.6 ms.
The first-run penalty is real and visible there. The other two rows are single
readings.

Peak resident memory for 256x8760 is **1.50 GB**, down from 1.95 GB when a
merged row-major matrix nothing read was removed.

Folding the transpose into assembly, measured both ways on one machine:

| Buses | Build, before | Transpose, before | **Total before** | **Now** |
| --- | --- | --- | --- | --- |
| 64 | 27.5 ms | 19.4 ms | **46.9 ms** | **28.8 ms** |
| 128 | 48.8 ms | 42.8 ms | **91.6 ms** | **53.6 ms** |
| 256 | 94.2 ms | 79.1 ms | **173.3 ms** | **100.0 ms** |
| 512 | 188.8 ms | 173.5 ms | **362.3 ms** | **189.6 ms** |


Between **1.6x and 1.9x** faster to a solver-ready matrix at every size, scaling
at about 1.88x per doubling. Best of three.

### Against other tools

Synthetic 256-bus ring, one year hourly. Construction only.

| | gridwright | linopy 0.9.0 | JuMP 1.31 | PyPSA 1.2.4 |
| --- | --- | --- | --- | --- |
| Variables | 16,258,560 | 16,258,560 | 16,258,560 | 14,016,000 |
| Nonzeros | 29,153,280 | 29,153,216 | 29,153,280 | 58,148,880 |
| Construction | **0.096 s** | **1.54 s** | 10.34 s | 10.39 s |
| Peak memory | **1.50 GB** | **3.45 GB** | 5.08 GB | 12.13 GB |


**Limitations of this table, which it does not carry on its face.** PyPSA's
counts do not match and never would — it formulates transmission through cycle
flows rather than voltage angles — so its column is a different problem and
belongs in a per-nonzero comparison. It was taken on a **machine that was not
idle** (2 of 14 cores busy), which biases *against* gridwright, the only one of
the four that builds in parallel. Repetition count **varies by condition**
(best of 5, 3, and 1 for every memory figure) and **no spread is reported**.
It has not been re-run at n=5. **Treat one significant figure as the resolution
these ratios support.**

**This table was wrong once, by a factor of 130.** The published ratio against
linopy was 2000x; it is about 16x. The cause was a benchmark script written here
that used linopy badly. It was caught only because PyPSA provided an independent
implementation to check against. **JuMP has no such cross-check, so treat the
100x as unconfirmed in exactly the way the 2000x turned out to be** — three JuMP
construction styles were tried and agreed within 3%, which is some protection
and not the same protection.

### Solve scaling

A full year hourly, solved whole, synthetic ring.

| Buses | Columns | Build | Solve, all five (s) | Median | Growth |
| --- | --- | --- | --- | --- | --- |
| 8 | 402,960 | 4.4 ms | 3.4, 3.4, 3.5, 3.6, 3.8 | **3.5 s** | |
| 16 | 805,920 | 5.2 ms | 9.9, 10.5, 10.7, 10.9, 11.1 | **10.7 s** | 3.1× |
| 32 | 1,611,840 | 8.6 ms | 29.4, 30.0, 30.1, 31.8, 32.7 | **30.1 s** | 2.8× |
| 64 | 3,223,680 | 15.5 ms | 104.9, 106.8, 107.1, 109.5, 117.6 | **107.1 s** | 3.6× |
| 128 | 6,447,360 | 27.9 ms | 331.6, 335.8, 339.1, 358.0, 365.6 | **339.1 s** | 3.2× |


**This table has been wrong three times**, twice from measuring on a busy machine
(one version reported 64 buses as "did not finish in seven minutes"). It is now
n=5 on an idle machine with every observation shown.

### How much the synthetic ring flatters the solve

A ring has degree two and a banded matrix; a real network is meshed and carries
2.2–2.3 nonzeros per column against the ring's 1.65. Matched column count, real
German hourly demand and renewable output:

| case | columns | real network | matched ring | |
| --- | --- | --- | --- | --- |
| IEEE 14 | 543,120 | 7.2 s | 5.7 s | 1.3× |
| IEEE 57 | 2,049,840 | 303.9 s | 41.8 s | **7.3×** |
| IEEE 118 | 4,826,760 | 788.8 s | 191.0 s | **4.1×** |


**Every solve figure in this document should be read as 1.3x to 7x optimistic
for a real network of the same size.** The ratio is not monotone, so that is a
range and not a trend. Construction is unaffected — it is linear in what is
written either way. Re-measured on an idle machine and reproduced within 3%.

## Correctness

Validated against networks nobody here designed: IEEE 14, 30, 57, 118 and 300
from the PGLib distribution (CC BY 4.0), and PEGASE 1354, a real European system
four times the largest of them. The from-scratch simplex agrees with HiGHS on
every one, including all 118 nodal prices of case118.

Checked on every network: generation balances demand exactly; no branch exceeds
rating; no generator violates limits; each synchronous area has exactly one
pinned angle; repeated solves agree; and **`f = B(theta0 - theta1)` holds on every
DC branch** against the solved angles. That last is the real check — on a network
with reactances spanning orders of magnitude, a sign error or transposed index
cannot survive it, where it would pass unnoticed on a symmetric triangle.

Unit tests are arithmetic rather than snapshots: cheap imports displace expensive
local generation to the exact MW; prices separate across a saturated
interconnector; power divides 2:1 on a triangle of equal susceptance; capacity is
built to the analytic break-even and not a MW past it, straddled from both sides.

**Two of those tests were wrong before the code was.** A transport loop had no
unique flow solution, and a carbon cap never bound because the clean option was
already economic. Both now assert what is determinate and say why in the test.

**Not claimed:** agreement with published AC-OPF objectives. This is a DC model
and generator costs are the linear term of a published quadratic, so the numbers
are not comparable.

## Formulation

| Area | Covered |
| --- | --- |
| **Dispatch** | Nodal balance per bus per snapshot; DC flow for AC lines; transport limits for HVDC; storage with round-trip efficiency and cyclic state of charge; renewable availability profiles; load shedding priced at value of lost load; **nodal marginal prices** |
| **Capacity expansion** | Extendable generators, storage and transport links with capital cost, floor at existing fleet, ceiling for land and grid connection. Expanding an AC line is *refused* rather than linearised — widening a conductor changes its impedance and the flow equation would become bilinear |
| **Unit commitment** | Stable minimum, start-up cost, minimum up and down time. Opt-in per generator because it makes the problem a MILP; a continuous relaxation idles a coal unit at 8% of rating and understates both cost and emissions |
| **Emissions** | System-wide CO2 budget as one constraint — structurally unlike everything else here, one row millions of entries wide |
| **Sector coupling** | Buses carry a carrier; a link moves energy between two at an efficiency. An electrolyser is a link to hydrogen at 70%, a heat pump a link to heat at 300% — one component, same equation |
| **Hydro** | Reservoirs with natural inflow and spill; cascades where an upper station's release becomes the lower one's inflow after travel time. Head modelled two ways: exactly over reservoir-level bands with a binary, and approximately by fixed-point iteration |
| **Multi-period** | Capacity built in one period available in all later ones, costs discounted per period. Without discounting a model defers every decision to the final period, which is arithmetic rather than a finding |
| **Non-European grids** | Buses belong to a *synchronous area*. An AC line may not cross one, and **each area gets its own angle reference** — pinning one leaves every other with a free constant. Areas join through HVDC, which carries losses |
| **Ramp limits** | Down-ramp binds *before* the problem appears: rather than run high and be stranded above demand later, the optimiser produces less earlier |
| **Losses** | Marginal rate on flow magnitude. Absolute value is not linear but is the maximum of two linear functions, and since loss only removes energy the optimiser drives it to that bound |
| **N-1 security** | Line outage distribution factors, so security costs rows rather than columns — the obvious formulation duplicates every flow variable per contingency. Islanding contingencies are reported, not silently ignored |
| **Rolling horizon** | Overlapping windows carrying reservoir levels and commitment states forward, because a window assuming cold starts invents start-up costs already paid |
| **Stochastic** | Two-stage: futures share one investment decision, operating costs probability-weighted, capital not |
| **AC power flow** | Jabr second-order-cone relaxation, cycle constraints of any length, spatial branch and bound, apparent-power limits |
| **Demand response** | Four distinct failure modes, answering different questions: shed at VoLL, shifted with energy conserved, declined on a willingness-to-pay curve, or curtailed under an interruptible contract a bounded number of times |

**Data.** Any format, by content rather than extension: MATPOWER, PSS/E RAW and
RAWX, CGMES, UCTE, PyPSA CSV and netCDF, IEEE CDF, Parquet, Excel. `load_bytes`
takes a name and a buffer, so **none of it needs a filesystem** — which is what
makes the browser build possible.

## The studio

An interface, running in a browser with **no server**: the model is built and
solved in the tab. That is the whole reason the simplex exists.

```sh
cargo run -p gridwright-studio -- examples/demo-grid   # a window
./crates/gridwright-studio/build-web.sh                # a tab
```

Busbars with circuits tapping onto them, IEC symbols for machines coloured by
carrier, and once solved: nodal price on the busbars, utilisation and direction
on the corridors, a scrubber over the horizon, and charts for price, duration,
flow against rating, state of charge and the system dispatch stack. Dragging any
chart scrubs the timeline. `⌘K` searches every bus, corridor and command.

Bundle: 7 MB on disk, **2 MB compressed**, which is the number that crosses the
wire. Full detail and an honest list of what it cannot do:
[`crates/gridwright-studio/README.md`](crates/gridwright-studio/README.md).

## Limitations

**Evidence.** Most scaling numbers are synthetic rings, labelled as such, and
overstate real-network solve performance by 1.3x to 7x. The head-to-head has not
been re-run at n=5 on an idle machine. The 100x against JuMP has no independent
cross-check.

**Scope.** Construction speed buys nothing if your model already solves — at
modest size the build is ~0.1% of runtime, and you should use PyPSA, which has a
decade of features this does not. The case here is narrower: models that are
*currently not being run*, clustered down from thousands of nodes to hundreds
because the full problem will not fit in memory. There, 100 ms of construction
is really a claim about the 1.50 GB it did not need.

**Features.** Nothing writes CIM, and a non-conformant CGMES file is worse than
none. Time series are read whole rather than streamed.

**Measured and deliberately not built**, each with numbers in `TODO.md` so
nobody repeats them: Forrest–Tomlin updates (address 2.3% of a solve), a
fill-reducing column ordering (halves fill, costs more than it saves), partial
pricing (implemented, switched off — a cheaper scan buys a worse entering
variable and the two cancel).

## Layout

| Crate | Purpose |
| --- | --- |
| `gridwright-model` | Sparse LP core: variable blocks, row batches, CSC transpose |
| `gridwright-net` | Network domain: buses, lines, generators, storage, loads |
| `gridwright-build` | Parallel LP assembly |
| `gridwright-simplex` | Bounded-variable revised simplex: sparse LU, branch and bound, duals, compiles to wasm |
| `gridwright-acopf` | AC OPF: Jabr relaxation, cycle constraints, spatial branch and bound |
| `gridwright-solve` | Solver trait, HiGHS and pure-Rust backends |
| `gridwright-io` | Every data format, in and out |
| `gridwright-emissions` | Production and consumption carbon accounting |
| `gridwright-worker` | Reading and solving behind one bytes-in interface |
| `gridwright-studio` | The interactive shell |
| `gridwright-cli` | The `gw` binary |

## Build

Needs a Rust toolchain and `cmake`, since HiGHS builds from source.

```bash
cargo build --release
cargo test --workspace --all-features   # 739 tests
./target/release/gw demo
./target/release/gw run examples/eu-mini --out results/
./target/release/gw case examples/pglib/case118_ieee.m
./target/release/gw bench --buses 256 --hours 8760 --solve
```

## Licence

**AGPL-3.0** ([LICENSE](LICENSE)). Use it, modify it, run it in production, no
permission needed. If you modify it and offer it over a network, publish your
modified source too.

Research, laboratories, regulators, NGOs and anyone publishing their modelling
are unaffected — they were going to show their working anyway. Companies needing
a closed surrounding stack need a commercial licence: see
[COMMERCIAL.md](COMMERCIAL.md). The code is identical either way and nothing is
withheld from the open version.
