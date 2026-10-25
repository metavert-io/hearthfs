#!/usr/bin/env python3
"""Per-file read latency benchmark for a mounted (or raw) directory tree.

Usage: bench_read.py <root> <label> [--files <manifest>]
Walks <root> (skipping @-magic dirs), times open+read+close per file,
two passes (cold = first touch through this mount, warm = second),
plus readdir and stat passes. Emits JSON to stdout.
"""
import json
import os
import sys
import time


def collect(root):
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if not d.startswith('@'))
        for f in sorted(filenames):
            if not f.startswith('@'):
                out.append(os.path.join(dirpath, f))
    return out


def time_reads(paths):
    lat = []
    for p in paths:
        t0 = time.perf_counter()
        with open(p, 'rb') as fh:
            fh.read()
        lat.append((time.perf_counter() - t0) * 1000.0)
    return lat


def time_stats(paths):
    lat = []
    for p in paths:
        t0 = time.perf_counter()
        os.stat(p)
        lat.append((time.perf_counter() - t0) * 1000.0)
    return lat


def time_walk(root):
    t0 = time.perf_counter()
    n = sum(len(fs) for _, _, fs in os.walk(root))
    return (time.perf_counter() - t0) * 1000.0, n


def pct(sorted_vals, p):
    if not sorted_vals:
        return None
    k = min(len(sorted_vals) - 1, max(0, int(round(p / 100.0 * (len(sorted_vals) - 1)))))
    return sorted_vals[k]


def summarize(lat):
    s = sorted(lat)
    return {
        'n': len(s),
        'mean_ms': sum(s) / len(s),
        'p50_ms': pct(s, 50),
        'p90_ms': pct(s, 90),
        'p95_ms': pct(s, 95),
        'p99_ms': pct(s, 99),
        'max_ms': s[-1],
        'total_ms': sum(s),
    }


def main():
    root, label = sys.argv[1], sys.argv[2]
    paths = collect(root)
    if not paths:
        print(json.dumps({'label': label, 'error': 'no files found'}))
        sys.exit(1)
    total_bytes = sum(os.path.getsize(p) for p in paths)

    walk_ms, walk_n = time_walk(root)
    stat_lat = time_stats(paths)
    cold = time_reads(paths)
    warm = time_reads(paths)

    result = {
        'label': label,
        'root': root,
        'files': len(paths),
        'total_bytes': total_bytes,
        'readdir_full_walk_ms': walk_ms,
        'stat': summarize(stat_lat),
        'cold_read': summarize(cold),
        'warm_read': summarize(warm),
        'cold_throughput_mb_s': (total_bytes / 1e6) / (sum(cold) / 1000.0),
        'warm_throughput_mb_s': (total_bytes / 1e6) / (sum(warm) / 1000.0),
    }
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
