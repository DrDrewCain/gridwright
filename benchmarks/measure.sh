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
#   GW_MAX_LOAD    one-minute load average above which it will not start (2.0)
#   GW_WAIT_MINS   how long to wait for the machine to go quiet (120)
#   GW_RUNS        repetitions (3)

set -u
cd "$(dirname "$0")/.."

LABEL="${1:?usage: measure.sh <label> <command...>}"
shift

MAX_LOAD="${GW_MAX_LOAD:-2.0}"
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
printf 'waiting for an idle machine (load below %s, nothing of ours running)\n' "$MAX_LOAD"

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

# Warm once and discard. The first run in a fresh process takes its page faults,
# which is a real cost for a one-shot CLI and noise for everything else.
printf 'warm-up (discarded)\n'
"$@" >/dev/null 2>&1

for i in $(seq 1 "$RUNS"); do
  printf -- '--- run %s of %s (load %s) ---\n' "$i" "$RUNS" "$(one_minute_load)"
  "$@" 2>&1
done

printf 'final load %s\n' "$(one_minute_load)"
printf '=== %s done ===\n' "$LABEL"
