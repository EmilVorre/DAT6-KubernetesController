#!/usr/bin/env bash
# Run the full experiment matrix: strats × scenarios, N repeats each.
#
# Strategies:  baseline, s1-early-readiness, s2-drain-verification
# Scenarios:   rollout, steady_scale_down
#
# Usage:
#   bash scripts/run_all_auto.sh [N]          # default N=5
#
# Controller lifecycle (in-cluster, kubectl-driven)
# -------------------------------------------------
# This script no longer runs `cargo run` locally. The controller is deployed
# as an in-cluster Deployment (k8s/controller/) so that /drainez polls (S2)
# can actually reach pod IPs through flannel — pod IPs (10.42.x.x) aren't
# routable from the dev box, which silently turned every S2 run into "S1 +
# pointless waiting" via the unreachable fallback.
#
# Per strategy we:
#   1. delete drainable-service                 (clean kustomize merge state)
#   2. delete dat6-controller                   (clean controller state)
#   3. kubectl apply -k k8s/controller/overlays/<strat>
#   4. kubectl rollout status deployment/dat6-controller --timeout=60s
#   5. kubectl logs -f deployment/dat6-controller > controller-<strat>.log &
#      (CTRL_LOG_PID tracks the background tail)
#   6. apply drainable-service overlay; wait for rollout
#   7. verify pod env, run repeats, kill log tail
#
# Bug-prevention features:
# - Single-instance lock (flock). A second invocation aborts immediately
#   instead of racing the cluster.
# - Cleanup trap deletes the controller deployment AND kills the log-tail
#   AND remote k6 on any exit path (success, failure, Ctrl-C).
# - Deletes the deployment between EVERY strat, not just baseline. This
#   prevents kustomize strategic-merge from leaving env vars behind when
#   an overlay's patch doesn't mention them (e.g. baseline → s1 leaks
#   nothing; s1 → baseline leaks DAT6_GRACEFUL_DRAIN=1, which is what
#   silently broke previous runs).
# - Verifies the deployed pod's env after each apply. Halts the matrix
#   if the env doesn't match what the strat is supposed to produce.
#
# Pre-requisites:
# - The dat6-controller image (ghcr.io/emilvorre/dat6-controller:latest) has
#   been built+pushed via `make push-controller`.
# - SA + RBAC are installed (the first `kubectl apply -k` of any controller
#   overlay does this for you; subsequent applies are no-ops on those
#   objects since we don't delete them between iterations).
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
CTRL_LOG_PID=""    # `kubectl logs -f` background pid for the current strat

cleanup() {
  local exit_code=$?
  trap - EXIT INT TERM
  echo ""
  echo "--- Cleanup ---"
  # Stop log tail (if any) before deleting the deployment, otherwise the
  # tail prints a noisy "container terminated" line as the controller
  # disappears under it.
  if [[ -n "${CTRL_LOG_PID}" ]] && kill -0 "${CTRL_LOG_PID}" 2>/dev/null; then
    echo "  Stopping controller log tail (pid=${CTRL_LOG_PID})"
    kill -TERM "${CTRL_LOG_PID}" 2>/dev/null || true
    wait "${CTRL_LOG_PID}" 2>/dev/null || true
  fi
  # Tear the controller down so the next manual investigation starts from
  # a known-clean state. SA + RBAC are intentionally left in place.
  if kubectl get deployment dat6-controller -n default >/dev/null 2>&1; then
    echo "  Deleting dat6-controller deployment"
    kubectl delete deployment dat6-controller -n default --ignore-not-found \
      --wait=false 2>/dev/null || true
  fi
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
# Apply the controller overlay for `strat`, wait for rollout, then start a
# background `kubectl logs -f` tail into ${COMPARE_DIR}/controller-<strat>.log.
# CTRL_LOG_PID is set to the tail's pid so stop_controller / cleanup can
# kill exactly that one process (no setsid / process-group dance required —
# the controller itself runs as a Deployment, which kubectl delete handles).
start_controller() {
  local strat="$1"
  case "${strat}" in
    baseline|s1-early-readiness|s2-drain-verification) ;;
    *)
      echo "ERROR: unknown strategy: ${strat}" >&2
      return 1 ;;
  esac

  local overlay="${ROOT_DIR}/k8s/controller/overlays/${strat}"
  local log="${COMPARE_DIR}/controller-${strat}.log"

  echo "  → Applying controller overlay: ${overlay}"
  kubectl apply -k "${overlay}" >/dev/null

  echo "  → Waiting for controller rollout"
  if ! kubectl rollout status deployment/dat6-controller -n default --timeout=60s; then
    echo "ERROR: controller rollout failed; recent events:" >&2
    kubectl get events -n default --sort-by='.lastTimestamp' | tail -20 >&2 || true
    kubectl describe deployment dat6-controller -n default >&2 || true
    return 1
  fi

  # Verify the controller pod is actually Ready (not just "rollout reports
  # progress complete"). On image-pull errors `rollout status` can return 0
  # while the pod is CrashLoopBackOff.
  if ! kubectl wait --for=condition=Available deployment/dat6-controller \
       -n default --timeout=30s >/dev/null 2>&1; then
    echo "ERROR: dat6-controller did not become Available. Tail of describe:" >&2
    kubectl describe deployment dat6-controller -n default | tail -40 >&2 || true
    return 1
  fi

  echo "  → Starting controller log tail (log: ${log})"
  # `kubectl logs -f deployment/...` follows the current pod's logs and
  # re-attaches if the pod restarts. nohup keeps it alive if our shell
  # gets SIGHUP'd; redirect stdin from /dev/null so it doesn't grab the
  # tty.
  nohup kubectl logs -f deployment/dat6-controller -n default \
    >"${log}" 2>&1 </dev/null &
  CTRL_LOG_PID=$!

  # Show the env that will be in effect for this strat, for the per-run log.
  local env_seen
  env_seen="$(kubectl exec -n default deploy/dat6-controller -- env 2>/dev/null \
    | grep -E '^DAT6_' | sort | tr '\n' ' ' || true)"
  echo "  → Controller running (log_tail_pid=${CTRL_LOG_PID}); env: ${env_seen:-<none>}"
}

stop_controller() {
  if [[ -n "${CTRL_LOG_PID}" ]] && kill -0 "${CTRL_LOG_PID}" 2>/dev/null; then
    echo "  → Stopping controller log tail (pid=${CTRL_LOG_PID})"
    kill -TERM "${CTRL_LOG_PID}" 2>/dev/null || true
    wait "${CTRL_LOG_PID}" 2>/dev/null || true
  fi
  CTRL_LOG_PID=""

  if kubectl get deployment dat6-controller -n default >/dev/null 2>&1; then
    echo "  → Deleting dat6-controller deployment"
    kubectl delete deployment dat6-controller -n default --ignore-not-found
    kubectl wait --for=delete deployment/dat6-controller -n default \
      --timeout=60s 2>/dev/null || true
  fi
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

# Sanity-check that the controller image is reachable in the registry. If
# `make push-controller` was forgotten this is the friendliest place to
# fail — early, with a clear message — rather than 60s into the first
# rollout when the kubelet finally surfaces ImagePullBackOff.
echo ""
echo "Pre-flight: verifying controller manifests parse..."
for strat in "${STRATS[@]}"; do
  if ! kubectl kustomize "k8s/controller/overlays/${strat}" >/dev/null; then
    echo "ERROR: kustomize failed for k8s/controller/overlays/${strat}" >&2
    exit 1
  fi
done
echo "  → OK."

for strat in "${STRATS[@]}"; do
  for scenario in "${SCENARIOS[@]}"; do
    echo ""
    echo "================================================================"
    echo "  ${strat} / ${scenario}"
    echo "================================================================"

    # Always delete the app deployment before the next strat so kustomize
    # strategic-merge starts from a clean spec.
    echo "  → Deleting any existing app deployment"
    kubectl delete deployment drainable-service --ignore-not-found
    kubectl wait --for=delete deployment/drainable-service \
      --timeout=60s 2>/dev/null || true

    # Controller goes up FIRST for all strategies. For S1/S2 this is
    # required (the controller must flip the readiness gate to True before
    # pods can become Ready, and must add the S2 finalizer before pods
    # start terminating); for baseline it costs nothing because the
    # reconciler is a no-op without DAT6_EARLY_READINESS_REMOVAL.
    start_controller "${strat}"

    echo "  → Applying app overlay: k8s/app/overlays/${strat}"
    kubectl apply -k "k8s/app/overlays/${strat}"
    kubectl rollout status deployment/drainable-service --timeout=120s

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
