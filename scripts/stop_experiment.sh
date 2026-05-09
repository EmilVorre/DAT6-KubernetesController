#!/usr/bin/env bash
# Cleanly stop any in-progress DAT6 experiment.
#
# Use this when:
# - Ctrl-C didn't work and the script is hanging
# - You suspect there are duplicate run_all_auto.sh processes (the bug
#   that previously caused env-var races against the cluster)
# - Something feels wrong and you just want a clean slate

set -euo pipefail

LOAD_HOST="${LOAD_HOST:-root@178.105.96.143}"
LOCK_FILE="/tmp/dat6-experiment.lock"

echo "Stopping experiment processes..."

# Local processes (script wrappers, controller, kubectl watches)
pkill -9 -f run_all_auto.sh                            2>/dev/null || true
pkill -9 -f run_repeats.sh                             2>/dev/null || true
pkill -9 -f run_scenario.sh                            2>/dev/null || true
pkill -9 -f 'cargo run'                                2>/dev/null || true
pkill -9 -f 'target/debug/DAT6_KubernetesController'   2>/dev/null || true
pkill -9 -f 'kubectl get events'                       2>/dev/null || true

# Remote k6
echo "Killing k6 on ${LOAD_HOST}..."
ssh -o ConnectTimeout=5 "${LOAD_HOST}" 'pkill -9 k6 2>/dev/null || true' \
  2>/dev/null || true

# Release lock if held
rm -f "${LOCK_FILE}" 2>/dev/null || true

# Report what's left
sleep 1
remaining="$(ps aux \
  | grep -E 'run_all_auto|run_repeats|run_scenario|cargo run|target/debug/DAT6' \
  | grep -v grep || true)"
if [[ -n "${remaining}" ]]; then
  echo ""
  echo "WARNING: some processes still running:"
  echo "${remaining}"
  echo ""
  echo "Try again or kill them manually."
  exit 1
fi
echo "All experiment processes stopped."
