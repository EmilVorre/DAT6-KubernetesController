#!/usr/bin/env bash
# Run the full experiment matrix: strats × scenarios, N repeats each.
#
# Strategies:  baseline, s1-early-readiness, s2-drain-verification
# Scenarios:   rollout, steady_scale_down
#
# Usage:
#   bash scripts/run_all_auto.sh [N]          # default N=5
#
# Bug-prevention features:
# - Single-instance lock (flock). A second invocation aborts immediately
#   instead of racing the cluster.
# - Cleanup trap kills the controller AND remote k6 on any exit path
#   (success, failure, Ctrl-C).
# - Deletes the deployment between EVERY strat, not just baseline. This
#   prevents kustomize strategic-merge from leaving env vars behind when
#   an overlay's patch doesn't mention them (e.g. baseline → s1 leaks
#   nothing; s1 → baseline leaks DAT6_GRACEFUL_DRAIN=1, which is what
#   silently broke previous runs).
# - Verifies the deployed pod's env after each apply. Halts the matrix
#   if the env doesn't match what the strat is supposed to produce.
#
# Environment overrides (all optional):
#   EXP_PROFILE   thesis-stress (default) | standard
#   K6_RPS        override RPS (defaults from profile)
#   K6_VUS        override VUs
#   K6_DURATION   override duration
#   LOAD_HOST     ssh host running k6 (default: root@178.105.96.143)
#   SVC_URL       cluster-side URL the load hits (default: http://10.43.3.26:80)

set -euo pipefail

# ---------------------------------------------------------------------------
# 1. Single-instance lock
# ---------------------------------------------------------------------------
LOCK_FILE="/tmp/dat6-experiment.lock"
exec 200>"${LOCK_FILE}"
if ! flock -n 200; then
  echo "ERROR: another experiment is already running (lock: ${LOCK_FILE})" >&2
  echo "       Stop it with 'make stop' or wait for it to finish." >&2
  exit 1
fi
echo "Acquired lock: ${LOCK_FILE} (pid=$$)"

# ---------------------------------------------------------------------------
# 2. Config
# ---------------------------------------------------------------------------
N="${1:-${N:-5}}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="${RUN_DIR:-runs}"
STAMP="$(date +%Y%m%d-%H%M%S)"
COMPARE_DIR="${RUN_DIR}/compare-${STAMP}"
STRATS=("baseline" "s1-early-readiness" "s2-drain-verification")
SCENARIOS=("rollout" "steady_scale_down")

# Make profile + k6 overrides visible to child scripts
export EXP_PROFILE="${EXP_PROFILE:-thesis-stress}"
export K6_RPS K6_VUS K6_DURATION LOAD_HOST SVC_URL

mkdir -p "${COMPARE_DIR}"
cd "${ROOT_DIR}"

# ---------------------------------------------------------------------------
# 3. Cleanup trap (covers Ctrl-C, errors, normal exit)
# ---------------------------------------------------------------------------
CTRL_PID=""

cleanup() {
  local exit_code=$?
  trap - EXIT INT TERM
  echo ""
  echo "--- Cleanup ---"
  if [[ -n "${CTRL_PID}" ]] && kill -0 "${CTRL_PID}" 2>/dev/null; then
    echo "  Stopping controller (pgid=${CTRL_PID})"
    kill -TERM -"${CTRL_PID}" 2>/dev/null || true
    sleep 2
    kill -KILL -"${CTRL_PID}" 2>/dev/null || true
    wait "${CTRL_PID}" 2>/dev/null || true
  fi
  # Belt-and-suspenders: any lingering controller binary
  pkill -9 -f 'target/debug/DAT6_KubernetesController' 2>/dev/null || true
  # Kill any in-flight k6 on the load host
  ssh -o ConnectTimeout=5 "${LOAD_HOST:-root@178.105.96.143}" \
    'pkill -9 k6 2>/dev/null || true' 2>/dev/null || true
  flock -u 200 2>/dev/null || true
  rm -f "${LOCK_FILE}" 2>/dev/null || true
  if [[ ${exit_code} -ne 0 ]]; then
    echo "Experiment exited with status ${exit_code}"
  fi
  exit ${exit_code}
}
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# 4. Helpers
# ---------------------------------------------------------------------------
start_controller() {
  local strat="$1"
  local -a env_vars=()
  case "${strat}" in
    baseline) ;;
    s1-early-readiness)
      env_vars=("DAT6_EARLY_READINESS_REMOVAL=1") ;;
    s2-drain-verification)
      env_vars=("DAT6_EARLY_READINESS_REMOVAL=1" "DAT6_DRAIN_VERIFICATION=1") ;;
    *)
      echo "ERROR: unknown strategy: ${strat}" >&2
      return 1 ;;
  esac

  local log="${COMPARE_DIR}/controller-${strat}.log"
  echo "  → Starting controller (log: ${log})"

  # setsid puts cargo + the spawned binary in their own process group, so
  # `kill -<pgid>` cleanly takes both down. Without this, killing cargo
  # leaves the compiled binary running.
  if [[ ${#env_vars[@]} -eq 0 ]]; then
    setsid bash -c "cd '${ROOT_DIR}' && cargo run --quiet" >"${log}" 2>&1 &
  else
    setsid bash -c "cd '${ROOT_DIR}' && env ${env_vars[*]} cargo run --quiet" >"${log}" 2>&1 &
  fi
  CTRL_PID=$!

  # Give cargo time to compile (first run) and the controller time to
  # connect to the apiserver before we start applying manifests.
  sleep 10
  if ! kill -0 "${CTRL_PID}" 2>/dev/null; then
    echo "ERROR: controller failed to start. Tail of ${log}:" >&2
    tail -20 "${log}" >&2 || true
    return 1
  fi
  echo "  → Controller running (pgid=${CTRL_PID})"
}

stop_controller() {
  if [[ -n "${CTRL_PID}" ]] && kill -0 "${CTRL_PID}" 2>/dev/null; then
    echo "  → Stopping controller (pgid=${CTRL_PID})"
    kill -TERM -"${CTRL_PID}" 2>/dev/null || true
    sleep 2
    kill -KILL -"${CTRL_PID}" 2>/dev/null || true
    wait "${CTRL_PID}" 2>/dev/null || true
  fi
  pkill -9 -f 'target/debug/DAT6_KubernetesController' 2>/dev/null || true
  CTRL_PID=""
}

verify_env() {
  local strat="$1"
  # Wait for at least one pod to be Ready before exec'ing
  kubectl wait --for=condition=Ready pod -l app=drainable-service \
    --timeout=60s 2>/dev/null || true

  local got
  got="$(kubectl exec deploy/drainable-service -- env 2>/dev/null \
    | grep -E '^DAT6_GRACEFUL_DRAIN=' || echo 'DAT6_GRACEFUL_DRAIN=<unset>')"

  case "${strat}" in
    baseline)
      if [[ "${got}" == "DAT6_GRACEFUL_DRAIN=1" ]]; then
        echo "ERROR: baseline pod has DAT6_GRACEFUL_DRAIN=1 (env-var leak)" >&2
        return 1
      fi
      ;;
    s1-early-readiness|s2-drain-verification)
      if [[ "${got}" != "DAT6_GRACEFUL_DRAIN=1" ]]; then
        echo "ERROR: ${strat} pod has '${got}', expected DAT6_GRACEFUL_DRAIN=1" >&2
        return 1
      fi
      ;;
  esac
  echo "  → Verified pod env: ${got}"
}

# ---------------------------------------------------------------------------
# 5. Main loop
# ---------------------------------------------------------------------------
echo "================================================================"
echo "  DAT6 experiment matrix"
echo "  Profile:    ${EXP_PROFILE}"
echo "  Repeats:    N=${N}"
echo "  Output:     ${COMPARE_DIR}"
echo "  Strategies: ${STRATS[*]}"
echo "  Scenarios:  ${SCENARIOS[*]}"
echo "================================================================"

# Pre-build the controller once so each combo's start_controller doesn't
# pay the compile cost (and so the 10s startup grace is realistic).
echo ""
echo "Pre-building controller..."
cargo build --quiet
echo "  → Done."

for strat in "${STRATS[@]}"; do
  for scenario in "${SCENARIOS[@]}"; do
    echo ""
    echo "================================================================"
    echo "  ${strat} / ${scenario}"
    echo "================================================================"

    # Always delete the deployment before the next strat so kustomize
    # strategic-merge starts from a clean spec.
    echo "  → Deleting any existing deployment"
    kubectl delete deployment drainable-service --ignore-not-found
    kubectl wait --for=delete deployment/drainable-service \
      --timeout=60s 2>/dev/null || true

    if [[ "${strat}" == "baseline" ]]; then
      # Baseline: controller has no effect, but run it for symmetry.
      kubectl apply -k "k8s/overlays/${strat}"
      kubectl rollout status deployment/drainable-service --timeout=120s
      start_controller "${strat}"
    else
      # S1/S2: controller MUST be running before pods become Ready so it
      # can flip the readiness gate to True. Otherwise rollout stalls at
      # 0/N available.
      start_controller "${strat}"
      kubectl apply -k "k8s/overlays/${strat}"
      kubectl rollout status deployment/drainable-service --timeout=120s
    fi

    verify_env "${strat}"

    # Run N repeats of this combo
    run_ts="$(date +%Y%m%d-%H%M%S)"
    TIMESTAMP="${run_ts}" RUN_DIR="${RUN_DIR}" \
      bash scripts/run_repeats.sh "${N}" "${scenario}" "${strat}"

    stop_controller

    # Copy results into the comparison directory for compare.py
    src_dir="${RUN_DIR}/${run_ts}-${scenario}-${strat}-repeats"
    dst_dir="${COMPARE_DIR}/${strat}-${scenario}"
    rm -rf "${dst_dir}"
    cp -a "${src_dir}" "${dst_dir}"
  done
done

echo ""
echo "================================================================"
echo "  Generating comparison"
echo "================================================================"
python3 scripts/compare.py "${COMPARE_DIR}"

echo ""
echo "Done."
echo "  Results:    ${COMPARE_DIR}"
echo "  Comparison: ${COMPARE_DIR}/comparison.md"
