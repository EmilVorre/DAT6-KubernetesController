#!/usr/bin/env python3
"""
Collect metrics from Prometheus for a run.
Output: OUTPUT_DIR/prom_snapshot.json (optional), OUTPUT_DIR/shutdown_timeline.json
"""
import json
import os
import subprocess
import sys
from datetime import datetime

def main():
    output_dir = sys.argv[1] if len(sys.argv) > 1 else "."
    os.makedirs(output_dir, exist_ok=True)

    # Try to get Prometheus snapshot via port-forward
    try:
        # Assume Prometheus is in observability namespace
        result = subprocess.run(
            ["kubectl", "get", "svc", "-n", "observability", "-o", "name"],
            capture_output=True, text=True, timeout=5
        )
        if "prometheus" in result.stdout.lower():
            # Port-forward and query (simplified - in real use, run before/after)
            pass
    except Exception:
        pass

    # Shutdown timeline from k8s events (if we have them)
    timeline = {
        "collected_at": datetime.utcnow().isoformat() + "Z",
        "events": [],
    }
    events_file = os.path.join(output_dir, "k8s_events.log")
    if os.path.exists(events_file):
        with open(events_file) as f:
            for line in f:
                if "drainable-service" in line or "drainable" in line.lower():
                    timeline["events"].append(line.strip())

    with open(os.path.join(output_dir, "shutdown_timeline.json"), "w") as f:
        json.dump(timeline, f, indent=2)

    print(f"Collected timeline to {output_dir}/shutdown_timeline.json")

if __name__ == "__main__":
    main()
