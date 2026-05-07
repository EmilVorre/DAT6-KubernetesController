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
  - `/readyz` (always 200)
  - `/drainez` (returns active connections + ready_to_delete)
  - `/metrics`
- In-flight counter is tracked via atomic and guard.
- App-level custom graceful shutdown logic has been removed.

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
