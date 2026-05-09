#!/usr/bin/env bash
# Run a single (scenario, strat) combo N times and aggregate results.
#
# Usage:
#   bash scripts/run_repeats.sh N SCENARIO STRAT
#
# This script does NOT manage the controller or deployment — it assumes
# both are already in the right state. run_all_auto.sh handles that for
# matrix runs. For standalone use, deploy the overlay yourself first:
#
#   make deploy STRAT=s1-early-readiness
#   bash scripts/run_repeats.sh 5 rollout s1-early-readiness

set -euo pipefail

N="${1:-5}"
SCENARIO="${2:-rollout}"
STRAT="${3:-baseline}"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="${RUN_DIR:-runs}"
TIMESTAMP="${TIMESTAMP:-$(date +%Y%m%d-%H%M%S)}"
OUTPUT_PARENT="${RUN_DIR}/${TIMESTAMP}-${SCENARIO}-${STRAT}-repeats"

mkdir -p "${OUTPUT_PARENT}"

echo "  Running ${N} repeats of ${SCENARIO}/${STRAT}"
echo "  Output: ${OUTPUT_PARENT}"

for i in $(seq 1 "${N}"); do
  echo "  --- Repeat ${i}/${N} ---"
  RUN_PATH="${OUTPUT_PARENT}/run_${i}"
  bash "${ROOT_DIR}/scripts/run_scenario.sh" "${SCENARIO}" "${STRAT}" "${RUN_PATH}"
  python3 "${ROOT_DIR}/scripts/plot_timeline.py" "${RUN_PATH}" 2>/dev/null || true
done

python3 "${ROOT_DIR}/scripts/aggregate_repeats.py" "${OUTPUT_PARENT}" "${N}"

echo "  Aggregated: ${OUTPUT_PARENT}/summary_repeats.csv"
