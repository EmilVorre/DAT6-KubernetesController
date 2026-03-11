# DAT6-KubernetesController

Kubernetes controller for bachelor research on **safely decomposing containers** in Kubernetes (Rust, [kube-rs](https://kube.rs/)).

## Technical approach

- **Watches:** Pods, Deployments, StatefulSets (including termination and scale-down events).
- **Enforces:** graceful shutdown ordering, traffic draining before deletion, verification of readiness loss, optional state handover validation.
- **Acts via:** preStop hooks, Pod deletion delays, custom annotations or CRDs.

## Architecture

```
+----------------------+
| Kubernetes API       |
+----------+-----------+
           |
           v
+----------------------+
| Rust Controller      |
|  - Watcher           |  <- Pods / Deployments / StatefulSets
|  - Policy Engine      |  <- ordering, drain, readiness, state handover
|  - Decommission FSM  |  <- Draining → ReadinessLost → PreStop → DeletionAllowed
+----------+-----------+
           |
           v
+----------------------+
| Pods / Services      |
+----------------------+
```

## Layout

| Path | Role |
|------|------|
| `src/lib.rs` | Library root; exports controller, policy, decommission, error, optional health/metrics. |
| `src/controller.rs` | **Watcher:** reconcile loops for Pod, Deployment, StatefulSet; uses policy + FSM. |
| `src/policy.rs` | **Policy engine:** `DecommissionPolicy`, `PolicyEngine::evaluate`, `PolicyDecision`. |
| `src/decommission.rs` | **Decommission FSM:** states, events, `transition()`, annotation persistence. |
| `src/error.rs` | Errors (`thiserror`); extend with decomposition-specific variants. |
| `src/main.rs` | Entrypoint: starts all three watchers; optional health/metrics server. |
| `src/health.rs` | Optional: `/live`, `/ready` (enable with `health` feature). |
| `src/metrics.rs` | Optional: Prometheus counters/gauges (enable with `metrics` feature). |

## Tech stack

- **kube** — Kubernetes controller framework
- **tokio** — async runtime
- **serde** — CRD & config serialization
- **tracing** — structured logging
- **thiserror / anyhow** — error handling

Optional (features):

- **axum** — health endpoints (`health` feature)
- **prometheus** — metrics (`metrics` feature)

## Baseline + Testbed

For reproducible experiments, see **[BASELINE.md](BASELINE.md)**:

```bash
make cluster-up
make deploy-baseline
make run SCENARIO=steady_scale_down STRAT=baseline
```

## S1 — Early Readiness Removal

When a pod is terminating, the controller can set a custom readiness gate to **False** so the pod is removed from Service endpoints immediately (no new traffic), then in-flight requests can drain during the grace period.

1. Deploy the S1 overlay: `make deploy-baseline K8S_OVERLAY=s1-early-readiness`
2. Run the controller with S1 enabled: `DAT6_EARLY_READINESS_REMOVAL=1 cargo run`
3. Run scenarios: `make run SCENARIO=rollout STRAT=s1-early-readiness`

## N repeats and stable metrics

Run a scenario N times and get aggregated metrics (e.g. mean/min/max loss and latency):

```bash
make run-repeats N=5 SCENARIO=steady_scale_down STRAT=baseline
make run-repeats N=5 SCENARIO=rollout STRAT=s1-early-readiness
```

Output: `runs/<timestamp>-<scenario>-<strat>-repeats/` with `run_1/` … `run_N/`, `summary_repeats.csv`, and `aggregate.json`.

### Why run_2+ can show 0% loss (steady_scale_down)

For `steady_scale_down` we scale the deployment **to 2** during load. After run_1 the cluster is left at 2 replicas. On run_2 we run `kubectl apply -k` again; the applied manifest has `replicas: 3` (from the base), but **`kubectl apply` does a three-way merge using `last-applied-configuration`**. That annotation was set when we first applied (with replicas: 3); when we later ran `kubectl scale ... --replicas=2`, the annotation was **not** updated. So on the next apply, kubectl sees “desired = 3, last-applied = 3” and sends no patch — the live replicas stay at 2. Then we run “scale to 2” again, which is a **no-op**. So only run_1 actually performs a scale-down; run_2+ see no churn and report 0% loss.

To make every repeat perform a real scale-down, the scenario now **explicitly scales back to 3** at the start when using `steady_scale_down`, so each run starts with 3 replicas and then scales to 2 under load.

## Run (Controller)

Requires kubeconfig (e.g. `~/.kube/config`) or in-cluster config.

```bash
cargo run
```

With health and metrics:

```bash
cargo run --features "health,metrics"
```

Log level:

```bash
RUST_LOG=debug cargo run
```

## Where to implement research logic

1. **Policy engine** (`src/policy.rs`): Implement `PolicyEngine::evaluate` using pod state, endpoints (traffic drain), and FSM state; return `DelayDeletion`, `EnsurePreStop`, `AllowDeletion`, or `WaitForStateHandover`.
2. **Pod reconcile** (`src/controller.rs`): Drive FSM with `decommission::transition`, persist state via annotation `decomposition.dat6.io/state`; add/ensure preStop hooks; add finalizer and remove it only when `DeletionAllowed`.
3. **Deployment / StatefulSet** (`src/controller.rs`): On scale-down, enforce `GracefulShutdownOrdering` (e.g. by ordinal); coordinate with pod reconciler.
4. **CRD / annotations:** Load `DecommissionPolicy` from a custom resource or pod/deployment annotations; extend `policy.rs` and context as needed.
