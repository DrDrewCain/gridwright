#!/usr/bin/env python3
"""Fetch a real year of hourly European power system time series.

Why this exists. Every scaling number published for gridwright was measured on
a synthetic ring: a regular topology with an invented demand profile. A regular
topology may flatter the solve, and an invented profile certainly does, because
a smooth analytic curve has none of the awkward structure that real demand and
real weather have. The topology half of that problem was already solved, since
the PGLib cases in `examples/pglib` are real published networks and the
MATPOWER reader reads them. The missing half was a real year of hourly data to
hang on them, and this script is that half.

What it fetches. The Open Power System Data "time series" package, version
2020-10-06, which is a single 130 MB CSV of hourly load, wind and solar for
Europe assembled from the ENTSO-E Transparency Platform. From it this script
keeps one calendar year for the four German transmission control zones,
50Hertz, Amprion, TenneT DE and TransnetBW, and writes a distilled CSV of a
megabyte or so.

Why the four German control zones rather than one national series. Applying a
single national profile to every bus makes demand perfectly correlated across
the whole network, which is the one thing guaranteed to make transmission
constraints easy: if every bus rises and falls together in proportion, very
little extra power needs to move. Four measured regional series restore some of
the spatial diversity that a real system has. Four is not many, and the
remaining correlation within a zone is still an artefact, but it is four real
measurements rather than one, and the direction of the residual error is known
and stated rather than hidden.

Why the data is cached and not committed. The distilled file is small enough to
commit, and the repository already carries a 7.5 MB MATPOWER case, so size is
not the objection. The objection is licensing. Open Power System Data publishes
its own processing scripts under MIT and aims to release its packages under
CC-BY, but says plainly that it cannot for every source: the underlying ENTSO-E
Transparency data remains subject to the terms of the Transparency Platform and
to copyright held by the primary data owners, which is a weaker and less
explicit grant than the CC-BY 4.0 under which the PGLib cases in this repository
are redistributed. Vendoring data whose redistribution terms are unclear into a
repository that is offered under two licences is a risk taken for very little
gain, so the data is fetched rather than shipped, into a gitignored cache. The
cost is that `real_scale.rs` skips with an explanatory message on a fresh clone
until this script has been run once.

Attribution, as prescribed by the publisher:

    Open Power System Data. 2020. Data Package Time series.
    Version 2020-10-06. https://doi.org/10.25832/time_series/2020-10-06
    (Primary data from various sources, for a complete list see the URL.)

Primary data: ENTSO-E Transparency Platform.

Usage:

    python3 benchmarks/fetch_opsd_time_series.py [--year 2019]

The 130 MB download is itself cached, so a re-run with a different year does
not fetch it again.
"""

from __future__ import annotations

import argparse
import csv
import os
import sys
import urllib.request
from pathlib import Path

# Pinned to a dated version rather than to `latest`. `latest` redirects, and a
# benchmark whose input silently changes underneath it is a benchmark whose
# numbers cannot be compared with the ones published last month.
OPSD_VERSION = "2020-10-06"
OPSD_URL = (
    "https://data.open-power-system-data.org/time_series/"
    f"{OPSD_VERSION}/time_series_60min_singleindex.csv"
)

CACHE = Path(__file__).resolve().parent / ".cache"
RAW = CACHE / f"opsd_time_series_60min_singleindex_{OPSD_VERSION}.csv"

# The four German transmission control zones. Each is a real balancing area
# with its own metered load and its own metered wind and solar output, which is
# exactly the granularity that makes the profiles spatially distinct instead of
# four copies of one national curve.
ZONES = ["50hertz", "amprion", "tennet", "transnetbw"]


def source_columns(zone: str) -> tuple[str, str, str]:
    """The three OPSD column names this script reads for one control zone.

    Onshore wind for every zone, including the two coastal ones that also have
    offshore. Mixing onshore for two zones and onshore-plus-offshore for the
    other two would put a different technology mix in different places for no
    reason connected to the networks being measured, and the offshore series
    would then be doing work that the topology cannot see.
    """
    return (
        f"DE_{zone}_load_actual_entsoe_transparency",
        f"DE_{zone}_solar_generation_actual",
        f"DE_{zone}_wind_onshore_generation_actual",
    )


def download(url: str, dest: Path) -> None:
    """Fetch `url` to `dest`, skipping the work if it is already there.

    Writes to a temporary name and renames on success. An interrupted download
    that left a truncated file in place would be read without complaint by the
    distillation step below and would silently shorten the year.
    """
    if dest.exists() and dest.stat().st_size > 0:
        print(f"cached: {dest} ({dest.stat().st_size / 1e6:.0f} MB)")
        return
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".partial")
    print(f"fetching {url}")
    with urllib.request.urlopen(url, timeout=300) as response, open(tmp, "wb") as out:
        total = int(response.headers.get("content-length", 0))
        seen = 0
        while chunk := response.read(1 << 20):
            out.write(chunk)
            seen += len(chunk)
            if total:
                print(f"\r  {seen / 1e6:6.0f} / {total / 1e6:.0f} MB", end="")
        print()
    os.replace(tmp, dest)
    print(f"saved: {dest} ({dest.stat().st_size / 1e6:.0f} MB)")


def distil(year: int) -> Path:
    """Cut one calendar year of four control zones out of the full package.

    Renewable output arrives in megawatts, and what the model wants is a
    per-unit availability in [0, 1]. Installed capacity per control zone is not
    in this package, only national capacity, so each series is normalised by its
    own maximum over the year. That makes the *shape* measured and the *level*
    a scaling choice: the best hour of the year becomes full output, which
    overstates the annual capacity factor relative to a normalisation by
    nameplate, because nothing ever quite reaches nameplate. The mean of each
    normalised series is printed below so the size of that effect is visible
    rather than assumed.
    """
    out_path = CACHE / f"opsd_de_control_zones_{year}_hourly.csv"

    stamps: list[str] = []
    series: dict[str, list[float]] = {}
    for zone in ZONES:
        for kind in ("load", "solar", "wind"):
            series[f"{zone}_{kind}"] = []

    with open(RAW, newline="") as handle:
        reader = csv.reader(handle)
        header = next(reader)
        try:
            index = {
                f"{zone}_{kind}": header.index(column)
                for zone in ZONES
                for kind, column in zip(("load", "solar", "wind"), source_columns(zone))
            }
        except ValueError as missing:
            raise SystemExit(
                f"the OPSD package no longer has a column this script needs: {missing}. "
                "The column naming changed between package versions, so check "
                "OPSD_VERSION and source_columns() together."
            ) from missing
        stamp_at = header.index("utc_timestamp")

        prefix = f"{year}-"
        for row in reader:
            stamp = row[stamp_at]
            if not stamp.startswith(prefix):
                continue
            values = {key: row[position] for key, position in index.items()}
            # A gap anywhere in the row means the hour is dropped entirely
            # rather than interpolated. Interpolating would invent the very
            # thing this whole exercise exists to avoid inventing, and refusing
            # the year outright is better than quietly measuring 8,713 hours
            # while the table says 8,760.
            if any(value == "" for value in values.values()):
                raise SystemExit(
                    f"{stamp} has a missing value in {year}; this script will not "
                    "interpolate. Pick a different year, or a different set of zones."
                )
            stamps.append(stamp)
            for key, value in values.items():
                series[key].append(float(value))

    hours = len(stamps)
    if hours == 0:
        raise SystemExit(f"the package contains no rows for {year}.")
    # A leap year has 8,784 hours and a clock change does not affect a UTC
    # index, so anything other than 8,760 or 8,784 means the year is partial.
    if hours not in (8760, 8784):
        raise SystemExit(
            f"{year} yielded {hours} hourly rows, not a whole year. The package "
            f"version {OPSD_VERSION} covers 2015 to mid-2020, so the first and "
            "last years it touches are incomplete."
        )

    print(f"\n{year}: {hours} hourly rows, no gaps\n")
    print("  zone         mean load MW   peak load MW   solar cf   wind cf")
    normalised: dict[str, list[float]] = {}
    for zone in ZONES:
        load = series[f"{zone}_load"]
        normalised[f"{zone}_load"] = load
        line = f"  {zone:<11}  {sum(load) / hours:12,.0f}   {max(load):12,.0f}"
        for kind in ("solar", "wind"):
            raw = series[f"{zone}_{kind}"]
            peak = max(raw)
            if peak <= 0:
                raise SystemExit(f"{zone} {kind} is zero all year; it cannot be normalised.")
            unit = [value / peak for value in raw]
            normalised[f"{zone}_{kind}"] = unit
            line += f"   {sum(unit) / hours:8.3f}"
        print(line)

    columns = ["utc_timestamp"]
    for zone in ZONES:
        columns += [f"{zone}_load_mw", f"{zone}_solar_pu", f"{zone}_wind_pu"]

    with open(out_path, "w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(columns)
        for hour in range(hours):
            row = [stamps[hour]]
            for zone in ZONES:
                row.append(f"{normalised[f'{zone}_load'][hour]:.1f}")
                row.append(f"{normalised[f'{zone}_solar'][hour]:.5f}")
                row.append(f"{normalised[f'{zone}_wind'][hour]:.5f}")
            writer.writerow(row)

    print(f"\nwrote {out_path} ({out_path.stat().st_size / 1e3:.0f} kB)")
    return out_path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--year",
        type=int,
        default=2019,
        help=(
            "calendar year to extract. 2019 by default: it is the last whole "
            "year this package version covers, and it predates the 2020 demand "
            "collapse, which is a real event but not a representative one."
        ),
    )
    args = parser.parse_args()
    download(OPSD_URL, RAW)
    distil(args.year)
    print(
        "\nAttribution, required by the publisher and reproduced wherever these\n"
        "numbers appear:\n"
        "  Open Power System Data. 2020. Data Package Time series.\n"
        f"  Version {OPSD_VERSION}. https://doi.org/10.25832/time_series/{OPSD_VERSION}\n"
        "  Primary data from the ENTSO-E Transparency Platform."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
