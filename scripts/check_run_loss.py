#!/usr/bin/env python3
"""
Quick check of loss metrics across run_1..run_N.
Usage: check_run_loss.py <parent_dir>
  e.g. check_run_loss.py runs/20260311-162355-steady_scale_down-long-request-repeats

Prints total, errors, loss_pct per run from client_results.json (or from k6_results.json
if client_results.json missing) so you can verify 83% → 0% is from data or a bug.
"""
import json
import sys
from pathlib import Path


def summarize_k6(k6_path: Path) -> tuple[int, int, float] | None:
    """Parse k6 JSON; return (total, errors, loss_pct) or None."""
    total = 0
    errors = 0
    if not k6_path.exists():
        return None
    with open(k6_path) as f:
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
                elif metric == "http_req_failed" and val:
                    errors += 1
            except (json.JSONDecodeError, TypeError, ValueError):
                continue
    loss = (errors / total * 100) if total > 0 else 0.0
    return (total, errors, loss)


def main():
    if len(sys.argv) < 2:
        print("Usage: check_run_loss.py <parent_dir>", file=sys.stderr)
        sys.exit(1)
    parent = Path(sys.argv[1])
    if not parent.is_dir():
        print(f"Not a directory: {parent}", file=sys.stderr)
        sys.exit(1)

    def run_index(p: Path) -> int:
        try:
            return int(p.name.split("_")[1])
        except (IndexError, ValueError):
            return 0

    runs = sorted(parent.glob("run_*"), key=run_index)
    if not runs:
        print(f"No run_* dirs in {parent}", file=sys.stderr)
        sys.exit(1)

    print(f"Run directory: {parent}")
    print(f"{'run':<8} {'total':>8} {'errors':>8} {'loss_pct':>10}  source")
    print("-" * 50)

    for run_dir in runs:
        cr = run_dir / "client_results.json"
        k6 = run_dir / "k6_results.json"
        if cr.exists():
            with open(cr) as f:
                data = json.load(f)
            total = data.get("total_requests", 0)
            errors = data.get("errors", 0)
            loss = data.get("loss_pct", 0)
            src = "client_results.json"
        else:
            res = summarize_k6(k6)
            if res is None:
                print(f"{run_dir.name:<8} {'—':>8} {'—':>8} {'—':>10}  (no data)")
                continue
            total, errors, loss = res
            src = "k6_results.json"
        print(f"{run_dir.name:<8} {total:>8} {errors:>8} {loss:>9.2f}%  {src}")

    print()
    print("If run_1 has high loss and run_2+ have 0%, either:")
    print("  - Real: only run_1 hit the scale-down; later runs had no failures.")
    print("  - Bug: run_2+ have no/empty k6_results.json or different structure.")


if __name__ == "__main__":
    main()
