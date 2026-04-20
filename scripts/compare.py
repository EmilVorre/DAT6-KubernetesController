#!/usr/bin/env python3
"""
Aggregate strategy/scenario repeat summaries into comparison outputs.
Usage: python scripts/compare.py runs/compare-<timestamp>/
"""
import csv
import statistics
import sys
from pathlib import Path


def summarize_summary_csv(path: Path):
    rows = list(csv.DictReader(path.open()))
    if not rows:
        return None
    loss = [float(r["loss_pct"]) for r in rows]
    p99 = [float(r["p99_ms"]) for r in rows]
    return {
        "runs": len(rows),
        "mean_loss_pct": statistics.mean(loss),
        "stddev_loss_pct": statistics.pstdev(loss) if len(loss) > 1 else 0.0,
        "p99_mean": statistics.mean(p99),
        "p99_max": max(p99),
    }


def markdown_table(rows):
    lines = [
        "| scenario | strat | runs | mean_loss% | stddev | p99_mean | p99_max |",
        "|---|---|---:|---:|---:|---:|---:|",
    ]
    for row in rows:
        lines.append(
            "| {scenario} | {strat} | {runs} | {mean_loss_pct:.2f} | {stddev_loss_pct:.2f} | {p99_mean:.2f} | {p99_max:.2f} |".format(
                **row
            )
        )
    return "\n".join(lines) + "\n"


def main():
    if len(sys.argv) != 2:
        print("Usage: python scripts/compare.py runs/compare-<timestamp>/", file=sys.stderr)
        return 1

    compare_dir = Path(sys.argv[1])
    if not compare_dir.exists():
        print(f"Directory does not exist: {compare_dir}", file=sys.stderr)
        return 1

    output_rows = []
    for summary in sorted(compare_dir.glob("*/summary_repeats.csv")):
        parent = summary.parent.name
        if "-" not in parent:
            continue
        parts = parent.split("-")
        scenario = parts[-1]
        strat = "-".join(parts[:-1])
        data = summarize_summary_csv(summary)
        if not data:
            continue
        output_rows.append(
            {
                "scenario": scenario,
                "strat": strat,
                **data,
            }
        )

    output_rows.sort(key=lambda r: (r["scenario"], r["strat"]))
    if not output_rows:
        print(f"No summary_repeats.csv found under {compare_dir}", file=sys.stderr)
        return 1

    csv_path = compare_dir / "comparison.csv"
    fields = ["scenario", "strat", "runs", "mean_loss_pct", "stddev_loss_pct", "p99_mean", "p99_max"]
    with csv_path.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fields)
        writer.writeheader()
        writer.writerows(output_rows)

    md_path = compare_dir / "comparison.md"
    md_path.write_text(markdown_table(output_rows))

    print("scenario            strat                    runs  mean_loss%  stddev  p99_mean  p99_max")
    for r in output_rows:
        print(
            f"{r['scenario']:<19} {r['strat']:<24} {r['runs']:>4}  "
            f"{r['mean_loss_pct']:>9.2f}  {r['stddev_loss_pct']:>6.2f}  "
            f"{r['p99_mean']:>8.2f}  {r['p99_max']:>7.2f}"
        )
    print(f"\nWrote {csv_path}")
    print(f"Wrote {md_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
