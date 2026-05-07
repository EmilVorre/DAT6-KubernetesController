#!/usr/bin/env bash
# Full experiment matrix runner:
# - baseline, s1-early-readiness, s2-drain-verification
# - rollout, steady_scale_down
# - N repeats per combo
#
# Usage:
#   bash scripts/run_all_auto.sh [N]
#   N defaults to 5

set -euo pipefail

N="${1:-5}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="${RUN_DIR:-runs}"
STAMP="${TIMESTAMP:-$(date +%Y%m%d-%H%M%S)}"
COMPARE_DIR="${RUN_DIR}/compare-${STAMP}"
STRATS=("baseline" "s1-early-readiness" "s2-drain-verification")
SCENARIOS=("rollout" "steady_scale_down")

CTRL_PID=""
CTRL_LOG=""

cleanup_controller() {
  if [[ -n "${CTRL_PID}" ]] && kill -0 "${CTRL_PID}" 2>/dev/null; then
    kill "${CTRL_PID}" 2>/dev/null || true
    wait "${CTRL_PID}" 2>/dev/null || true
  fi
  CTRL_PID=""
}

trap cleanup_controller EXIT

start_controller() {
  local strat="$1"
  cleanup_controller

  local -a env_vars=()
  case "${strat}" in
    baseline)
      ;;
    s1-early-readiness)
      env_vars=("DAT6_EARLY_READINESS_REMOVAL=1")
      ;;
    s2-drain-verification)
      env_vars=("DAT6_EARLY_READINESS_REMOVAL=1" "DAT6_DRAIN_VERIFICATION=1")
      ;;
    *)
      echo "Unknown strategy: ${strat}" >&2
      exit 1
      ;;
  esac

  CTRL_LOG="${COMPARE_DIR}/controller-${strat}.log"
  mkdir -p "${COMPARE_DIR}"
  echo "Starting controller for ${strat}..."
  (
    cd "${ROOT_DIR}"
    if [[ "${#env_vars[@]}" -eq 0 ]]; then
      cargo run >"${CTRL_LOG}" 2>&1
    else
      env "${env_vars[@]}" cargo run >"${CTRL_LOG}" 2>&1
    fi
  ) &
  CTRL_PID=$!
  sleep 8
  if ! kill -0 "${CTRL_PID}" 2>/dev/null; then
    echo "Controller failed to start for ${strat}. See ${CTRL_LOG}" >&2
    exit 1
  fi
}

cd "${ROOT_DIR}"
mkdir -p "${COMPARE_DIR}"

echo "Running full matrix: N=${N}"
echo "Output: ${COMPARE_DIR}"

for strat in "${STRATS[@]}"; do
  for scenario in "${SCENARIOS[@]}"; do
    echo "=== ${strat} / ${scenario} ==="
    make cluster-reset
    make deploy-prometheus
    # For S1/S2 overlays, controller must already be running so it can set
    # readiness gate True on active pods; otherwise rollout can stall at 0/3 available.
    if [[ "${strat}" == "baseline" ]]; then
      make deploy-baseline K8S_OVERLAY="${strat}"
      start_controller "${strat}"
    else
      start_controller "${strat}"
      make deploy-baseline K8S_OVERLAY="${strat}"
    fi

    run_ts="$(date +%Y%m%d-%H%M%S)"
    TIMESTAMP="${run_ts}" RUN_DIR="${RUN_DIR}" make run-repeats N="${N}" SCENARIO="${scenario}" STRAT="${strat}"
    cleanup_controller

    src_dir="${RUN_DIR}/${run_ts}-${scenario}-${strat}-repeats"
    dst_dir="${COMPARE_DIR}/${strat}-${scenario}"
    rm -rf "${dst_dir}"
    cp -a "${src_dir}" "${dst_dir}"
  done
done

python3 scripts/compare.py "${COMPARE_DIR}"
echo "Done."
echo "Comparison markdown: ${COMPARE_DIR}/comparison.md"
