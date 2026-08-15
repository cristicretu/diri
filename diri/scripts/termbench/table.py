#!/usr/bin/env python3
"""Render the three-way comparison from the suite CSVs.

Each row is one workload; the number is MB/s of fully-parsed throughput
(bytes divided by the time until the terminal answered a query queued behind
the payload). Higher is better. The winner of each row is marked.
"""
import os
import sys

RESULTS = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(__file__), "results")
TERMS = ["dirijor", "ghostty", "kitty"]

rates, times = {}, {}
for term in TERMS:
    path = os.path.join(RESULTS, f"{term}.suite.csv")
    if not os.path.exists(path):
        continue
    for line in open(path):
        parts = line.strip().split(",")
        if len(parts) != 4:
            continue
        workload, _t_cat, t_sync, rate = parts
        rates.setdefault(workload, {})[term] = float(rate)
        times.setdefault(workload, {})[term] = t_sync

present = [t for t in TERMS if any(t in v for v in rates.values())]
width = max((len(w) for w in rates), default=10) + 2

print(f"{'workload':<{width}}" + "".join(f"{t:>12}" for t in present) + "   winner")
print("-" * (width + 12 * len(present) + 12))
wins = dict.fromkeys(present, 0)
for workload in sorted(rates):
    row = rates[workload]
    best = max(row, key=row.get) if row else None
    if best:
        wins[best] += 1
    cells = ""
    for term in present:
        value = row.get(term)
        cells += f"{value:>11.1f}{'*' if term == best else ' '}" if value else f"{'-':>12}"
    print(f"{workload:<{width}}{cells}   {best or '-'}")

print("-" * (width + 12 * len(present) + 12))
print(f"{'row wins':<{width}}" + "".join(f"{wins[t]:>12}" for t in present))

for term in present:
    path = os.path.join(RESULTS, f"{term}.latency.csv")
    if os.path.exists(path):
        median, low, p95, count = open(path).read().strip().split(",")
        print(f"{term:>10} query latency: median {median} ms, min {low} ms, p95 {p95} ms (n={count})")
