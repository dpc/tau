#!/usr/bin/env python3
"""Routine content-free provider/model charts using Tau's built-in trace export."""

import argparse
import collections
import csv
import datetime as dt
import html
import json
import math
import os
import pathlib
import statistics
import subprocess
import tempfile

UTC = dt.timezone.utc
BUCKET_US = 6 * 3600 * 1_000_000
DAY_US = 24 * 3600 * 1_000_000
COLORS = ["#2563eb", "#dc2626", "#059669", "#7c3aed", "#ea580c", "#0891b2",
          "#be185d", "#4d7c0f"]
FIELDS = ["bucket_start", "bucket_end", "provider", "model", "completed_samples",
          "latency_samples", "rate_samples", "median_wall_latency_seconds",
          "median_output_tokens_per_wall_second"]


def integer(value):
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def instant(text):
    value = dt.datetime.fromisoformat(text.replace("Z", "+00:00"))
    if value.tzinfo is None:
        raise ValueError("timestamps must have an explicit UTC offset")
    return int(value.timestamp() * 1_000_000)


def iso(value):
    return (dt.datetime(1970, 1, 1, tzinfo=UTC) +
            dt.timedelta(microseconds=value)).isoformat().replace("+00:00", "Z")


def read_trace(stream, since, until, counts):
    """Select only ordinary prompt scalars; discard all IDs before output."""
    header = json.loads(next(stream))
    if (header.get("schema") != "tau.agent_performance" or
            header.get("schema_version") != 0 or
            header.get("record_type") != "header" or
            header.get("content_included") is not False):
        raise ValueError("unsupported performance trace")
    origin = header.get("origin_recorded_at_unix_micros")
    samples = []
    seen = set()
    for line in stream:
        row = json.loads(line)
        if row.get("record_type") != "provider_prompt":
            continue
        counts["prompt_rows"] += 1
        identity = (row["agent_id"], row["agent_prompt_id"], row["journal_seq"])
        if identity in seen:
            counts["duplicate_rows"] += 1
            continue
        seen.add(identity)
        if not row.get("terminal_present"):
            counts["incomplete_prompts"] += 1
            continue
        terminal = row.get("terminal_at_us")
        if not integer(origin) or not integer(terminal):
            counts["missing_terminal_time"] += 1
            continue
        timestamp = origin + terminal
        if not since <= timestamp < until:
            counts["out_of_range"] += 1
            continue
        model = row.get("model")
        if not isinstance(model, str) or "/" not in model:
            counts["missing_provider_model"] += 1
            continue
        provider, model = model.split("/", 1)
        if not provider or not model:
            counts["missing_provider_model"] += 1
            continue
        elapsed = row.get("recorded_at_wall_elapsed_us")
        latency = elapsed / 1_000_000 if integer(elapsed) else None
        tokens = row.get("response_received_tokens")
        rate = tokens / latency if integer(tokens) and latency and latency > 0 else None
        counts["selected_completed"] += 1
        if latency is None:
            counts["missing_latency"] += 1
        if rate is None:
            counts["missing_rate"] += 1
        samples.append((timestamp // BUCKET_US * BUCKET_US, provider, model, latency, rate))
    return samples


def aggregate(samples, since, until):
    groups = collections.defaultdict(list)
    for bucket, provider, model, latency, rate in samples:
        groups[bucket, provider, model].append((latency, rate))
    rows = []
    for (bucket, provider, model), values in sorted(groups.items()):
        latencies = [v[0] for v in values if v[0] is not None]
        rates = [v[1] for v in values if v[1] is not None]
        rows.append(dict(
            bucket_start=iso(max(bucket, since)),
            bucket_end=iso(min(bucket + BUCKET_US, until)),
            provider=provider, model=model, completed_samples=len(values),
            latency_samples=len(latencies), rate_samples=len(rates),
            median_wall_latency_seconds=statistics.median(latencies) if latencies else "",
            median_output_tokens_per_wall_second=statistics.median(rates) if rates else "",
        ))
    return rows


def chart(rows, metric, title, unit, since, until):
    """Linear axes, bucket midpoint dots, and gaps across missing six-hour bins."""
    series = sorted({(r["provider"], r["model"]) for r in rows})
    width, left, right, top, bottom = 1200, 85, 1170, 100, 460
    height = 545 + 22 * len(series)
    maximum = max((r[metric] for r in rows if r[metric] != ""), default=1) or 1
    maximum *= 1.05
    x = lambda t: left + (t - since) / (until - since) * (right - left)
    y = lambda v: bottom - v / maximum * (bottom - top)
    svg = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
           f'viewBox="0 0 {width} {height}">',
           '<rect width="100%" height="100%" fill="white"/>',
           '<g font-family="sans-serif" font-size="13" fill="#111827">',
           f'<text x="20" y="27" font-size="20">{html.escape(title)}</text>',
           '<text x="20" y="50">Median per prompt, UTC six-hour buckets; dots are evidence, gaps are unknown.</text>',
           f'<text x="20" y="72">{html.escape(iso(since))} to {html.escape(iso(until))} [end exclusive]</text>']
    for i in range(6):
        value = maximum * i / 5
        svg.extend([f'<path d="M{left},{y(value):.2f} H{right}" stroke="#e5e7eb"/>',
                    f'<text x="5" y="{y(value)+4:.2f}">{value:.3g}</text>'])
    svg.append(f'<text x="5" y="92">{html.escape(unit)}</text>')
    day = (since // DAY_US + 1) * DAY_US
    # Every day gets a guide; labels thin out for long bounded ranges.
    stride = max(1, math.ceil((until - since) / DAY_US / 16))
    index = 0
    while day < until:
        svg.append(f'<path d="M{x(day):.2f},{top} V{bottom}" stroke="#e5e7eb"/>')
        if index % stride == 0:
            svg.append(f'<text x="{x(day):.2f}" y="484" text-anchor="middle">{iso(day)[5:10]}</text>')
        day += DAY_US
        index += 1
    for index, key in enumerate(series):
        color = COLORS[index % len(COLORS)]
        dash = ["", "6 3", "2 3"][index // len(COLORS) % 3]
        points = [r for r in rows if (r["provider"], r["model"]) == key and r[metric] != ""]
        previous = None
        for row in points:
            start, end = instant(row["bucket_start"]), instant(row["bucket_end"])
            bucket = start // BUCKET_US
            px, py = x((start + end) / 2), y(row[metric])
            if previous and bucket == previous[0] + 1:
                svg.append(f'<path d="M{previous[1]:.2f},{previous[2]:.2f} L{px:.2f},{py:.2f}" '
                           f'fill="none" stroke="{color}" stroke-width="2" stroke-dasharray="{dash}"/>')
            svg.append(f'<circle cx="{px:.2f}" cy="{py:.2f}" r="3" fill="{color}"/>')
            previous = bucket, px, py
        label = html.escape("/".join(key))
        svg.extend([f'<path d="M20,{520+index*22} h30" stroke="{color}" stroke-width="2" stroke-dasharray="{dash}"/>',
                    f'<text x="60" y="{524+index*22}">{label}</text>'])
    if not series:
        svg.append('<text x="400" y="280">No selected completed prompt evidence</text>')
    svg.append("</g></svg>\n")
    return "\n".join(svg)


def scan(args, since, until, counts):
    samples = []
    # Root-only snapshots avoid descendant double counting. Fix discovery once.
    agents = sorted(p for p in args.agents_dir.iterdir()
                    if p.is_dir() and (p / "events.cbor").is_file())
    counts["discovered_journals"] = len(agents)
    for agent in agents:
        # Private anonymous scratch, never a published trace. Discard stderr,
        # which may include private paths or IDs. Count every skipped journal.
        with tempfile.TemporaryFile(mode="w+t", encoding="utf-8") as trace:
            try:
                result = subprocess.run(
                    [args.tau, "agent", "trace", agent.name, "--agents-dir",
                     str(args.agents_dir), "--format", "agent-performance-jsonl"],
                    stdout=trace, stderr=subprocess.DEVNULL, timeout=args.timeout,
                    check=False)
            except subprocess.TimeoutExpired:
                counts["timed_out_journals"] += 1
                continue
            if result.returncode:
                counts["failed_journals"] += 1
                continue
            trace.seek(0)
            local_counts = collections.Counter()
            try:
                selected = read_trace(trace, since, until, local_counts)
            except (ValueError, KeyError, TypeError, StopIteration):
                counts["unsupported_or_malformed_traces"] += 1
                continue
            counts.update(local_counts)
            counts["successful_journals"] += 1
            samples.extend(selected)
    return samples


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    state = pathlib.Path(os.environ.get("XDG_STATE_HOME", pathlib.Path.home() / ".local/state"))
    parser.add_argument("--agents-dir", type=pathlib.Path, default=state / "tau/agents")
    parser.add_argument("--tau", default="tau", help="Tau executable with performance trace support")
    parser.add_argument("--since", help="inclusive RFC3339 instant; default until minus 14 days")
    parser.add_argument("--until", help="exclusive RFC3339 instant; default current UTC instant")
    parser.add_argument("--timeout", type=float, default=120, help="seconds per journal (default 120)")
    parser.add_argument("--out", type=pathlib.Path, required=True, help="new artifact directory")
    args = parser.parse_args(argv)
    try:
        until = instant(args.until) if args.until else instant(dt.datetime.now(UTC).isoformat())
        since = instant(args.since) if args.since else until - 14 * DAY_US
        if since < 0 or not 0 < until - since <= 366 * DAY_US:
            raise ValueError("range must be positive, after Unix epoch, and at most 366 days")
        if not math.isfinite(args.timeout) or args.timeout <= 0:
            raise ValueError("timeout must be positive and finite")
        if args.out.exists():
            raise ValueError("output directory must not already exist")
        counts = collections.Counter()
        samples = scan(args, since, until, counts)
    except (ValueError, OverflowError, OSError) as error:
        parser.exit(2, f"error: {error}\n")
    rows = aggregate(samples, since, until)
    args.out.mkdir(parents=True, mode=0o700)
    with (args.out / "performance.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, FIELDS)
        writer.writeheader()
        writer.writerows(rows)
    for metric, filename, title, unit in [
        ("median_wall_latency_seconds", "latency.svg", "Prompt-to-terminal wall latency", "seconds"),
        ("median_output_tokens_per_wall_second", "throughput.svg",
         "Output tokens / prompt-to-terminal wall time (not decoder speed)", "tokens/s"),
    ]:
        (args.out / filename).write_text(chart(rows, metric, title, unit, since, until))
    summary = [f"Range: [{iso(since)}, {iso(until)})", "Buckets: UTC six-hour; median per prompt",
               f"Provider/model series: {len({(r['provider'], r['model']) for r in rows})}",
               f"Bucket/model rows: {len(rows)}"]
    for key in ["discovered_journals", "successful_journals", "failed_journals",
                "timed_out_journals", "unsupported_or_malformed_traces", "prompt_rows",
                "duplicate_rows", "incomplete_prompts", "missing_terminal_time",
                "out_of_range", "missing_provider_model", "selected_completed",
                "missing_latency", "missing_rate"]:
        summary.append(f"{key}: {counts[key]}")
    summary.extend([
        "Incomplete/missing-time counts describe all scanned history, not only the selected range.",
        "Failed journals may be old/unsupported, corrupt, or busy without a checkpoint.",
        "Ordinary prompt rows only; compaction excluded. Prompt retries share the overall elapsed interval.",
        "Wall time includes harness/provider work and retries; not wire latency or pure token generation.",
        "No first-output timestamps in this projection. Missing evidence is not zero.",
        "Read performance.csv for completed/latency/rate sample counts per bucket and model.",
        "No prompts, responses, tool data, agent IDs, paths, or raw traces exported.",
    ])
    (args.out / "summary.txt").write_text("\n".join(summary) + "\n")
    (args.out / "README.md").write_text(
        "# Agent performance charts\n\n"
        "Keep performance.csv, latency.svg, throughput.svg and summary.txt together.\n"
        "Six-hour medians group exact recorded provider/model versions, with no alias inference.\n"
        "Partial boundary buckets contain only terminals in the selected half-open range.\n"
        "Latency runs from prompt materialization to canonical accepted response terminal.\n"
        "Output throughput divides response tokens by that same entire interval, including retries.\n"
        "It is **not decoder speed**. Missing usage/timing never becomes a zero sample.\n"
        "Zero output tokens with positive elapsed time is a real zero-rate sample.\n"
        "Dots sit at clipped bucket midpoints; lines stop at missing metric buckets.\n"
        "Only completed ordinary inference is charted; incomplete prompts and standalone compaction\n"
        "are excluded. Running journals use validated checkpoint prefixes, possibly lagging live work.\n"
        "Skipping old unsupported journals can bias historical comparisons; inspect summary counts.\n"
        "Artifacts omit IDs and content but still expose model names, timing and aggregate work patterns.\n"
        "Treat them as private; do not commit them or infer account identity from provider names.\n")
    print("\n".join(summary))
    print(f"Artifacts: {args.out}")


if __name__ == "__main__":
    main()
