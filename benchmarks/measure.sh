#!/bin/zsh
#
# Run a measurement only when the machine is actually idle, and report the
# spread rather than a single number.
#
# This exists because load has corrupted four measurements in this project, and
# every one of them survived into a document as a fact:
#
#   - the CSR to CSC transpose was recorded as taking 90 ms. It takes about 21.
#     Three different counting strategies were then compared and all measured
#     the same, because the measurement was environment-bound and the algorithm
#     could not move it.
#   - a page-prefault experiment measured backwards, appearing to make
#     construction slower, while three agents were compiling.
#   - the whole-year scaling table was wrong in every row. 16 buses was
#     published as 20 s and is 10.3, 32 as 194 s and is 31.0, and 64 was
#     published as "did not finish in seven minutes" and takes 110.7.
#   - the large PGLib simplex ladder drifted between passes, 23.5 s to 42 s and
#     197 s to 230 s, while other benchmarks were running.
#
# The common thread is that nothing stopped anyone measuring on a busy machine.
# A number that is wrong by six times is worse than no number, because it gets
# quoted. So this refuses rather than warns.
#
# Usage:
#   benchmarks/measure.sh <label> <command...>
#
# Environment:
#   GW_MAX_LOAD    one-minute load average above which it will not start
#                  (default: half the core count)
#   GW_WAIT_MINS   how long to wait for the machine to go quiet (120)
#   GW_RUNS        repetitions (3)
#   GW_WARMUP      run once and discard before timing (off; see below)
#
# On reporting. Every run's output is printed in full and nothing here reduces
# them to one number, which is deliberate. This project spent a while quoting
# best-of-N, and best-of-N is the wrong summary for the question people actually
# ask of these tables. It is the right statistic for comparing two
# implementations, because taking the fastest observation of each strips noise
# that belongs to neither. It is the wrong one for "what will this cost me",
# because the fastest run is a floor: measured across five runs the whole-year
# ladder sits 2 to 4% above its own best, and a single rung was 7% above.
#
# So a table built from this script should quote every observation and say how
# many there were, rather than a best with the spread mentioned in prose
# afterwards. A reader can compute whatever summary they want from n values and
# cannot recover anything from one.
#
# On the threshold. The first version of this refused above a load of 2.0 and
# then never ran, because an interactive desktop does not go that quiet: a
# browser, the window server and the editor together hold a steady 4 to 5 on a
# 14-core machine, and none of that is going away while anybody is using it.
# Waiting for a number that cannot occur is not caution, it is a gate that never
# opens, so the default is now half the cores.
#
# What actually ruins a measurement here is not background load in general, it
# is *another build or benchmark of ours* competing for the same cores, which is
# what happened all four times. That check has no threshold and is not
# negotiable. The load figure is a second line of defence and is recorded beside
# every run so a reader can judge it rather than trust it.

set -u
cd "$(dirname "$0")/.."

LABEL="${1:?usage: measure.sh <label> <command...>}"
shift

CORES="$(sysctl -n hw.ncpu 2>/dev/null || echo 8)"
MAX_LOAD="${GW_MAX_LOAD:-$(printf '%s\n' "scale=1; $CORES / 2" | bc -l)}"
WAIT_MINS="${GW_WAIT_MINS:-120}"
RUNS="${GW_RUNS:-3}"

one_minute_load() {
  uptime | sed -E 's/.*load averages?: ([0-9.]+).*/\1/'
}

# Something of ours already running is disqualifying regardless of load: a
# benchmark that competes with another benchmark is measuring the scheduler.
ours_running() {
  pgrep -f "target/(release|debug)/deps/" >/dev/null 2>&1 && return 0
  pgrep -x rustc >/dev/null 2>&1 && return 0
  return 1
}

printf '=== %s ===\n' "$LABEL"
printf 'machine has %s cores; waiting for load below %s and nothing of ours running\n' \
  "$CORES" "$MAX_LOAD"

waited=0
while :; do
  load="$(one_minute_load)"
  quiet=$(printf '%s < %s\n' "$load" "$MAX_LOAD" | bc -l)
  if [ "$quiet" = "1" ] && ! ours_running; then
    printf 'machine idle at load %s after %s min\n' "$load" "$waited"
    break
  fi
  if [ "$waited" -ge "$WAIT_MINS" ]; then
    printf 'REFUSING TO MEASURE: still at load %s after %s minutes.\n' "$load" "$waited"
    printf 'A number measured here would be wrong by an unknown factor, and this\n'
    printf 'project has published four such numbers already.\n'
    exit 1
  fi
  sleep 60
  waited=$((waited + 1))
done

# Warm-up is off by default, which is a correction rather than a preference.
#
# It was on, reasoning that a first run in a fresh process pays its page faults
# and that cost is noise. True of a benchmark measured in milliseconds. For a
# long one it simply doubles the bill: a ladder here that takes twenty-five
# minutes spent the first twenty-five inside a warm-up whose output was thrown
# away.
#
# It is also redundant whenever RUNS is greater than one, since taking the best
# of several runs already discards a slow first one, which is the whole thing
# the warm-up was for.
#
# That reasoning was first written down more broadly than the evidence
# supported, so here is the evidence. A first-run penalty is real, it is in
# *construction* rather than in solving, and it scales with the model: building
# case2869_pegase, 68 M rows and 13 GB, takes 1.5 s on the first run against a
# 421 ms median of the four after it. Solves show no such bias at any size
# measured, down to 3.9 ms, so this is page-faulting freshly allocated matrices
# rather than process warm-up in general.
#
# The consequence is about which summary you take, not about the warm-up.
# A median is robust to one slow observation by construction; a *mean* is not,
# and would read 51% high on that row. So: leave the warm-up off, take n >= 5,
# and quote the median. Turn GW_WARMUP on when n is small and construction
# milliseconds are the measurement.
if [ "${GW_WARMUP:-0}" != "0" ]; then
  printf 'warm-up (discarded)\n'
  "$@" >/dev/null 2>&1
fi

for i in $(seq 1 "$RUNS"); do
  before="$(one_minute_load)"
  printf -- '--- run %s of %s (load before %s) ---\n' "$i" "$RUNS" "$before"
  "$@" 2>&1
  printf -- '--- run %s load after %s ---\n' "$i" "$(one_minute_load)"
done

printf 'final load %s\n' "$(one_minute_load)"
printf '=== %s done ===\n' "$LABEL"
