#!/usr/bin/env python3
"""Run the compiled integration test locally and retain raw and paired results.

Usage: python3 summarize.py BINARY OUTPUT_PREFIX
Run after building; never overlaps measurement with this task's compilation.
Uses only Python's standard library. OUTPUT_PREFIX's parent must exist.
"""

import json
from pathlib import Path
import statistics
import subprocess
import sys


def stats(values):
    return {
        "median": statistics.median(values),
        "mean": statistics.mean(values),
        "stdev": statistics.stdev(values),
        "min": min(values),
        "max": max(values),
        "count": len(values),
    }


binary = Path(sys.argv[1]).resolve(strict=True)
prefix = Path(sys.argv[2]).resolve()
assert prefix.parent.is_dir()
svm = Path(__file__).resolve().parents[2]
command = [str(binary), "call_overhead::bench_warmed_singleton_vs_small_batches",
           "--exact", "--ignored", "--nocapture", "--test-threads=1"]
result = subprocess.run(command, cwd=svm, capture_output=True, text=True)
prefix.with_suffix(".txt").write_text((result.stdout + result.stderr).rstrip() + "\n")
result.check_returncode()
samples = {}
components = {}
for line in result.stdout.splitlines():
    if line.startswith("sample "):
        row = dict(word.split("=", 1) for word in line.split()[1:])
        samples[(row["sbf"], int(row["round"]), int(row["pair"]),
                 int(row["batch"]))] = row
    elif line.startswith(("empty ", "builtin_clone ")):
        row = dict(word.split("=", 1) for word in line.split()[1:])
        components.setdefault((line.split()[0], row["sbf"]), []).append(
            float(row["ns_per_call"]))

assert len(samples) == 192, "missing or duplicated sample rows"
summary = {"command": command, "pairs": [], "components": []}
for sbf in ["false", "true"]:
    for batch in [2, 4, 8, 16]:
        singleton = [samples[(sbf, r, batch, 1)] for r in range(12)]
        batched = [samples[(sbf, r, batch, batch)] for r in range(12)]
        single_ns = [float(row["ns_per_tx"]) for row in singleton]
        batch_ns = [float(row["ns_per_tx"]) for row in batched]
        summary["pairs"].append({
            "sbf": sbf, "batch": batch,
            "singleton_ns_per_tx": stats(single_ns),
            "batched_ns_per_tx": stats(batch_ns),
            "paired_saved_ns_per_tx": stats([a - b for a, b in zip(single_ns, batch_ns)]),
            "paired_reduction_percent": stats([100 * (a - b) / a for a, b in zip(single_ns, batch_ns)]),
            "native_median_ns_per_tx": {
                metric: {"singleton": statistics.median(float(row[metric]) for row in singleton),
                         "batched": statistics.median(float(row[metric]) for row in batched)}
                for metric in ["validate_ns", "load_ns", "execute_ns", "cache_ns"]
            },
        })
for (component, sbf), values in components.items():
    assert len(values) == 12
    summary["components"].append({"component": component, "sbf": sbf,
                                  "ns_per_call": stats(values)})
prefix.with_suffix(".json").write_text(json.dumps(summary, indent=2) + "\n")
for pair in summary["pairs"]:
    saved = pair["paired_saved_ns_per_tx"]
    print(f"sbf={pair['sbf']} batch={pair['batch']} "
          f"singleton={pair['singleton_ns_per_tx']['median']:.1f} "
          f"batched={pair['batched_ns_per_tx']['median']:.1f} "
          f"saved_median={saved['median']:.1f} saved_sd={saved['stdev']:.1f} "
          f"saved_range={saved['min']:.1f}..{saved['max']:.1f} "
          f"reduction={pair['paired_reduction_percent']['median']:.2f}%")
for component in summary["components"]:
    print(component)
