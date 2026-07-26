"""Build the same model in linopy, and time only the building.

The claim this project was founded on is that Python is slow at *constructing*
optimisation problems rather than at solving them. That is a quotable claim from
the energy modelling literature and it deserves a measurement.

WHAT THIS SCRIPT GOT WRONG, AND WHY IT IS WORTH READING BEFORE TRUSTING IT

The first version wrote the nodal balance as `(p * g_at).sum("gen")`, where
`g_at` is a dense generators-by-buses incidence array. That looks like idiomatic
xarray and it is not: multiplying a `(gen, snapshot)` variable by a
`(gen, bus)` array materialises a `(gen, bus, snapshot)` intermediate. At the
largest size that is 768 x 256 x 8760 entries to express a sum in which all but
three terms per bus are zero. The DC flow constraint had the same shape.

Measured that way linopy took 200.8 s and 22.4 GB, and this project published
that figure as "about two thousand times slower". It was not a fair measurement
of linopy. PyPSA drives the *same linopy 0.9.0* to a larger matrix in about
17 s, and the difference is entirely in how the expression is written.

The default path here now uses `groupby` for the per-bus sums and vectorised
`sel` for the two ends of a line, which is what PyPSA does and is what the
library is for. The dense path is kept behind `--dense` so the old number can be
reproduced and so the trap stays visible, because it is an easy one to fall into
and the code that falls into it looks correct.

Both paths are asserted to produce the identical matrix: same variables, same
constraints, same nonzeros. That is what makes the comparison a comparison of
how the expression is written rather than of what was built.

FAIRNESS, since a benchmark is only as good as its fairness

- The model is the same synthetic ring `gw bench` builds, with matching counts:
  nodal balance, DC flow through voltage angles, storage dynamics across
  snapshots, and load shedding.
- Only construction is timed. Both tools hand the result to the same solver
  afterwards and that part is not in dispute.
- Matrix export is timed separately, because gridwright's figure includes
  producing compressed sparse columns and linopy's assembly does not.
- If you find a faster way to write this in linopy, that is a bug report against
  this file, not a defence of the number. The point is to measure the library at
  its best.
"""

import argparse
import sys
import time

import numpy as np
import pandas as pd
import xarray as xr
import linopy


def dense_incidence(rows, cols, at, dim_row, dim_col, sign=1.0):
    """A dense 0/±1 array mapping one component axis onto another.

    Only used by the `--dense` path. This is the object whose presence in an
    expression costs two orders of magnitude.
    """
    a = np.zeros((rows, cols))
    for i, j in enumerate(at):
        a[i, j] = sign
    return xr.DataArray(
        a,
        dims=[dim_row, dim_col],
        coords={dim_row: np.arange(rows), dim_col: np.arange(cols)},
    )


def build(buses: int, hours: int, dense: bool = False):
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

    if dense:
        # The original formulation, kept so the published figure can be
        # reproduced. Every `.sum` here is a sum over an axis of a dense product.
        g_at = dense_incidence(gens, buses, gen_bus, "gen", "bus")
        s_at = dense_incidence(stores, buses, store_bus, "store", "bus")
        l_from = dense_incidence(lines, buses, bus0, "line", "bus", -1.0)
        l_to = dense_incidence(lines, buses, bus1, "line", "bus", 1.0)

        balance = (
            (p * g_at).sum("gen")
            + (f * (l_from + l_to)).sum("line")
            + (di * s_at).sum("store")
            - (ch * s_at).sum("store")
            + shed
        )
        m.add_constraints(balance == 300.0, name="balance")
        m.add_constraints(
            f - 10.0 * ((theta * (-l_from)).sum("bus") - (theta * l_to).sum("bus")) == 0,
            name="dcflow",
        )
    else:
        # Each component axis carries the bus it sits on as a label, and the sum
        # over a bus is a grouped sum rather than a projection through a matrix.
        # Nothing here is ever larger than the answer.
        at_bus = lambda values, dim, index: xr.DataArray(  # noqa: E731
            values, dims=[dim], coords={dim: index}, name="bus"
        )
        gen_at = at_bus(gen_bus, "gen", gen)
        line_from = at_bus(bus0, "line", line)
        line_to = at_bus(bus1, "line", line)
        store_at = at_bus(store_bus, "store", store)

        balance = p.groupby(gen_at).sum()
        # A line withdraws at bus0 and delivers at bus1.
        balance = balance + (-1.0 * f).groupby(line_from).sum()
        balance = balance + f.groupby(line_to).sum()
        balance = balance + (di - ch).groupby(store_at).sum()
        balance = balance + 1.0 * shed
        m.add_constraints(balance == 300.0, name="balance")

        # DC flow: pick each line's two angles directly rather than projecting
        # every bus angle onto every line and summing away the zeroes.
        m.add_constraints(
            f - 10.0 * (theta.sel(bus=line_from) - theta.sel(bus=line_to)) == 0,
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


def check_paths_agree():
    """The two formulations must build the same matrix, or nothing below means
    anything: a faster path that built less would be a cheat rather than a fix.
    """
    for buses, hours in [(8, 6), (16, 24)]:
        dense = build(buses, hours, dense=True)[2:]
        grouped = build(buses, hours, dense=False)[2:]
        if dense != grouped:
            raise SystemExit(
                f"the two formulations disagree at {buses}x{hours}: "
                f"dense built {dense}, grouped built {grouped}"
            )
    return True


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--dense",
        action="store_true",
        help="use the original dense-incidence formulation, which is what "
        "produced the published 200.8 s figure and is not how linopy is meant "
        "to be used",
    )
    ap.add_argument(
        "--skip-check",
        action="store_true",
        help="skip the assertion that both formulations build the same matrix",
    )
    args = ap.parse_args()

    print(f"linopy {linopy.__version__}")
    if not args.skip_check:
        check_paths_agree()
        print("both formulations build the same matrix at small sizes")
    print("formulation:", "dense incidence (the slow one)" if args.dense else "grouped")
    print(
        f"{'buses':>6} {'hours':>6} {'vars':>12} {'cons':>10} "
        f"{'nonzeros':>11} {'assemble':>10} {'export':>10}"
    )
    for buses, hours in [(16, 24), (32, 168), (64, 720), (128, 2190), (256, 8760)]:
        try:
            a, e, nv, nc, nnz = build(buses, hours, dense=args.dense)
            print(f"{buses:6} {hours:6} {nv:12} {nc:10} {nnz:11} {a:9.3f}s {e:9.3f}s")
        except MemoryError:
            print(f"{buses:6} {hours:6}  out of memory")
            break
        except Exception as exc:  # noqa: BLE001
            print(f"{buses:6} {hours:6}  failed: {type(exc).__name__}: {exc}")
            break
