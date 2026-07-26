"""Build the same network in PyPSA, the way a PyPSA user actually gets to a matrix.

`linopy_build.py` writes the model out in linopy by hand. That is a fair test of
linopy, but nobody models a power system that way: they describe a network in
PyPSA and let PyPSA emit the optimisation problem. This script measures that
path, because it is the one the comparison is really about. PyPSA is the largest
open energy modelling ecosystem there is, and the clustering advice that
motivated this whole project is PyPSA-Eur's.

Fairness, since a benchmark is only as good as its fairness:

- PyPSA is used exactly as documented. The network is described through
  `n.add` with array arguments, which is PyPSA's vectorised bulk path, and the
  problem comes from `n.optimize.create_model()`, which is what
  `n.optimize()` calls before handing anything to a solver. No internals are
  reached into and nothing is hand-rolled.

- The network is the same synthetic ring `gw bench` builds: a ring plus chords,
  three generators per bus, storage on every fourth bus, a wind profile per bus
  and a demand profile per bus. Load shedding is added the way PyPSA users add
  it, as a generator per bus priced at the value of lost load, since PyPSA has
  no shed variable of its own.

- Building the `pypsa.Network` object is timed separately from building the
  optimisation problem. gridwright reports its own network generation
  separately for the same reason: neither engine should be charged for
  assembling the inputs. Only the `create_model` figure is the one under test.

- Consistency checking is switched off with `consistency_check=False`. It is a
  validation pass rather than construction, and charging PyPSA for it would
  flatter gridwright, which does not do the equivalent.

- Matrix export is timed separately, matching how `linopy_build.py` reports it,
  because gridwright's figure includes producing compressed sparse columns.

THE COUNTS DO NOT MATCH, AND THAT IS THE POINT OF REPORTING THEM.

This script does not and cannot produce the same matrix gridwright produces,
for two reasons that are properties of PyPSA rather than mistakes here. The
script prints both sets of counts side by side and explains each difference,
because a benchmark that quietly compares two different problems reads as a
result and is worse than no benchmark at all.

1. PyPSA uses the Kirchhoff voltage law formulation, one row per independent
   cycle, where gridwright uses voltage angles, one row and one angle variable
   per line. PyPSA therefore has fewer columns (no angles) and fewer flow rows
   (cycles rather than lines), but its cycle rows are long where gridwright's
   flow rows have exactly three entries each.

2. PyPSA emits explicit rows for fixed capacity limits: `Generator-fix-p-upper`
   and its siblings are constraints, not variable bounds. gridwright puts the
   same limits in the bound vectors, where they cost no rows and no nonzeros.
   This is the larger of the two effects and it makes PyPSA's matrix
   substantially bigger than gridwright's for identical physics.

So the honest reading of any timing below is "what it costs PyPSA to reach a
solver-ready matrix for this network", not "what it costs to build the same
matrix". The JuMP benchmark in `jump_build.jl` is the one that matches
gridwright's counts exactly, and it is the stronger evidence for that reason.

Setup:

    python -m venv .venv && .venv/bin/pip install pypsa

Run, with peak resident memory measured the same way as every other figure in
this project:

    /usr/bin/time -l python benchmarks/pypsa_build.py --buses 256 --hours 8760

Peak memory is only meaningful with `--reps 1`, since more repetitions leave the
previous model live long enough to overlap with the next one.
"""

import argparse
import gc
import math
import time
import warnings

import numpy as np
import pandas as pd

warnings.filterwarnings("ignore")

import pypsa  # noqa: E402


VOLL = 3000.0


def topology(buses: int):
    """The ring plus chords that `gw bench` generates.

    One ring line per bus, then a chord from every second bus to the one a
    third of the way round, which is what the CLI's `synthetic` does.
    """
    bus0 = list(range(buses))
    bus1 = [(b + 1) % buses for b in range(buses)]
    if buses > 8:
        for b in range(0, buses, 2):
            far = (b + buses // 3) % buses
            if far != b:
                bus0.append(b)
                bus1.append(far)
    return bus0, bus1


def make_network(buses: int, hours: int) -> pypsa.Network:
    """Describe the network. This is data setup and is timed on its own."""
    bus0, bus1 = topology(buses)
    lines = len(bus0)
    stores = max(buses // 4, 1)

    n = pypsa.Network()
    n.set_snapshots(pd.RangeIndex(hours))

    bus_names = [f"bus{b}" for b in range(buses)]
    n.add("Bus", bus_names)

    # Ring lines are the strong ones, chords the weak ones, matching the CLI's
    # susceptances of 10 and 6. PyPSA wants reactance, so it is the reciprocal.
    reactance = [1.0 / 10.0] * buses + [1.0 / 6.0] * (lines - buses)
    s_nom = [3000.0] * buses + [1500.0] * (lines - buses)
    n.add(
        "Line",
        [f"line{i}" for i in range(lines)],
        bus0=[bus_names[b] for b in bus0],
        bus1=[bus_names[b] for b in bus1],
        x=reactance,
        s_nom=s_nom,
    )

    # Three generators per bus: baseload, peaking, and a variable renewable
    # whose availability profile is what makes the time series matter.
    t = np.arange(hours)
    n.add(
        "Generator",
        [f"base{b}" for b in range(buses)],
        bus=bus_names,
        p_nom=800.0,
        marginal_cost=[12.0 + (b % 5) for b in range(buses)],
    )
    n.add(
        "Generator",
        [f"peak{b}" for b in range(buses)],
        bus=bus_names,
        p_nom=400.0,
        marginal_cost=[85.0 + (b % 11) for b in range(buses)],
    )
    wind_profile = np.clip(
        0.45
        + 0.45
        * np.sin(
            (t[:, None] + np.arange(buses)[None, :] * 7) * 2.0 * math.pi / 24.0
        ),
        0.0,
        1.0,
    )
    n.add(
        "Generator",
        [f"wind{b}" for b in range(buses)],
        bus=bus_names,
        p_nom=600.0,
        marginal_cost=0.0,
        p_max_pu=pd.DataFrame(
            wind_profile, index=n.snapshots, columns=[f"wind{b}" for b in range(buses)]
        ),
    )

    # Load shedding. PyPSA has no shed variable, so the idiomatic encoding is a
    # generator per bus priced at the value of lost load, which is what
    # PyPSA-Eur itself does.
    n.add(
        "Generator",
        [f"shed{b}" for b in range(buses)],
        bus=bus_names,
        p_nom=1e5,
        marginal_cost=VOLL,
    )

    demand = 700.0 * (1.0 + 0.25 * np.sin(t * 2.0 * math.pi / 24.0))[:, None] + (
        np.arange(buses) % 13
    )[None, :] * 10.0
    n.add(
        "Load",
        [f"load{b}" for b in range(buses)],
        bus=bus_names,
        p_set=pd.DataFrame(
            demand, index=n.snapshots, columns=[f"load{b}" for b in range(buses)]
        ),
    )

    n.add(
        "StorageUnit",
        [f"batt{s}" for s in range(stores)],
        bus=[bus_names[(s * 4) % buses] for s in range(stores)],
        p_nom=200.0,
        max_hours=6.0,
        efficiency_store=0.92,
        efficiency_dispatch=0.92,
        cyclic_state_of_charge=True,
    )

    return n


def gridwright_counts(buses: int, hours: int):
    """What gridwright produces for the same network, derived not copied.

    Per snapshot: a balance row per bus carrying every generator, both endpoints
    of every line, a charge and a discharge term per store and one shed term; a
    DC flow row per line carrying the flow and two angles; a cyclic storage row
    per store carrying four terms.
    """
    bus0, _ = topology(buses)
    lines = len(bus0)
    gens = buses * 3
    stores = max(buses // 4, 1)
    cols = (gens + lines + buses + buses + 3 * stores) * hours
    rows = (buses + lines + stores) * hours
    nnz = (
        (gens + 2 * lines + 2 * stores + buses) + (3 * lines) + (4 * stores)
    ) * hours
    return cols, rows, nnz


def run_once(buses: int, hours: int, want_matrix: bool):
    gc.collect()

    t0 = time.perf_counter()
    n = make_network(buses, hours)
    setup_s = time.perf_counter() - t0

    t1 = time.perf_counter()
    m = n.optimize.create_model(consistency_check=False)
    build_s = time.perf_counter() - t1

    export_s = float("nan")
    nnz = -1
    if want_matrix:
        t2 = time.perf_counter()
        try:
            nnz = m.matrices.A.nnz
            export_s = time.perf_counter() - t2
        except Exception as exc:  # noqa: BLE001
            print(f"    (matrix export failed: {type(exc).__name__}: {exc})")

    blocks = {}
    for name in m.constraints:
        try:
            blocks[name] = int(m.constraints[name].size)
        except Exception:  # noqa: BLE001
            pass

    result = dict(
        setup_s=setup_s,
        build_s=build_s,
        export_s=export_s,
        nvars=int(m.nvars),
        ncons=int(m.ncons),
        nnz=nnz,
        blocks=blocks,
        var_blocks={name: int(m.variables[name].size) for name in m.variables},
    )
    del m, n
    gc.collect()
    return result


def report(r, buses: int, hours: int):
    g_cols, g_rows, g_nnz = gridwright_counts(buses, hours)

    print("\nvariable blocks PyPSA created:")
    for name, size in sorted(r["var_blocks"].items()):
        print(f"  {name:<34} {size:>14,}")
    print("\nconstraint blocks PyPSA created:")
    for name, size in sorted(r["blocks"].items()):
        print(f"  {name:<34} {size:>14,}")

    print("\ncounts, PyPSA against gridwright on the same network:")
    print(f"  {'':<12} {'PyPSA':>14} {'gridwright':>14} {'difference':>14}")
    rows = [
        ("columns", r["nvars"], g_cols),
        ("rows", r["ncons"], g_rows),
        ("nonzeros", r["nnz"], g_nnz),
    ]
    mismatched = False
    for label, got, want in rows:
        if got < 0:
            print(f"  {label:<12} {'not measured':>14} {want:>14,}")
            continue
        if got != want:
            mismatched = True
        print(f"  {label:<12} {got:>14,} {want:>14,} {got - want:>+14,}")

    if mismatched:
        print(
            "\n  *** THE COUNTS DO NOT MATCH, and were never going to. PyPSA "
            "uses the\n"
            "      Kirchhoff cycle formulation rather than voltage angles, and "
            "it emits\n"
            "      fixed capacity limits as rows where gridwright puts them in "
            "the bound\n"
            "      vectors. This row is therefore 'what PyPSA costs to reach a "
            "matrix\n"
            "      for this network', not a same-matrix comparison. See "
            "jump_build.jl\n"
            "      for the benchmark whose counts do match exactly. ***"
        )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--buses", type=int, default=256)
    ap.add_argument("--hours", type=int, default=8760)
    ap.add_argument("--reps", type=int, default=3)
    ap.add_argument("--matrix", action="store_true", help="also time matrix export")
    args = ap.parse_args()

    print(f"pypsa {pypsa.__version__}")
    try:
        import linopy

        print(f"  built on linopy {linopy.__version__}, which is what PyPSA now "
              f"uses internally")
    except ImportError:
        pass

    print(
        f"\nsynthetic network: {args.buses} buses x {args.hours} snapshots, "
        f"best of {args.reps}"
    )

    best = None
    for i in range(args.reps):
        r = run_once(args.buses, args.hours, args.matrix)
        line = (
            f"  run {i + 1}: data setup {r['setup_s']:8.3f} s   "
            f"create_model {r['build_s']:8.3f} s"
        )
        if r["nnz"] >= 0:
            line += f"   matrix export {r['export_s']:8.3f} s"
        print(line)
        if best is None or r["build_s"] < best["build_s"]:
            best = r

    report(best, args.buses, args.hours)

    print()
    print(f"  data setup:   {best['setup_s']:10.3f} s  (not the number under test)")
    print(f"  CONSTRUCTION: {best['build_s']:10.3f} s  (best of {args.reps})")
    if best["nnz"] >= 0:
        print(f"  matrix export:{best['export_s']:10.3f} s")
        print(
            f"  throughput:   {best['nnz'] / best['build_s'] / 1e6:10.2f} "
            f"M nonzeros/s"
        )


if __name__ == "__main__":
    main()
