"""Build the same model in linopy, and time only the building.

The claim this project was founded on is that Python is slow at *constructing*
optimisation problems rather than at solving them. That is a quotable claim
from the energy modelling literature and it deserves a measurement.

Fairness, since a benchmark is only as good as its fairness:

- linopy is used the way it is meant to be, vectorised over xarray dimensions
  with incidence arrays. Python loops over components would measure the wrong
  thing.
- The model is the same synthetic ring `gw bench` builds, with the same
  variable and constraint counts: nodal balance, DC flow with angles, storage
  dynamics across snapshots, and shedding.
- Only construction is timed. Both tools hand the result to the same solver
  afterwards and that part is not in dispute.
- Matrix export is timed separately, because gridwright's figure includes
  producing compressed sparse columns while linopy's assembly does not.
"""

import sys
import time

import numpy as np
import pandas as pd
import xarray as xr
import linopy


def incidence(rows, cols, at, dim_row, dim_col, sign=1.0):
    """A sparse-in-spirit 0/±1 array mapping one component axis onto another."""
    a = np.zeros((rows, cols))
    for i, j in enumerate(at):
        a[i, j] = sign
    return xr.DataArray(
        a,
        dims=[dim_row, dim_col],
        coords={dim_row: np.arange(rows), dim_col: np.arange(cols)},
    )


def build(buses: int, hours: int):
    lines = buses * 3 // 2
    gens = buses * 3
    stores = max(buses // 4, 1)

    snap = pd.RangeIndex(hours, name="snapshot")
    bus = pd.RangeIndex(buses, name="bus")
    gen = pd.RangeIndex(gens, name="gen")
    line = pd.RangeIndex(lines, name="line")
    store = pd.RangeIndex(stores, name="store")

    # Topology: a ring plus chords, generators and stores spread over the buses.
    bus0 = [i % buses for i in range(lines)]
    bus1 = [(i + 1) % buses for i in range(lines)]
    gen_bus = [i % buses for i in range(gens)]
    store_bus = [(i * 4) % buses for i in range(stores)]

    t0 = time.perf_counter()
    m = linopy.Model()

    p = m.add_variables(lower=0.0, upper=400.0, coords=[gen, snap], name="p")
    f = m.add_variables(lower=-500.0, upper=500.0, coords=[line, snap], name="f")
    theta = m.add_variables(lower=-np.pi, upper=np.pi, coords=[bus, snap], name="theta")
    shed = m.add_variables(lower=0.0, coords=[bus, snap], name="shed")
    soc = m.add_variables(lower=0.0, upper=600.0, coords=[store, snap], name="soc")
    ch = m.add_variables(lower=0.0, upper=100.0, coords=[store, snap], name="ch")
    di = m.add_variables(lower=0.0, upper=100.0, coords=[store, snap], name="di")

    g_at = incidence(gens, buses, gen_bus, "gen", "bus")
    s_at = incidence(stores, buses, store_bus, "store", "bus")
    # A line withdraws at bus0 and delivers at bus1.
    l_from = incidence(lines, buses, bus0, "line", "bus", -1.0)
    l_to = incidence(lines, buses, bus1, "line", "bus", 1.0)

    balance = (
        (p * g_at).sum("gen")
        + (f * (l_from + l_to)).sum("line")
        + (di * s_at).sum("store")
        - (ch * s_at).sum("store")
        + shed
    )
    m.add_constraints(balance == 300.0, name="balance")

    # DC flow: f = B (theta_from - theta_to).
    m.add_constraints(
        f - 10.0 * ((theta * (-l_from)).sum("bus") - (theta * l_to).sum("bus")) == 0,
        name="dcflow",
    )

    # Storage dynamics across snapshots.
    m.add_constraints(
        soc - soc.shift(snapshot=1) - 0.94 * ch + di / 0.94 == 0,
        name="storage",
    )

    assembled = time.perf_counter() - t0

    t1 = time.perf_counter()
    exported = float("nan")
    nnz = 0
    try:
        a = m.matrices.A
        nnz = a.nnz
        exported = time.perf_counter() - t1
    except Exception as e:  # noqa: BLE001
        print(f"    (matrix export failed: {type(e).__name__}: {e})", file=sys.stderr)

    return assembled, exported, m.nvars, m.ncons, nnz


if __name__ == "__main__":
    print(f"linopy {linopy.__version__}")
    print(f"{'buses':>6} {'hours':>6} {'vars':>12} {'cons':>10} {'nonzeros':>11} {'assemble':>10} {'export':>10}")
    for buses, hours in [(16, 24), (32, 168), (64, 720), (128, 2190), (256, 8760)]:
        try:
            a, e, nv, nc, nnz = build(buses, hours)
            print(f"{buses:6} {hours:6} {nv:12} {nc:10} {nnz:11} {a:9.3f}s {e:9.3f}s")
        except MemoryError:
            print(f"{buses:6} {hours:6}  out of memory")
            break
        except Exception as exc:  # noqa: BLE001
            print(f"{buses:6} {hours:6}  failed: {type(exc).__name__}: {exc}")
            break
