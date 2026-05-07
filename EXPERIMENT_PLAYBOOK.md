# Experiment Playbook

This file is a practical guide for:

- what is already implemented in this repo,
- what parameters you can tune,
- and what we are trying to prove with baseline vs S1 vs S2.

---

## 1) What is built right now

### Controller strategies

- `baseline`
  - No readiness-gate strategy behavior enabled.
- `s1-early-readiness`
  - Uses readiness gate `decomposition.dat6.io/drain`.
  - Controller is intended to set readiness gate `False` on termination.
- `s2-drain-verification`
  - Builds on S1 readiness-gate setup.
  - Adds `/drainez` polling logic in controller path before deletion decisions.

### App behavior (`app/src/main.rs`)

- Endpoints:
  - `/` (main request path)
  - `/healthz`
  - `/readyz` — **always returns 200 by design.** Readiness during
    decomposition is owned by the controller (via the
    `decomposition.dat6.io/drain` gate), not the app. This is a deliberate
    choice so the experiment isolates the controller's contribution; it
    means kube-proxy will not remove the pod from endpoints when the app is
    unhealthy. See module docs in `app/src/main.rs` for the trade-off.
  - `/drainez` — returns `active_connections`, `draining` (true only after
    SIGTERM has actually been received), and `ready_to_delete` (only true
    once `draining=true && active_connections=0`).
  - `/metrics`
- In-flight counter is tracked via atomic and guard.
- Graceful drain is **opt-in** via `DAT6_GRACEFUL_DRAIN=1` (set in the
  `s1-early-readiness` and `s2-drain-verification` overlays). When unset, the
  process inherits the kernel default for SIGTERM and dies immediately —
  this is what the `baseline` overlay uses.

### Test runner behavior

- `run-all-auto` runs all combinations:
  - strats: `baseline`, `s1-early-readiness`, `s2-drain-verification`
  - scenarios: `rollout`, `steady_scale_down`
- Cluster is reset per combination.
- App image is rebuilt and loaded into kind during deploy.
- Load path is now via NodePort:
  - `http://127.0.0.1:30080`
  - (avoids `kubectl port-forward` drop artifacts during rollout)
- Events are captured both as:
  - watch stream
  - sorted snapshot

### Aggregation outputs

- Per run:
  - `k6_results.json`
  - `client_results.json`
  - `k8s_events.log`
  - `shutdown_timeline.json`
  - `timeline.png` (if matplotlib available)
- Per combo:
  - `summary_repeats.csv`
  - `aggregate.json`
- Final compare:
  - `comparison.csv`
  - `comparison.md`

---

## 2) What we are trying to prove

Main thesis goal:

- Compare request loss and latency during shutdown churn across:
  - baseline
  - S1 early readiness removal
  - S2 active drain verification

Expected directional outcome:

- Baseline should be worse under sufficient stress.
- S1 should reduce loss by removing pods from endpoints earlier.
- S2 should be at least as good as S1 (possibly slightly higher latency due to verification path).

Important:

- If all strategies show 0% loss, your test is too gentle.
- If all strategies show similarly high loss, the test harness or path may be flawed.

---

## 3) What you can tune (the knobs)

You can tune these via environment variables when running `make run-all-auto` or `make run-repeats`.

### A) Load intensity knobs (most important)

- `K6_RPS`
  - Requests per second target.
  - Higher value increases contention and failure likelihood.
- `K6_VUS`
  - Number of virtual users.
  - Must be high enough to sustain chosen RPS.
- `K6_DURATION`
  - Test duration, e.g. `120s`, `150s`, `180s`.
  - Longer runs increase chance of observing rollout edge cases.
- `K6_SCENARIO`
  - Current default is `steady`.
  - Can be adjusted if needed.

### B) Profile knob

- `EXP_PROFILE`
  - `standard`
  - `thesis-stress`

Current defaults in `run_scenario.sh`:

- `standard`: `RPS=50`, `VUS=10`, `DURATION=90s`
- `thesis-stress`: `RPS=250`, `VUS=20`, `DURATION=120s`

### C) Strategy/controller knobs

- S1:
  - `DAT6_EARLY_READINESS_REMOVAL=1`
- S2:
  - `DAT6_EARLY_READINESS_REMOVAL=1 DAT6_DRAIN_VERIFICATION=1`

Handled automatically by `run-all-auto` per strategy.

### D) Scenario knobs

- `SCENARIO=rollout`
  - Best for observing replacement behavior and endpoint transitions.
- `SCENARIO=steady_scale_down`
  - Useful for controlled scale event behavior.

### E) Repeat count

- `N`
  - Number of repeats per combo.
  - Use `N=1` for smoke test, `N=5` (or higher) for thesis-quality stability.

---

## 4) Recommended tuning workflow

1. Smoke-test pipeline:
   - `EXP_PROFILE=thesis-stress make run-all-auto N=1`
2. If baseline still near 0% loss, increase load:
   - `K6_RPS=400 K6_VUS=40 K6_DURATION=150s`
3. If still near 0%, increase again:
   - `K6_RPS=600 K6_VUS=60 K6_DURATION=180s`
4. Once baseline shows clear degradation and S1/S2 improve:
   - run final set with `N=5`.

Example:

```bash
EXP_PROFILE=thesis-stress K6_RPS=400 K6_VUS=40 K6_DURATION=150s make run-all-auto N=3
```

Then final:

```bash
EXP_PROFILE=thesis-stress K6_RPS=400 K6_VUS=40 K6_DURATION=180s make run-all-auto N=5
```

---

## 5) How to interpret results quickly

Use `comparison.md`:

- `mean_loss%`:
  - primary reliability metric.
- `stddev`:
  - stability across repeats.
- `p99_mean` / `p99_max`:
  - tail-latency cost of each strategy.

Interpretation pattern:

- Good:
  - baseline higher loss, S1/S2 significantly lower.
- Suspicious:
  - all strategies near 0% loss -> load too easy.
  - all strategies similarly high loss -> strategy not taking effect or harness issue.

---

## 6) Known current caveats

- S1/S2 behavior is sensitive to controller timing and rollout order.
- The controller currently watches broadly; heavy cluster noise can affect responsiveness.
- One-repeat runs (`N=1`) are useful for smoke tests only, not conclusions.

### 6a) Controller-first ordering for S1/S2

S1 and S2 require the controller to be running **before** the pods enter
`Running`. Concretely:

- **S1** sets the `decomposition.dat6.io/drain` readiness gate to `True` on
  active pods. If the controller is not running, the gate stays `False` (the
  default for unset gates) and the pod's overall `Ready` condition will be
  `False`, which means the rollout can stall at `0/N available`.

- **S2** additionally relies on the controller patching the
  `decomposition.dat6.io/finalizer` onto each pod **before** it terminates.
  Once a pod has its `deletionTimestamp` set, Kubernetes may finish removing
  the pod resource before the controller can patch the finalizer in;
  `add_pod_finalizer` is best-effort in that branch but the deletion-gating
  it provides is no longer guaranteed. The supported flow is:

  1. Start the controller with `DAT6_EARLY_READINESS_REMOVAL=1
     DAT6_DRAIN_VERIFICATION=1`.
  2. Apply the S2 overlay; wait for pods to become `Running`.
  3. Verify the finalizer is present:
     `kubectl get pod -l app=drainable-service -o jsonpath='{.items[*].metadata.finalizers}'`
  4. Then run the scenario.

  `run_all_auto.sh` already does (1)→(2) in this order for non-`baseline`
  strategies; do not deploy first and start the controller afterwards.

### 6b) Graceful drain in the app

S1 and S2 only matter if the app stays alive long enough for in-flight
requests to complete (S1) and for `/drainez` to be polled (S2). The app
opts in via `DAT6_GRACEFUL_DRAIN=1` (already wired in the S1/S2 overlays).
The baseline overlay deliberately omits it so the comparison shows the
baseline's "no graceful shutdown" behavior. If you change overlays, keep
this asymmetry in mind.

### 6c) kind testbed: NodePort propagation race is not reproducible

This is the **most important methodology limitation** of this repo. The
canonical "endpoint propagation race" — kube-proxy still sending traffic to a
pod that has begun termination because Endpoints/EndpointSlice updates have
not yet reached every node — is *not* observable on a single-node kind
cluster, and is only marginally observable on a multi-node kind cluster.

Why:

- On the control-plane node where NodePort is exposed, kube-proxy updates
  iptables/nftables in microseconds after the local Endpoints object changes.
  Requests that hit the NodePort at that node are routed to the new endpoint
  set immediately.
- All kind nodes run on the same Linux host, sharing the kernel's networking
  stack, so the inter-node propagation window real clusters have (etcd ->
  kube-controller-manager -> kube-apiserver -> watch -> per-node kube-proxy
  -> per-node iptables) collapses to a function call.
- NodePort traffic from the host (`http://127.0.0.1:30080`) lands on the
  control-plane node, whose proxy update is the fastest; it sees the change
  before any in-flight request can even arrive at a dying pod.

What this means for our metrics:

- **Baseline loss is essentially 0% in this testbed.** That is *not*
  evidence that the baseline is fine — it is evidence that the testbed cannot
  reproduce the failure mode. Do not draw "S1/S2 are unnecessary" conclusions
  from a kind run.
- The fault we *can* observe in kind is the in-process one: the app dying
  mid-request because there is no SIGTERM handler. That is what the
  `DAT6_GRACEFUL_DRAIN=1` toggle exposes.
- For empirical validation of the endpoint-propagation hypothesis, the thesis
  needs a real multi-node cluster where kube-proxy on a *different* node from
  the dying pod is the one routing client traffic. Practically this means a
  small VPS-based cluster (e.g. 3 worker VMs + 1 control plane) with the
  client connecting through a node that does *not* host the pod under test,
  or an actual cloud LB in front of the Service. Document this constraint
  explicitly in the methodology chapter.

---

## 7) Useful commands

Smoke:

```bash
EXP_PROFILE=thesis-stress make run-all-auto N=1
```

Stress trial:

```bash
EXP_PROFILE=thesis-stress K6_RPS=400 K6_VUS=40 K6_DURATION=150s make run-all-auto N=3
```

Final:

```bash
EXP_PROFILE=thesis-stress K6_RPS=400 K6_VUS=40 K6_DURATION=180s make run-all-auto N=5
```
