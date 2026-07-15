#!/usr/bin/env python3
"""Compare fat and ThinLTO Tau binaries on a prepared session-store fixture."""

import argparse
import json
import os
import random
import statistics
import subprocess
import time


def run_batch(binary, arguments, iterations):
    """Return elapsed seconds for one subprocess batch."""
    started = time.perf_counter_ns()
    for _ in range(iterations):
        subprocess.run(
            [binary, *arguments],
            check=True,
            stdout=subprocess.DEVNULL,
        )
    return (time.perf_counter_ns() - started) / 1e9


def main():
    """Parse arguments, run paired batches, and print machine-readable results."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--fat", required=True)
    parser.add_argument("--thin", required=True)
    parser.add_argument("--sessions-dir", required=True)
    parser.add_argument("--seed", type=int, default=2000)
    parser.add_argument("--rounds", type=int, default=25)
    parser.add_argument("--iterations", type=int, default=10)
    args = parser.parse_args()

    os.sched_setaffinity(0, {min(os.sched_getaffinity(0))})
    command_arguments = ["session-list", "--sessions-dir", args.sessions_dir]
    for binary in (args.fat, args.thin):
        run_batch(binary, command_arguments, 5)

    pairs = []
    for round_index in range(args.rounds):
        order = [("fat", args.fat), ("thin", args.thin)]
        random.Random(args.seed + round_index).shuffle(order)
        pair = {}
        for label, binary in order:
            pair[label] = run_batch(binary, command_arguments, args.iterations)
        pairs.append(pair)

    ratios = [pair["thin"] / pair["fat"] for pair in pairs]
    print(
        json.dumps(
            {
                "seed": args.seed,
                "rounds": args.rounds,
                "iterations": args.iterations,
                "fat_median_seconds_per_invocation": (
                    statistics.median(p["fat"] for p in pairs) / args.iterations
                ),
                "thin_median_seconds_per_invocation": (
                    statistics.median(p["thin"] for p in pairs) / args.iterations
                ),
                "paired_ratio_median": statistics.median(ratios),
                "paired_ratio_range": [min(ratios), max(ratios)],
                "paired_ratios": ratios,
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
