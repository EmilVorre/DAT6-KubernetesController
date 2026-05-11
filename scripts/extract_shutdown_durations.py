#!/usr/bin/env python3
"""
Extract per-pod shutdown durations from `pod_states_watch.jsonl`.

Background
----------
`run_scenario.sh` captures a pod-state watch with absolute Kubernetes
timestamps:

    kubectl get pods -l app=drainable-service \
      --output-watch-events --watch -o json

That stream lets us reconstruct, for every pod that went through a
SIGTERM during the run:

  start  = metadata.deletionTimestamp - spec.terminationGracePeriodSeconds
           (the Kubernetes API server's "delete request received" time;
           the kubelet sends SIGTERM essentially immediately on observing
           this, so we treat it as the SIGTERM timestamp.)

  end    = max(status.containerStatuses[*].state.terminated.finishedAt)
           (when the container's PID 1 actually exited.)

  duration = end - start

This is the metric the user wants for "how fast does each strategy
actually shut a pod down once the controller has started the shutdown
process". It complements the request-loss metric:

  baseline (drain budget 1s):  fast shutdown, lossy
  S1       (drain budget 8s):  medium shutdown, less lossy
  S2       (controller polled): fast-when-possible, slow-when-needed,
                                 zero loss

Output
------
Writes `shutdown_durations.json` next to the input file with:

  {
    "per_pod": [{"pod": "...", "duration_secs": float, "grace_secs": int}, ...],
    "stats":   {"count": int, "median_secs": float, "p95_secs": float,
                "max_secs": float, "min_secs": float}
  }

Robustness notes
----------------
* `kubectl ... -o json --watch` emits pretty-printed JSON objects
  concatenated, *not* line-delimited. We use `json.JSONDecoder.raw_decode`
  to stream them.
* Pods that never observed `deletionTimestamp` (e.g. the new replicas
  brought up during a rollout) are skipped.
* Pods that observed `deletionTimestamp` but no terminated container
  status (e.g. the watch ended before the container exited) are skipped.
"""
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path


def parse_ts(s):
    """RFC 3339 timestamp from kubectl JSON output -> aware datetime."""
    if not s:
        return None
    # k8s emits 'Z' suffix; fromisoformat needs '+00:00'
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def stream_objects(path):
    """Yield every JSON object in a file containing back-to-back JSON values."""
    text = Path(path).read_text()
    decoder = json.JSONDecoder()
    idx = 0
    n = len(text)
    while idx < n:
        # Skip whitespace between objects
        while idx < n and text[idx].isspace():
            idx += 1
        if idx >= n:
            break
        try:
            obj, end = decoder.raw_decode(text, idx)
        except json.JSONDecodeError:
            # Truncated final object (watch was killed mid-emit). Stop here.
            break
        yield obj
        idx = end


def main():
    output_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
    watch_file = output_dir / "pod_states_watch.jsonl"
    out_file = output_dir / "shutdown_durations.json"

    empty = {
        "per_pod": [],
        "stats": {
            "count": 0,
            "median_secs": 0.0,
            "p95_secs": 0.0,
            "max_secs": 0.0,
            "min_secs": 0.0,
        },
    }

    if not watch_file.exists() or watch_file.stat().st_size == 0:
        out_file.write_text(json.dumps(empty, indent=2))
        print("No pod_states_watch.jsonl; wrote empty shutdown_durations.json")
        return

    # name -> {deletion_ts, grace_secs, finished_ts}
    pods = {}
    for evt in stream_objects(watch_file):
        # Each watch event is a WatchEvent: {type, object: <Pod>}
        obj = evt.get("object") if "object" in evt else evt
        if not isinstance(obj, dict) or obj.get("kind") != "Pod":
            continue
        meta = obj.get("metadata") or {}
        spec = obj.get("spec") or {}
        status = obj.get("status") or {}

        name = meta.get("name")
        if not name:
            continue
        pod = pods.setdefault(name, {})

        # deletionTimestamp: first time we observe it set
        dt_raw = meta.get("deletionTimestamp")
        if dt_raw and "deletion_ts" not in pod:
            pod["deletion_ts"] = parse_ts(dt_raw)

        grace = spec.get("terminationGracePeriodSeconds")
        if grace is not None:
            pod["grace_secs"] = int(grace)

        # Container terminated timestamp: latest seen `finishedAt`. There's
        # only one container in this app but be defensive.
        for cs in status.get("containerStatuses") or []:
            term = (cs.get("state") or {}).get("terminated")
            if not term:
                continue
            fin = parse_ts(term.get("finishedAt"))
            if fin and (
                "finished_ts" not in pod or fin > pod["finished_ts"]
            ):
                pod["finished_ts"] = fin

    per_pod = []
    for name, p in pods.items():
        if "deletion_ts" not in p or "finished_ts" not in p or "grace_secs" not in p:
            continue
        sigterm_ts = p["deletion_ts"] - timedelta(seconds=p["grace_secs"])
        duration = (p["finished_ts"] - sigterm_ts).total_seconds()
        # Sanity: clip negative durations (clock skew / unusual timing)
        if duration < 0:
            continue
        per_pod.append(
            {
                "pod": name,
                "duration_secs": round(duration, 3),
                "grace_secs": p["grace_secs"],
            }
        )

    if not per_pod:
        out_file.write_text(json.dumps(empty, indent=2))
        print("No pods with complete shutdown timing; wrote empty stats")
        return

    durations = sorted(d["duration_secs"] for d in per_pod)
    n = len(durations)

    def pct(p):
        if n == 0:
            return 0.0
        # nearest-rank, simple and adequate for small n
        i = max(0, min(n - 1, int(round(p * (n - 1)))))
        return durations[i]

    median = pct(0.50)
    p95 = pct(0.95)

    stats = {
        "count": n,
        "median_secs": round(median, 3),
        "p95_secs": round(p95, 3),
        "max_secs": round(durations[-1], 3),
        "min_secs": round(durations[0], 3),
    }

    out_file.write_text(json.dumps({"per_pod": per_pod, "stats": stats}, indent=2))
    print(
        f"Shutdown durations: count={stats['count']} "
        f"median={stats['median_secs']:.2f}s "
        f"p95={stats['p95_secs']:.2f}s "
        f"max={stats['max_secs']:.2f}s"
    )


if __name__ == "__main__":
    main()
