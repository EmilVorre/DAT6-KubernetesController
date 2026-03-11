#!/usr/bin/env python3
"""
Summarize a run: parse k6 JSON output, produce summary.csv and client_results.json.

Assumes k6 emits one Point per request for http_reqs and one per request for
http_req_failed (value 0 or 1). Loss = errors / total_requests * 100.
"""
import json
import os
import sys
from pathlib import Path

def main():
    output_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(".")
    k6_file = output_dir / "k6_results.json"

    if not k6_file.exists():
        print(f"No k6 results at {k6_file}")
        return

    # Parse k6 JSON (line-delimited)
    http_durations = []
    total = 0
    errors = 0

    with open(k6_file) as f:
        for line in f:
            try:
                obj = json.loads(line)
                if obj.get("type") != "Point":
                    continue
                metric = obj.get("metric", "")
                data = obj.get("data", {})
                val = data.get("value", 0)
                if metric == "http_reqs":
                    total += 1
                elif metric == "http_req_duration":
                    http_durations.append(float(val))
                elif metric == "http_req_failed" and val:
                    errors += 1
            except (json.JSONDecodeError, TypeError, ValueError):
                continue

    # k6 JSON: one Point per request for http_reqs and http_req_failed (value 0 or 1)
    if errors > total:
        # Inconsistent stream (e.g. different emit order); cap and warn
        errors = total
        print("Warning: error count exceeded total requests; capping loss at 100%", file=sys.stderr)
    loss_pct = (errors / total * 100) if total > 0 else 0
    durations_sorted = sorted(http_durations) if http_durations else [0]
    n = len(durations_sorted)
    p50 = durations_sorted[int(n * 0.5)] if n > 0 else 0
    p95 = durations_sorted[int(n * 0.95)] if n > 0 else 0
    p99 = durations_sorted[int(n * 0.99)] if n > 0 else 0

    summary = {
        "total_requests": total,
        "errors": errors,
        "loss_pct": round(loss_pct, 2),
        "p50_ms": round(p50, 2),
        "p95_ms": round(p95, 2),
        "p99_ms": round(p99, 2),
    }

    with open(output_dir / "client_results.json", "w") as f:
        json.dump(summary, f, indent=2)

    # CSV row
    csv_row = f"{output_dir.name},{total},{errors},{loss_pct:.2f},{p50:.2f},{p95:.2f},{p99:.2f}"
    csv_header = "run,total,errors,loss_pct,p50_ms,p95_ms,p99_ms"
    csv_file = output_dir / "summary.csv"
    if not csv_file.exists():
        with open(csv_file, "w") as f:
            f.write(csv_header + "\n")
    with open(csv_file, "a") as f:
        f.write(csv_row + "\n")

    print(f"Summary: loss={loss_pct:.2f}% p99={p99:.0f}ms")
    print(f"Written: {output_dir}/client_results.json, {output_dir}/summary.csv")

if __name__ == "__main__":
    main()
