#!/usr/bin/env python3
"""Per-crate line coverage from `cargo llvm-cov --json --summary-only`.

Usage: coverage_summary.py <coverage.json>  (prints a Markdown table)
"""
import json
import re
import sys
from collections import defaultdict


def main(path):
    with open(path, encoding="utf-8") as f:
        data = json.load(f)["data"][0]
    lines = defaultdict(lambda: [0, 0])
    for entry in data["files"]:
        m = re.search(r"(?:^|/)(crates/[^/]+|xtask)/", entry["filename"].replace("\\", "/"))
        if not m:
            continue
        summary = entry["summary"]["lines"]
        lines[m.group(1)][0] += summary["covered"]
        lines[m.group(1)][1] += summary["count"]
    total = data["totals"]["lines"]
    print("## Line coverage per crate\n")
    print("| Crate | Covered | Lines | % |")
    print("|---|---:|---:|---:|")
    for name, (covered, count) in sorted(lines.items()):
        pct = 100.0 * covered / count if count else 0.0
        print(f"| `{name.removeprefix('crates/')}` | {covered} | {count} | {pct:.1f} |")
    print(f"| **workspace** | {total['covered']} | {total['count']} | {total['percent']:.1f} |")


if __name__ == "__main__":
    main(sys.argv[1])
