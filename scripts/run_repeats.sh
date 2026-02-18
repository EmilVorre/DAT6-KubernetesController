#!/usr/bin/env bash
# Run a scenario N times and produce stable aggregated metrics.
#
# Usage: ./run_repeats.sh [N] [SCENARIO] [STRAT]
#   N: number of repeats (default 5)
#   SCENARIO: steady_scale_down | rollout | delete_pod
#   STRAT: baseline | s1-early-readiness | long-requests | burst | ...
#
# Output: runs/<timestamp>-<scenario>-<strat>-repeats/
#   run_1/ ... run_N/   — same structure as run_scenario.sh (summary.csv, client_results.json, ...)
#   summary_repeats.csv — all runs concatenated (run,total,errors,loss_pct,p50_ms,p95_ms,p99_ms)
#   aggregate.json      — mean, min, max (and optional std) for loss_pct, p50_ms, p95_ms, p99_ms
#
# Requires: kubectl, k6, python3, kind cluster; for S1 also run controller with DAT6_EARLY_READINESS_REMOVAL=1

set -euo pipefail

N="${1:-5}"
SCENARIO="${2:-steady_scale_down}"
STRAT="${3:-baseline}"
TIMESTAMP="${TIMESTAMP:-$(date +%Y%m%d-%H%M%S)}"
RUN_DIR="${RUN_DIR:-runs}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUTPUT_PARENT="$RUN_DIR/${TIMESTAMP}-${SCENARIO}-${STRAT}-repeats"

echo "Running $N repeats: SCENARIO=$SCENARIO STRAT=$STRAT"
echo "Output parent: $OUTPUT_PARENT"
mkdir -p "$OUTPUT_PARENT"

for i in $(seq 1 "$N"); do
  echo "--- Repeat $i / $N ---"
  bash "$SCRIPT_DIR/scripts/run_scenario.sh" "$SCENARIO" "$STRAT" "$OUTPUT_PARENT/run_$i"
done

# Aggregate: summary_repeats.csv + aggregate.json
if command -v python3 &>/dev/null; then
  python3 "$SCRIPT_DIR/scripts/aggregate_repeats.py" "$OUTPUT_PARENT" "$N" 2>/dev/null || true
fi

echo "Done. Results in $OUTPUT_PARENT (summary_repeats.csv, aggregate.json)"
