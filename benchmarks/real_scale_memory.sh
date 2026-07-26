#!/usr/bin/env bash
# Peak resident memory for the real-network scaling ladder, one rung per process.
#
# Why this exists rather than a column in the test's own output. `getrusage`
# reports a high-water mark for the whole process, so when four rungs share one
# test binary, every rung after the first inherits the largest peak reached so
# far. The rungs run in increasing size and each model is dropped before the
# next, which makes the reported figure approximately right, but "approximately"
# is not a standard this project gets to use about memory: the founding claim in
# the README is a memory claim, 1.50 GB against 22.4 GB, and a memory number
# measured loosely here would sit next to it and be read as comparable.
#
# So each rung gets its own process and its own high-water mark, taken from
# `/usr/bin/time -l`, which reports the kernel's figure rather than the
# program's opinion of it. The compile happens once up front, outside the
# measured runs, because a cargo invocation that decides to rebuild would put a
# rustc process's memory into the wrong column entirely.
#
# Usage, from the repository root:
#
#     ./benchmarks/real_scale_memory.sh
#
# Requires `python3 benchmarks/fetch_opsd_time_series.py` to have been run once;
# without it every rung prints its skip message and exits without measuring.
#
# The load average is printed before each rung. Anything much above two on a
# machine with cores to spare means something else is running, and a timing
# taken alongside it should not be quoted. Three numbers in this project have
# already had to be withdrawn for exactly that.

set -euo pipefail

cd "$(dirname "$0")/.."

if [[ ! -f benchmarks/.cache/opsd_de_control_zones_2019_hourly.csv ]]; then
  echo "The time series has not been fetched. Run:"
  echo "    python3 benchmarks/fetch_opsd_time_series.py"
  exit 1
fi

echo "Compiling the test binary once, so no rung pays for a build."
cargo test -p gridwright-solve --all-features --test real_scale --release --no-run 2>&1 | tail -2

# The same three rungs LADDER in real_scale.rs solves. IEEE 300 is not here
# because its whole-year solve is not attempted; see the comment on LADDER.
for case in case14_ieee case57_ieee case118_ieee; do
  echo
  echo "=== ${case} ==="
  echo "load average before: $(uptime | sed 's/.*load averages*: //')"
  # `-l` is the BSD form and prints "maximum resident set size" in bytes. On
  # Linux the equivalent is `/usr/bin/time -v`, whose figure is in kilobytes.
  GRIDWRIGHT_REAL_CASE="${case}" /usr/bin/time -l \
    cargo test -p gridwright-solve --all-features --test real_scale --release \
    -- --ignored --nocapture --exact one_rung_in_its_own_process_for_an_honest_memory_figure \
    2>&1 | grep -E "case|maximum resident set size|real |skipping|Run "
done
