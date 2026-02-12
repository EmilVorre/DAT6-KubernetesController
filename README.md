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

## Run

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
