#!/usr/bin/env bash
# Scenario runner: deploy workload, run load, trigger shutdown, collect results.
#
# Usage: ./run_scenario.sh SCENARIO STRAT OUTPUT_DIR
#   SCENARIO: steady_scale_down | rollout | delete_pod
#   STRAT: baseline | s1-early-readiness | s2-drain-verification | long-requests | burst | baseline-prestop-bad
#   OUTPUT_DIR: e.g. runs/20240216-120000-steady_scale_down-baseline
#
# Requires: kubectl, k6, kind cluster with drainable-service image loaded

set -euo pipefail

SCENARIO="${1:-steady_scale_down}"
STRAT="${2:-baseline}"
OUTPUT_DIR="${3:-runs/$(date +%Y%m%d-%H%M%S)-${SCENARIO}-${STRAT}}"
CLUSTER_NAME="${CLUSTER_NAME:-dat6-testbed}"
SVC_URL="${SVC_URL:-http://127.0.0.1:30080}"
EXP_PROFILE="${EXP_PROFILE:-standard}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

mkdir -p "$OUTPUT_DIR"
echo "Output: $OUTPUT_DIR"
echo "Scenario: $SCENARIO | Strategy: $STRAT | Profile: $EXP_PROFILE"

case "$EXP_PROFILE" in
  thesis-stress)
    K6_DURATION="${K6_DURATION:-120s}"
    K6_VUS="${K6_VUS:-20}"
    K6_RPS="${K6_RPS:-250}"
    K6_SCENARIO="${K6_SCENARIO:-steady}"
    ;;
  standard|*)
    K6_DURATION="${K6_DURATION:-90s}"
    K6_VUS="${K6_VUS:-10}"
    K6_RPS="${K6_RPS:-50}"
    K6_SCENARIO="${K6_SCENARIO:-steady}"
    ;;
esac

# Save scenario config for reproducibility
echo "{\"scenario\":\"$SCENARIO\",\"strat\":\"$STRAT\"}" > "$OUTPUT_DIR/scenario_config.json"

# Deploy overlay if not already applied
echo "Ensuring overlay $STRAT is applied..."
kubectl apply -k "$SCRIPT_DIR/k8s/overlays/$STRAT" --wait=true 2>/dev/null || true
kubectl rollout status deployment/drainable-service -n default --timeout=120s 2>/dev/null || true

# For steady_scale_down, ensure we start with 3 replicas so "scale to 2" is a real scale-down.
# (After run_1 the cluster is at 2; kubectl apply does not reset replicas due to last-applied-configuration.)
if [[ "$SCENARIO" == "steady_scale_down" ]]; then
  echo "Resetting replicas to 3 for scale-down repeat..."
  kubectl scale deployment drainable-service -n default --replicas=3
  kubectl rollout status deployment/drainable-service -n default --timeout=120s 2>/dev/null || true
fi

# Wait briefly for service routing
sleep 3
if ! curl -sf "$SVC_URL/healthz" >/dev/null; then
  echo "ERROR: drainable-service not reachable at $SVC_URL"
  echo "Hint: run 'make cluster-reset' after kind config changes so port 30080 is mapped."
  exit 1
fi

# Start event watch before triggering scenario so we capture rollout/termination sequence.
kubectl get events -n default --watch-only > "$OUTPUT_DIR/k8s_events_watch.log" 2>/dev/null &
EV_PID=$!
trap "kill $EV_PID 2>/dev/null || true" EXIT

# 1. Start load in background
echo "Starting load (k6)..."
k6 run \
  --out json="$OUTPUT_DIR/k6_results.json" \
  --env TARGET_URL="$SVC_URL" \
  --env SCENARIO="$K6_SCENARIO" \
  --env DURATION="$K6_DURATION" \
  --env VUS="$K6_VUS" \
  --env RPS="$K6_RPS" \
  "$SCRIPT_DIR/scripts/k6/load.js" &
K6_PID=$!

# Let load stabilize
sleep 15

# 2. Trigger shutdown event
echo "Triggering shutdown: $SCENARIO"
case "$SCENARIO" in
  steady_scale_down)
    kubectl scale deployment drainable-service -n default --replicas=2
    ;;
  rollout)
    kubectl rollout restart deployment drainable-service -n default
    ;;
  delete_pod)
    POD=$(kubectl get pods -n default -l app=drainable-service -o jsonpath='{.items[0].metadata.name}')
    kubectl delete pod "$POD" -n default
    ;;
  *)
    echo "Unknown scenario: $SCENARIO"
    exit 1
    ;;
esac

# 3. Wait for k6 to finish
wait $K6_PID 2>/dev/null || true

# 4. Stop event watch and capture a sorted snapshot too.
kill $EV_PID 2>/dev/null || true
wait $EV_PID 2>/dev/null || true
kubectl get events -n default --sort-by='.lastTimestamp' > "$OUTPUT_DIR/k8s_events_snapshot.log" 2>/dev/null || true
{
  echo "=== watch_only ==="
  cat "$OUTPUT_DIR/k8s_events_watch.log" 2>/dev/null || true
  echo ""
  echo "=== snapshot_sorted ==="
  cat "$OUTPUT_DIR/k8s_events_snapshot.log" 2>/dev/null || true
} > "$OUTPUT_DIR/k8s_events.log"

# 5. Run collection scripts
if command -v python3 &>/dev/null; then
  python3 "$SCRIPT_DIR/scripts/collect_metrics.py" "$OUTPUT_DIR" 2>/dev/null || true
  python3 "$SCRIPT_DIR/scripts/summarize_run.py" "$OUTPUT_DIR" 2>/dev/null || true
fi

echo "Done. Results in $OUTPUT_DIR"
