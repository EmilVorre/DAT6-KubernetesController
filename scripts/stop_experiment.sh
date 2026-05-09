#!/usr/bin/env bash
# Cleanly stop any in-progress DAT6 experiment.
#
# Use this when:
# - Ctrl-C didn't work and the script is hanging
# - You suspect there are duplicate run_all_auto.sh processes (the bug
#   that previously caused env-var races against the cluster)
# - Something feels wrong and you just want a clean slate
#
# This handles both the legacy local-cargo controller (in case anything
# is left behind from an old branch) and the current in-cluster
# controller deployment + its `kubectl logs -f` log-tail processes.

set -euo pipefail

LOAD_HOST="${LOAD_HOST:-root@178.105.96.143}"
LOCK_FILE="/tmp/dat6-experiment.lock"

echo "Stopping experiment processes..."

# Local processes (script wrappers, kubectl watches/log-tails).
pkill -9 -f run_all_auto.sh                            2>/dev/null || true
pkill -9 -f run_repeats.sh                             2>/dev/null || true
pkill -9 -f run_scenario.sh                            2>/dev/null || true
pkill -9 -f 'kubectl get events'                       2>/dev/null || true
pkill -9 -f 'kubectl logs -f deployment/dat6-controller' 2>/dev/null || true
# Legacy local-cargo controller (only present on old branches; harmless
# if no such process exists).
pkill -9 -f 'cargo run'                                2>/dev/null || true
pkill -9 -f 'target/debug/DAT6_KubernetesController'   2>/dev/null || true
pkill -9 -f 'target/release/DAT6_KubernetesController' 2>/dev/null || true

# In-cluster controller. Leave SA + RBAC in place — they're idempotent
# and the next `make deploy-controller` / `run_all_auto.sh` will reuse
# them. We only want the running pod gone so the next iteration starts
# from a known state.
if command -v kubectl >/dev/null 2>&1; then
  if kubectl get deployment dat6-controller -n default >/dev/null 2>&1; then
    echo "Deleting dat6-controller deployment..."
    kubectl delete deployment dat6-controller -n default --ignore-not-found \
      --wait=false 2>/dev/null || true
  fi
fi

# Remote k6
echo "Killing k6 on ${LOAD_HOST}..."
ssh -o ConnectTimeout=5 "${LOAD_HOST}" 'pkill -9 k6 2>/dev/null || true' \
  2>/dev/null || true

# Release lock if held
rm -f "${LOCK_FILE}" 2>/dev/null || true

# Report what's left
sleep 1
remaining="$(ps aux \
  | grep -E 'run_all_auto|run_repeats|run_scenario|cargo run|target/(debug|release)/DAT6|kubectl logs -f deployment/dat6-controller' \
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
