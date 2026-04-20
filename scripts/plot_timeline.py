#!/usr/bin/env python3
"""
Plot request errors over time with rollout killing markers.
Usage: python scripts/plot_timeline.py runs/<run-dir>/
"""
import json
import re
import sys
from datetime import datetime
from pathlib import Path

try:
    import matplotlib.pyplot as plt
except Exception:
    plt = None


def parse_k6_errors(k6_path: Path):
    points = []
    with k6_path.open() as f:
        for line in f:
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if obj.get("type") != "Point":
                continue
            metric = obj.get("metric")
            if metric not in {"errors", "http_req_failed"}:
                continue
            data = obj.get("data", {})
            ts = data.get("time")
            if not ts:
                continue
            value = float(data.get("value", 0))
            # http_req_failed is a per-request boolean signal in k6 points.
            if metric == "http_req_failed" and value <= 0:
                continue
            points.append((ts, value))
    return points


def parse_killing_events(timeline_path: Path):
    with timeline_path.open() as f:
        timeline = json.load(f)
    events = timeline.get("events", [])
    times = []
    for e in events:
        if "Killing" not in e:
            continue
        m = re.match(r"^(\S+)\s+", e)
        if not m:
            continue
        times.append(m.group(1))
    return times


def to_dt(ts: str):
    if ts.endswith("Z"):
        ts = ts[:-1] + "+00:00"
    return datetime.fromisoformat(ts)


def main():
    if len(sys.argv) != 2:
        print("Usage: python scripts/plot_timeline.py runs/<run-dir>/", file=sys.stderr)
        return 1

    run_dir = Path(sys.argv[1])
    if plt is None:
        print("matplotlib not available, skipping timeline plot.")
        return 0

    k6_path = run_dir / "k6_results.json"
    timeline_path = run_dir / "shutdown_timeline.json"
    if not k6_path.exists() or not timeline_path.exists():
        print("Missing k6_results.json or shutdown_timeline.json, skipping plot.")
        return 0

    err_points = parse_k6_errors(k6_path)
    if not err_points:
        print("No error points found; writing empty timeline figure.")
    err_times = [to_dt(t) for t, _ in err_points] if err_points else []
    err_vals = [v for _, v in err_points] if err_points else []

    kill_times = [to_dt(t) for t in parse_killing_events(timeline_path)]

    fig, ax1 = plt.subplots(figsize=(12, 4))
    if err_points:
        ax1.plot(err_times, err_vals, color="tab:red", linewidth=1.0, label="errors")
    ax1.set_ylabel("error signal", color="tab:red")
    ax1.tick_params(axis="y", labelcolor="tab:red")

    ax2 = ax1.twinx()
    ax2.set_ylabel("k8s events", color="tab:blue")
    ax2.set_yticks([])
    for t in kill_times:
        ax2.axvline(t, color="tab:blue", linestyle="--", alpha=0.7, linewidth=1.0)

    ax1.set_title("Error timeline vs pod Killing events")
    ax1.set_xlabel("time")
    fig.tight_layout()

    out = run_dir / "timeline.png"
    fig.savefig(out, dpi=150)
    print(f"Wrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
