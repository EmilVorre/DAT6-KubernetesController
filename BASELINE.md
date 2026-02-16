# Baseline + Testbed

Reproducible testbed for safe container decomposition research.

## Quick Start

```bash
make cluster-up
make deploy-prometheus
make deploy-baseline
make run SCENARIO=steady_scale_down STRAT=baseline
```

## Structure

| Path | Role |
|------|------|
| `kind/cluster.yaml` | kind cluster config (pinned k8s v1.31.14) |
| `app/` | Drainable HTTP service (Rust + axum) |
| `k8s/base/` | Deployment + Service (Kustomize base) |
| `k8s/overlays/` | baseline, long-requests, burst, baseline-prestop-bad |
| `observability/prometheus/` | Helm values for kube-prometheus-stack |
| `scripts/` | run_scenario.sh, k6 load.js, collect_metrics.py, summarize_run.py |

## Drainable Service

- **GET /** — 200, configurable latency (`SLEEP_MS`), fault injection (`LONG_REQUESTS_PCT`)
- **GET /healthz** — always OK while running
- **GET /readyz** — fails when draining (SIGTERM received)
- **GET /metrics** — Prometheus metrics (in-flight, total requests, errors)
- **SIGTERM** — stop accepting, finish in-flight, exit

## Overlays

| Overlay | Purpose |
|---------|---------|
| `baseline` | terminationGracePeriodSeconds=30, no preStop |
| `baseline-prestop-bad` | preStop: sleep 10 (shows why naive preStop is bad) |
| `long-requests` | LONG_REQUESTS_PCT=5 (5% requests sleep 2–10s) |
| `burst` | BURST_MODE=1 (load generator drives RPS spikes) |

## Scenarios

| Scenario | Trigger |
|----------|---------|
| `steady_scale_down` | `kubectl scale --replicas=2` |
| `rollout` | `kubectl rollout restart` |
| `delete_pod` | `kubectl delete pod <name>` |

## Run Output

Each run produces:

- `client_results.json` — loss %, p50/p95/p99
- `summary.csv` — append row per run
- `k8s_events.log` — cluster events
- `k6_results.json` — raw k6 output
- `shutdown_timeline.json` — parsed timeline

## Prerequisites

- kind, kubectl, helm, docker
- k6 (`go install go.k6.io/k6@latest` or download)
- Python 3 (for collect/summarize scripts)
- bash (WSL or Git Bash on Windows)
