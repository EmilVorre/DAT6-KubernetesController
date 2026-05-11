#!/usr/bin/env bash
# Run one scenario × strat: deploy (idempotent), generate load, trigger
# shutdown event, collect k6 + k8s events, summarize.
#
# Usage:
#   bash scripts/run_scenario.sh SCENARIO STRAT OUTPUT_DIR
#
# Profile / load knobs (env, override on command line):
#   EXP_PROFILE   thesis-stress (default) | standard
#   K6_RPS        override RPS (defaults from profile)
#   K6_VUS        override VUs
#   K6_DURATION   override duration
#
# Cluster knobs:
#   LOAD_HOST     ssh host running k6 (default: root@178.105.96.143)
#   SVC_URL       cluster URL the load hits (default: http://10.43.3.26:80)

set -euo pipefail

SCENARIO="${1:-rollout}"
STRAT="${2:-baseline}"
OUTPUT_DIR="${3:-runs/$(date +%Y%m%d-%H%M%S)-${SCENARIO}-${STRAT}}"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# --- load generator ---
LOAD_HOST="${LOAD_HOST:-root@178.105.96.143}"
SVC_URL="${SVC_URL:-http://10.43.3.26:80}"

# --- profile-driven k6 settings ---
# IMPORTANT: thesis-stress must actually be high RPS. The previous version
# of this script set RPS=50 in BOTH profiles (only VUS/duration differed),
# which silently capped load at the standard profile's rate regardless of
# the EXP_PROFILE selection. Don't undo this.
EXP_PROFILE="${EXP_PROFILE:-thesis-stress}"
case "${EXP_PROFILE}" in
  thesis-stress)
    K6_RPS="${K6_RPS:-250}"
    K6_VUS="${K6_VUS:-250}"
    K6_DURATION="${K6_DURATION:-150s}"
    ;;
  standard|*)
    K6_RPS="${K6_RPS:-50}"
    K6_VUS="${K6_VUS:-50}"
    K6_DURATION="${K6_DURATION:-90s}"
    ;;
esac
K6_SCENARIO="${K6_SCENARIO:-steady}"

mkdir -p "${OUTPUT_DIR}"
echo "    → output: ${OUTPUT_DIR}"
echo "    → profile=${EXP_PROFILE} rps=${K6_RPS} vus=${K6_VUS} duration=${K6_DURATION}"

# Save the run config alongside results
cat > "${OUTPUT_DIR}/scenario_config.json" <<EOF
{
  "scenario": "${SCENARIO}",
  "strat": "${STRAT}",
  "profile": "${EXP_PROFILE}",
  "k6_rps": ${K6_RPS},
  "k6_vus": ${K6_VUS},
  "k6_duration": "${K6_DURATION}"
}
EOF

# --- ensure deployment is in expected state ---
# Idempotent — when called by run_all_auto.sh, the deployment is already
# applied, so this is a fast no-op. For standalone use, this brings up
# the workload.
kubectl apply -k "${ROOT_DIR}/k8s/app/overlays/${STRAT}" >/dev/null
kubectl rollout status deployment/drainable-service --timeout=120s 2>/dev/null || true

# steady_scale_down resets to 6 between repeats. Without this, the previous
# repeat left replicas=2 and the next "scale to 2" is a silent no-op.
if [[ "${SCENARIO}" == "steady_scale_down" ]]; then
  echo "    → resetting replicas=6"
  kubectl scale deployment drainable-service --replicas=6 >/dev/null
  kubectl rollout status deployment/drainable-service --timeout=120s 2>/dev/null || true
fi

sleep 3

# Reachability check
if ! ssh -o ConnectTimeout=5 "${LOAD_HOST}" "curl -sf ${SVC_URL}/healthz" >/dev/null; then
  echo "ERROR: drainable-service not reachable at ${SVC_URL}" >&2
  exit 1
fi

# --- start event watch ---
EV_LOG_WATCH="${OUTPUT_DIR}/k8s_events_watch.log"
EV_LOG_SNAPSHOT="${OUTPUT_DIR}/k8s_events_snapshot.log"
kubectl get events --watch-only > "${EV_LOG_WATCH}" 2>/dev/null &
EV_PID=$!

# --- start pod state watch ---
# Streams a JSON-line per pod state change (ADDED/MODIFIED/DELETED) for every
# drainable-service pod. This is what gives us absolute-timestamped per-pod
# lifecycle data: deletionTimestamp, terminationGracePeriodSeconds, and
# containerStatuses[*].state.terminated.{startedAt,finishedAt}. The default
# `kubectl get events` text output only has relative timestamps ("0s",
# "1s") which is useless for measuring shutdown duration; the watch below
# is the source of truth for `extract_shutdown_durations.py`.
POD_WATCH_LOG="${OUTPUT_DIR}/pod_states_watch.jsonl"
kubectl get pods -l app=drainable-service \
  --output-watch-events --watch -o json \
  > "${POD_WATCH_LOG}" 2>/dev/null &
POD_WATCH_PID=$!

# Cleanup trap covers Ctrl-C / errors. Kills local event watchers and
# remote k6 (if it started).
cleanup_local() {
  kill "${EV_PID}" 2>/dev/null || true
  kill "${POD_WATCH_PID}" 2>/dev/null || true
  ssh -o ConnectTimeout=5 "${LOAD_HOST}" \
    'pkill -9 k6 2>/dev/null || true' 2>/dev/null || true
}
trap cleanup_local EXIT INT TERM

# --- start k6 on the load host ---
echo "    → starting k6 on ${LOAD_HOST}"
scp -q "${ROOT_DIR}/scripts/k6/load.js" "${LOAD_HOST}:/tmp/load.js"

ssh -t "${LOAD_HOST}" "k6 run \
  --out json=/tmp/k6_results.json \
  --env TARGET_URL=${SVC_URL} \
  --env SCENARIO=${K6_SCENARIO} \
  --env DURATION=${K6_DURATION} \
  --env VUS=${K6_VUS} \
  --env RPS=${K6_RPS} \
  /tmp/load.js" &
K6_PID=$!

# Let load stabilize before triggering the shutdown event
sleep 15

# --- trigger shutdown event ---
echo "    → triggering ${SCENARIO}"
case "${SCENARIO}" in
  steady_scale_down)
    kubectl scale deployment drainable-service --replicas=2 >/dev/null
    ;;
  rollout)
    kubectl rollout restart deployment drainable-service >/dev/null
    ;;
  delete_pod)
    POD=$(kubectl get pods -l app=drainable-service \
      -o jsonpath='{.items[0].metadata.name}')
    kubectl delete pod "${POD}" >/dev/null
    ;;
  *)
    echo "ERROR: unknown scenario: ${SCENARIO}" >&2
    exit 1
    ;;
esac

# --- wait for k6, copy results ---
wait "${K6_PID}" 2>/dev/null || true
scp -q "${LOAD_HOST}:/tmp/k6_results.json" "${OUTPUT_DIR}/k6_results.json"

# --- collect events ---
kill "${EV_PID}" 2>/dev/null || true
wait "${EV_PID}" 2>/dev/null || true
kill "${POD_WATCH_PID}" 2>/dev/null || true
wait "${POD_WATCH_PID}" 2>/dev/null || true
trap - EXIT INT TERM

kubectl get events --sort-by='.lastTimestamp' > "${EV_LOG_SNAPSHOT}" 2>/dev/null || true
{
  echo "=== watch_only ==="
  cat "${EV_LOG_WATCH}" 2>/dev/null || true
  echo ""
  echo "=== snapshot_sorted ==="
  cat "${EV_LOG_SNAPSHOT}" 2>/dev/null || true
} > "${OUTPUT_DIR}/k8s_events.log"

# --- summarize ---
python3 "${ROOT_DIR}/scripts/collect_metrics.py" "${OUTPUT_DIR}" 2>/dev/null || true
python3 "${ROOT_DIR}/scripts/extract_shutdown_durations.py" "${OUTPUT_DIR}" 2>/dev/null || true
python3 "${ROOT_DIR}/scripts/summarize_run.py" "${OUTPUT_DIR}"

echo "    → done"