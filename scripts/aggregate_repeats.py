#!/usr/bin/env python3
"""
Aggregate N repeated runs into summary_repeats.csv and aggregate.json.
Usage: aggregate_repeats.py <output_parent_dir> <N>
"""
import json
import sys
from pathlib import Path


def main():
    if len(sys.argv) < 3:
        print("Usage: aggregate_repeats.py <output_parent_dir> <N>", file=sys.stderr)
        return
    parent = Path(sys.argv[1])
    n = int(sys.argv[2])

    rows = []
    for i in range(1, n + 1):
        run_dir = parent / f"run_{i}"
        results_file = run_dir / "client_results.json"
        if not results_file.exists():
            continue
        with open(results_file) as f:
            data = json.load(f)
        run_name = run_dir.name
        total = data.get("total_requests", 0)
        errors = data.get("errors", 0)
        loss_pct = data.get("loss_pct", 0)
        p50 = data.get("p50_ms", 0)
        p95 = data.get("p95_ms", 0)
        p99 = data.get("p99_ms", 0)
        sd_count = data.get("shutdown_count", 0)
        sd_median = data.get("shutdown_median_secs", 0)
        sd_p95 = data.get("shutdown_p95_secs", 0)
        sd_max = data.get("shutdown_max_secs", 0)
        rows.append(
            (run_name, total, errors, loss_pct, p50, p95, p99,
             sd_count, sd_median, sd_p95, sd_max)
        )

    if not rows:
        print(f"No client_results.json found in {parent}/run_1..run_{n}", file=sys.stderr)
        return

    # summary_repeats.csv
    csv_path = parent / "summary_repeats.csv"
    header = (
        "run,total,errors,loss_pct,p50_ms,p95_ms,p99_ms,"
        "shutdown_count,shutdown_median_secs,shutdown_p95_secs,shutdown_max_secs"
    )
    with open(csv_path, "w") as f:
        f.write(header + "\n")
        for r in rows:
            f.write(
                f"{r[0]},{r[1]},{r[2]},{r[3]:.2f},{r[4]:.2f},{r[5]:.2f},{r[6]:.2f},"
                f"{r[7]},{r[8]:.3f},{r[9]:.3f},{r[10]:.3f}\n"
            )
    print(f"Wrote {csv_path} ({len(rows)} rows)")

    # aggregate.json: mean, min, max for numeric metrics
    loss_vals = [r[3] for r in rows]
    p50_vals = [r[4] for r in rows]
    p95_vals = [r[5] for r in rows]
    p99_vals = [r[6] for r in rows]
    sd_median_vals = [r[8] for r in rows if r[7] > 0]
    sd_p95_vals = [r[9] for r in rows if r[7] > 0]
    sd_max_vals = [r[10] for r in rows if r[7] > 0]

    def stats(vals):
        if not vals:
            return {"mean": 0, "min": 0, "max": 0}
        return {
            "mean": round(sum(vals) / len(vals), 2),
            "min": round(min(vals), 2),
            "max": round(max(vals), 2),
        }

    aggregate = {
        "n": len(rows),
        "loss_pct": stats(loss_vals),
        "p50_ms": stats(p50_vals),
        "p95_ms": stats(p95_vals),
        "p99_ms": stats(p99_vals),
        "shutdown_median_secs": stats(sd_median_vals),
        "shutdown_p95_secs": stats(sd_p95_vals),
        "shutdown_max_secs": stats(sd_max_vals),
    }
    agg_path = parent / "aggregate.json"
    with open(agg_path, "w") as f:
        json.dump(aggregate, f, indent=2)
    print(f"Wrote {agg_path}")


if __name__ == "__main__":
    main()
