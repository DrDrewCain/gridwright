# Head to head: gridwright against JuMP, PyPSA and linopy

The project's founding quote says Python is "non-competitive" at *building*
optimisation problems "compared to tools based on Julia or C++". Until now the
only head to head here was against `linopy`, which is the tool the quote
criticises. It had never been run against the tool the quote holds up as the
alternative. This is that measurement.

Two things came out of it. The first is that gridwright is about a hundred
times faster than JuMP at construction, on a matrix whose three counts match
exactly. The second is less comfortable and is reported here with equal
prominence: **the quote's actual proposition did not survive the test.** On this
model Python was roughly twice as fast as Julia per nonzero, and the published
"about two thousand times faster than linopy" ratio is substantially an artefact
of how `benchmarks/linopy_build.py` uses linopy rather than a property of
linopy.

## Method

The model is the synthetic ring `gw bench` builds: 256 buses, a ring plus
chords giving 384 lines, three generators per bus, storage on every fourth bus,
load shedding at every bus, over 8,760 hourly snapshots. Nodal balance, DC flow
through voltage angles, and cyclic storage dynamics. Only construction is timed
and nothing is solved.

| | |
| --- | --- |
| Machine | MacBook Pro, Apple M3 Max, 14 cores, 36 GB |
| gridwright | current `main`, `cargo build --release` |
| JuMP | 1.31.1 on Julia 1.12.6, with HiGHS.jl |
| PyPSA | 1.2.4, on Python 3.12 |
| linopy | 0.9.0, which is both the published figure's version and the one PyPSA 1.2.4 pulls in |
| Repetitions | best of 5 for gridwright and JuMP `Model()`, best of 3 for JuMP direct and the PyPSA ladder, best of 1 for the peak memory runs |
| Peak memory | `/usr/bin/time -l`, one build per process, identically for all four tools |

Julia compiles on first call, so a cold run measures the compiler rather than
the program. Every Julia figure below is from a process that first ran an
untimed 8-bus by 24-hour build through exactly the same functions with exactly
the same types. Only runs after that warm-up are timed.

**The machine was not idle, and that is stated here rather than buried.**
Another process was running this repository's `--ignored` test binaries
throughout, occupying two of fourteen cores. The direction of that bias is
worth being precise about: gridwright's assembly is parallel across all cores
and is the only one of the four that loses anything, while JuMP, PyPSA and
linopy construct single-threaded and are essentially unaffected. So the
contamination works *against* gridwright and cannot manufacture a gridwright
win. Its size is bounded by two reproductions: gridwright's published 0.104 s
and 1.50 GB came out at 0.102 s and 1.58 GB, and linopy's published 200.8 s and
22.4 GB came out at 200.2 s and 23.0 GB. Both are within the run to run spread
this project has documented elsewhere.

## The counts match, and that is what makes the comparison mean anything

The linopy benchmark had a real bug caught by exactly this check, where a
degenerate constraint gave linopy less to build. So `jump_build.jl` derives the
expected counts from the topology, asserts them against what JuMP reports, and
prints a loud mismatch line if they disagree. They agreed at every size:

| Size | Columns | Rows | Nonzeros | JuMP |
| --- | --- | --- | --- | --- |
| 32 × 168 | 38,976 | 14,784 | 69,888 | all three identical |
| 64 × 720 | 334,080 | 126,720 | 599,040 | all three identical |
| 128 × 2,190 | 2,032,320 | 770,880 | 3,644,160 | all three identical |
| 256 × 8,760 | 16,258,560 | 6,167,040 | 29,153,280 | all three identical |

The nonzero figures for JuMP are read out of HiGHS itself after building
through `direct_model`, so they are the count of a real matrix rather than a
prediction. Note that this makes the JuMP comparison slightly *stricter* than
the published linopy one: gridwright's storage is cyclic, so its first snapshot
links to the last, and `jump_build.jl` reproduces that. `linopy_build.py` uses
`.shift(snapshot=1)`, which drops that term, which is why its nonzero count is
29,153,216 rather than 29,153,280. JuMP was asked to build 64 nonzeros more
than linopy was, not fewer.

## PyPSA's counts do not match, and were never going to

PyPSA is the exception, and it is reported loudly rather than quietly because a
benchmark that compares two different problems reads as a result. Every run of
`pypsa_build.py` prints this table and a banner. At 256 buses over 8,760 hours:

| | PyPSA | gridwright | Difference |
| --- | --- | --- | --- |
| Columns | 14,016,000 | 16,258,560 | -2,242,560 |
| Rows | 31,965,240 | 6,167,040 | +25,798,200 |
| Nonzeros | 58,148,880 | 29,153,280 | +28,995,600 |

Both differences are properties of PyPSA rather than mistakes in the script,
and both are fully accounted for.

**The missing columns are the voltage angles.** PyPSA formulates transmission
through the Kirchhoff voltage law, one row per independent cycle, where
gridwright uses angles, one variable and one row per line. The shortfall is
exactly 256 × 8,760, which is one angle per bus per snapshot, to the variable.

**The extra rows are fixed capacity limits.** PyPSA emits
`Generator-fix-p-upper` and its eleven siblings as constraints. gridwright puts
the same limits in the bound vectors, where they cost no rows and no nonzeros
at all. This is the larger effect by far and it is why PyPSA's matrix comes out
roughly twice the size for identical physics.

Load shedding is added the way PyPSA users add it, as a generator per bus
priced at the value of lost load, because PyPSA has no shed variable. That part
does match gridwright one for one.

The consequence for reading the table below is that PyPSA's time is "what it
costs PyPSA to reach a solver-ready matrix for this network", not "what it
costs to build the same matrix". Comparing it to gridwright fairly means
comparing throughput per nonzero rather than wall clock, and that is done
below. The JuMP row is the one whose counts match exactly, and it is the
stronger evidence for that reason.

## Result, 256 buses over 8,760 hours

| | gridwright | JuMP 1.31.1 `Model()` | JuMP direct to HiGHS | PyPSA 1.2.4 | linopy 0.9.0 as scripted |
| --- | --- | --- | --- | --- | --- |
| Variables | 16,258,560 | 16,258,560 | 16,258,560 | 14,016,000 | 16,258,560 |
| Constraints | 6,167,040 | 6,167,040 | 6,167,040 | 31,965,240 | 6,167,040 |
| Nonzeros | 29,153,280 | 29,153,280 | 29,153,280 | 58,148,880 | 29,153,216 |
| Construction | **0.102 s** | **10.34 s** | **23.51 s** | **10.39 s** | **98.39 s** |
| Then to a sparse matrix | included | see below | included | +6.30 s | +101.83 s |
| Peak resident memory | **1.58 GB** | **5.08 GB** | **5.88 GB** | **12.13 GB** | **23.02 GB** |
| Throughput, M nonzeros/s | 285 | 2.82 | 1.24 | 5.59 | 0.30 |
| Bytes per nonzero | 54 | 174 | 202 | 209 | 790 |

Two of those columns need their meaning stated, because "construction time" is
not one quantity.

**gridwright's 0.102 s is to a matrix a solver takes**, compressed sparse
columns included, which is the standard the README already holds itself to.
**JuMP's 10.34 s is not**: it is time to MathOptInterface's in-memory cache,
which is what a JuMP user writes but is not yet a matrix. The like for like
figure is the direct one, 23.51 s, which builds straight into HiGHS's own
matrix and is what JuMP's documentation recommends when you want to skip the
cache. The other route from a cached `Model()` to a sparse matrix is
`lp_matrix_data`, which was not run at full size; at 128 by 2,190 it took
4.71 s against 1.26 s of construction, so it costs several times what building
did. JuMP's own docs call it pedagogical and say it is not a solver interface,
so it is reported here and not used for any headline.

So the ratio is **101x on JuMP's natural in-memory model, and 230x to a matrix
a solver could read**. Either way it is not close.

## Scaling

Construction seconds, best of N, same session:

| Size | gridwright | JuMP `Model()` | JuMP direct | PyPSA `create_model` | linopy assemble |
| --- | --- | --- | --- | --- | --- |
| 32 × 168 | 0.0014 | 0.010 | | 0.233 | 0.087 |
| 64 × 720 | 0.0045 | 0.187 | 0.397 | 0.278 | 0.302 |
| 128 × 2,190 | 0.0147 | 1.281 | 2.478 | 0.686 | 3.031 |
| 256 × 8,760 | 0.1023 | 10.344 | 23.507 | 10.394 | 98.392 |

gridwright and JuMP both scale close to linearly in problem size across the
whole ladder. linopy as scripted does not, and that turns out to matter a great
deal. See the last section.

## The honest read

### JuMP is not competitive with gridwright here, and that was not guaranteed

This was the result that could have gone the other way, and it is worth saying
that plainly rather than treating it as expected. JuMP is a mature compiled
modelling layer, it is the tool the founding quote names as the alternative to
Python, and a hundredfold gap is not something a benchmark owes anyone in
advance. On an identical matrix it built at 2.82 M nonzeros per second against
gridwright's 285 M, on 3.2 times the memory.

The reason is structural rather than a matter of Julia being slow. JuMP builds
a general algebraic model: every constraint becomes a `ScalarAffineFunction`
with its own heap-allocated term vector, indices are handed out through
MathOptInterface, and the matrix is materialised afterwards. gridwright hands
out every variable block up front, so every index is a pure function of block
and offset, and threads scatter straight into column major form with no
coordination. That is a narrower thing to be, and being narrower is the whole
of the advantage.

### The quote's actual claim did not survive, and this is the more interesting finding

The quote is that Python is non-competitive at building *compared to tools
based on Julia or C++*. On this model that is not what happened.

PyPSA, which is Python end to end and builds through linopy, produced a matrix
of 58.1 M nonzeros in 10.39 s. JuMP produced 29.2 M nonzeros in 10.34 s. Per
nonzero that is **5.59 M/s for the Python stack against 2.82 M/s for the Julia
one**, so Python was about twice as fast. On memory they are almost
indistinguishable, 209 bytes per nonzero against 174.

The distinction the measurement actually supports is not Python against Julia.
It is *general purpose algebraic modelling layers* against a *purpose built
assembler*, and both of the general purpose layers land within a factor of two
of each other and fifty to a hundred times behind the assembler. That is a
different claim from the one this project was founded on, it is narrower, and
the README's framing should follow the measurement rather than the quote.

### The published linopy ratio is inflated by the benchmark script, not by linopy

This is the finding that most needs acting on. `linopy_build.py` takes 98.4 s to
assemble and 101.8 s to export on 23.0 GB, reproducing the published 200.8 s and
22.4 GB almost exactly, so the published number is not wrong about what the
script does. But **PyPSA drives the same linopy 0.9.0 to a matrix twice the size
in 10.39 s plus 6.30 s of export, on 12.13 GB.** Same library, same version,
same machine, same session, and a twelvefold difference in time on a larger
problem.

The cause is visible in the script and the arithmetic accounts for it
quantitatively. `linopy_build.py` builds nodal balance as
`(p * g_at).sum("gen")`, where `g_at` is a *dense* 768 by 256 incidence array.
Broadcasting a `(gen, snapshot)` expression against a `(gen, bus)` array
materialises a dense `(gen, bus, snapshot)` intermediate before the sum
collapses it: 768 × 256 × 8,760, which is 1.72 billion terms, about 13.8 GB of
coefficients. The line incidence does the same at 384 × 256 × 8,760, about
6.9 GB. That predicts roughly 20.7 GB, against 23.0 GB measured. The model
itself is 29.2 M nonzeros and would be about 2.9 GB at a hundred bytes each.
Nearly all of the memory, and by implication nearly all of the time, is spent on
intermediates that are quadratic in the number of buses for a model that is
linear in it. The superlinear ladder is the same story from the other side:
from 128 × 2,190 to 256 × 8,760 the model grows eightfold and the assemble time
grows thirty-twofold.

The header of `linopy_build.py` says linopy "is used the way it is meant to be,
vectorised over xarray dimensions with incidence arrays", and that was a
reasonable belief. Dense incidence arrays are how you would write this in
numpy, but they are not how PyPSA drives linopy and they are not what linopy is
efficient at. The consequence is that **the README's "about two thousand times
faster, on fifteen times less memory" overstates the case.** Against a Python
path that is actually in production use, the honest figures on this model are
about a hundredfold on time and under fourfold on memory per nonzero.

None of that changes the gridwright column, which reproduced. It changes what
it should be compared against. Fixing `linopy_build.py` was outside this task's
remit, and rewriting it to use linopy's own grouping primitives and remeasuring
is the obvious next piece of work.

### What these numbers do and do not support

They support: gridwright builds this model 101 times faster than JuMP on an
identical matrix, and 230 times faster if both are measured to a matrix a
solver could read. Against PyPSA the wall clock ratio is 102x, but PyPSA's
matrix is twice the size, so the defensible figure there is 51x per nonzero.
Memory is three to four times lower per nonzero than either. All three of
gridwright's counts match JuMP's exactly at every size tested.

They do not support: that Python is non-competitive with Julia at building
optimisation problems, which is the founding quote and which this measurement
contradicts. They do not support the two thousandfold ratio or the fifteenfold
memory ratio currently in the README, both of which rest on a linopy script that
does quadratic work. They say nothing about any real network's topology, since
this is one synthetic ring. They say nothing about solve time, which dominates
completely at this resolution, and nothing about the features PyPSA has and
gridwright does not.

The three things the README already says the fast build actually buys, namely
that the model fits at all, that rebuilding is not a per-iteration tax, and that
it runs where a Python stack cannot, are untouched by any of this. If anything
they become the whole of the case rather than the qualification on it, because
the raw ratio is smaller than advertised and the memory ratio much smaller.

## Reproducing

```bash
# JuMP
julia -e 'using Pkg; Pkg.add(["JuMP", "HiGHS"])'
/usr/bin/time -l julia benchmarks/jump_build.jl --buses 256 --hours 8760 --reps 1
/usr/bin/time -l julia benchmarks/jump_build.jl --buses 256 --hours 8760 --reps 1 --backend direct

# PyPSA
python -m venv .venv && .venv/bin/pip install pypsa
/usr/bin/time -l .venv/bin/python benchmarks/pypsa_build.py --buses 256 --hours 8760 --reps 1 --matrix

# linopy, and gridwright
/usr/bin/time -l python benchmarks/linopy_build.py
/usr/bin/time -l ./target/release/gw bench --buses 256 --hours 8760
```

Both new scripts refuse to be quiet about a mismatch. `jump_build.jl` prints a
line per count and a banner if any of them disagree with what the topology
implies, and `pypsa_build.py` prints its counts against gridwright's every time,
because in its case they never match and the reader needs to know why.
