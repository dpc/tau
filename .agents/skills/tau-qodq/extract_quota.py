#!/usr/bin/env python3
"""Extract canonical provider quota observations into redacted CSV/SVG/summary."""

import argparse
import csv
import datetime as dt
import html
import json
import math
import pathlib
import time

DAY_MS = 86_400_000
MAX_RANGE_MS = 366 * DAY_MS
EVENT = "harness.provider_quota_changed"
CSV_FIELDS = [
    "observed_at", "observed_at_unix_ms", "provider", "profile_epoch", "sequence",
    "limit_id", "window_id", "used_basis_points", "used_percent",
    "remaining_basis_points", "remaining_percent", "window_seconds", "reset_at",
    "reset_at_unix_seconds", "remaining_seconds_at_timing_anchor",
    "timing_anchor_observed_at_unix_ms", "server_offset_ms",
    "server_offset_observed_at_unix_ms", "route_models_json",
    "route_provenances_json", "route_observed_at_unix_ms_json",
    "omitted_unchanged_before",
]
UTC = dt.timezone.utc


class MalformedCanonical(ValueError):
    pass


def parse_instant(value):
    text = value.replace("Z", "+00:00")
    instant = dt.datetime.fromisoformat(text)
    if instant.tzinfo is None:
        instant = instant.replace(tzinfo=UTC)
    return instant.astimezone(UTC)


def unix_ms(instant):
    return int(instant.timestamp() * 1000)


def iso_from_unix(value, divisor):
    if isinstance(value, bool) or not isinstance(value, int):
        raise MalformedCanonical("timestamp must be an integer")
    try:
        instant = dt.datetime(1970, 1, 1, tzinfo=UTC) + dt.timedelta(seconds=value / divisor)
    except (OverflowError, ValueError) as error:
        raise MalformedCanonical("timestamp is outside the representable range") from error
    return instant.isoformat(timespec="milliseconds").replace("+00:00", "Z")


def require_dict(value, label):
    if not isinstance(value, dict):
        raise MalformedCanonical(f"{label} must be an object")
    return value


def require_list(value, label):
    if not isinstance(value, list):
        raise MalformedCanonical(f"{label} must be an array")
    return value


def require_string(value, label):
    if not isinstance(value, str) or not value:
        raise MalformedCanonical(f"{label} must be a nonempty string")
    return value


def optional_int(value, label):
    if value is None:
        return ""
    if isinstance(value, bool) or not isinstance(value, int):
        raise MalformedCanonical(f"{label} must be an integer or null")
    return value


def selected_event_files(root):
    if not root.exists() or not root.is_dir():
        raise ValueError(f"sessions root is not a readable directory: {root}")
    files = []
    try:
        sessions = sorted(root.iterdir(), key=lambda path: path.name)
    except OSError as error:
        raise ValueError(f"cannot enumerate sessions root {root}: {error}") from error
    for session in sessions:
        # is_dir follows an explicitly selected session-directory symlink.
        try:
            if not session.is_dir():
                continue
            candidate = session / "events.jsonl"
            if candidate.is_file():
                files.append(candidate)
        except OSError as error:
            raise ValueError(f"cannot inspect selected session {session}: {error}") from error
    return files


def route_evidence(bindings, limit_id):
    matches = []
    for binding in require_list(bindings, "route_bindings"):
        binding = require_dict(binding, "route binding")
        limits = require_list(binding.get("limit_ids"), "route limit_ids")
        if any(not isinstance(item, str) for item in limits):
            raise MalformedCanonical("route limit_ids must contain strings")
        model = require_string(binding.get("model"), "route model")
        provenance = binding.get("provenance", "")
        if not isinstance(provenance, str):
            raise MalformedCanonical("route provenance must be a string")
        observed = optional_int(binding.get("observed_at_unix_ms"), "route observed time")
        if observed != "":
            iso_from_unix(observed, 1000)
        if limit_id in limits:
            matches.append((model, provenance, observed))
    matches.sort()
    return (
        json.dumps([item[0] for item in matches], separators=(",", ":")),
        json.dumps([item[1] for item in matches], separators=(",", ":")),
        json.dumps([item[2] for item in matches], separators=(",", ":")),
    )


def rows_from_record(record, provider, lower_ms, upper_ms):
    record = require_dict(record, "record")
    event = require_dict(record.get("event"), "event")
    if event.get("event") != EVENT:
        return None
    if record.get("type") != "published":
        raise MalformedCanonical("canonical record type must be published")
    recorded = optional_int(record.get("recorded_at_micros"), "recorded_at_micros")
    if recorded == "":
        raise MalformedCanonical("recorded_at_micros is required")
    iso_from_unix(recorded, 1_000_000)
    payload = require_dict(event.get("payload"), "payload")
    event_provider = require_string(payload.get("provider"), "provider")
    if event_provider != provider:
        return False
    epoch = require_string(payload.get("profile_epoch"), "profile_epoch")
    sequence = optional_int(payload.get("sequence"), "sequence")
    if sequence == "":
        raise MalformedCanonical("sequence is required")
    bindings = require_list(payload.get("route_bindings"), "route_bindings")
    windows = require_list(payload.get("windows"), "windows")
    result = []
    for window in windows:
        window = require_dict(window, "window")
        key = require_dict(window.get("key"), "window key")
        limit_id = require_string(key.get("limit_id"), "limit_id")
        window_id = require_string(key.get("window_id"), "window_id")
        observed = optional_int(window.get("usage_observed_at_unix_ms"), "usage observation")
        if observed == "":
            raise MalformedCanonical("usage observation is required")
        observed_at = iso_from_unix(observed, 1000)
        used = optional_int(window.get("used_basis_points"), "used_basis_points")
        duration = optional_int(window.get("window_seconds"), "window_seconds")
        if used == "" or not (0 <= used <= 10_000) or duration == "" or duration < 0:
            raise MalformedCanonical("invalid quota usage or window duration")
        reset = optional_int(window.get("reset_at_unix_seconds"), "reset time")
        reset_at = iso_from_unix(reset, 1) if reset != "" else ""
        anchor = optional_int(window.get("timing_anchor_observed_at_unix_ms"), "timing anchor")
        offset_at = optional_int(window.get("server_offset_observed_at_unix_ms"), "offset observation")
        if anchor != "":
            iso_from_unix(anchor, 1000)
        if offset_at != "":
            iso_from_unix(offset_at, 1000)
        models, provenances, route_times = route_evidence(bindings, limit_id)
        if observed < lower_ms or not observed < upper_ms:
            continue
        result.append({
            "observed_at": observed_at, "observed_at_unix_ms": observed,
            "provider": event_provider, "profile_epoch": epoch, "sequence": sequence,
            "limit_id": limit_id, "window_id": window_id,
            "used_basis_points": used, "used_percent": used / 100,
            "remaining_basis_points": 10_000 - used, "remaining_percent": 100 - used / 100,
            "window_seconds": duration, "reset_at": reset_at,
            "reset_at_unix_seconds": reset,
            "remaining_seconds_at_timing_anchor": optional_int(window.get("remaining_seconds_at_timing_anchor"), "remaining seconds"),
            "timing_anchor_observed_at_unix_ms": anchor,
            "server_offset_ms": optional_int(window.get("server_offset_ms"), "server offset"),
            "server_offset_observed_at_unix_ms": offset_at,
            "route_models_json": models, "route_provenances_json": provenances,
            "route_observed_at_unix_ms_json": route_times,
            "omitted_unchanged_before": 0,
        })
    return result


def evidence_signature(row):
    excluded = {"observed_at", "observed_at_unix_ms", "sequence", "omitted_unchanged_before"}
    return tuple((field, row[field]) for field in CSV_FIELDS if field not in excluded)


def collapse_group(rows):
    rows.sort(key=lambda row: (row["observed_at_unix_ms"], row["sequence"]))
    kept = []
    index = 0
    while index < len(rows):
        end = index + 1
        signature = evidence_signature(rows[index])
        while end < len(rows) and evidence_signature(rows[end]) == signature:
            end += 1
        run = rows[index:end]
        kept.append(run[0].copy())
        if 1 < len(run):
            latest = run[-1].copy()
            latest["omitted_unchanged_before"] = max(0, len(run) - 2)
            kept.append(latest)
        index = end
    return kept


def write_csv(path, rows):
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, CSV_FIELDS)
        writer.writeheader()
        writer.writerows(rows)


def render_svg(path, rows, provider, lower_ms, upper_ms):
    width, height, left, right, top, bottom = 1200, 520, 70, 25, 35, 65
    span = max(1, upper_ms - lower_ms)
    x = lambda value: left + (value - lower_ms) * (width - left - right) / span
    y = lambda value: top + (100 - value) * (height - top - bottom) / 100
    colors = ["#2563eb", "#dc2626", "#059669", "#7c3aed", "#ea580c", "#0891b2"]
    groups = {}
    for row in rows:
        key = (row["provider"], row["profile_epoch"], row["limit_id"], row["window_id"])
        groups.setdefault(key, []).append(row)
    svg = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">', '<rect width="100%" height="100%" fill="white"/>', '<style>text{font:13px monospace}.grid{stroke:#ddd}</style>', f'<text x="{left}" y="20" font-weight="bold">{html.escape(provider)} quota remaining — canonical observations</text>']
    for percent in range(0, 101, 20):
        svg += [f'<line class="grid" x1="{left}" x2="{width-right}" y1="{y(percent)}" y2="{y(percent)}"/>', f'<text x="8" y="{y(percent)+4}">{percent}%</text>']
    midnight = ((lower_ms // DAY_MS) + 1) * DAY_MS
    while midnight < upper_ms:
        svg.append(f'<line class="grid" stroke-dasharray="3 3" x1="{x(midnight):.1f}" x2="{x(midnight):.1f}" y1="{top}" y2="{height-bottom}"/>')
        midnight += DAY_MS
    legend = {}
    plotted = set()
    for key, samples in sorted(groups.items()):
        label = f"{key[2]}/{key[3]}"
        color = legend.setdefault(label, colors[len(legend) % len(colors)])
        for row in samples:
            point = (color, round(x(row["observed_at_unix_ms"]), 1), round(y(row["remaining_percent"]), 1))
            if point in plotted:
                continue
            plotted.add(point)
            svg.append(f'<circle fill="{color}" cx="{point[1]:.1f}" cy="{point[2]:.1f}" r="2.5"/>')
    for index, (label, color) in enumerate(legend.items()):
        svg.append(f'<text x="{left+10+(index%3)*360}" y="{height-38+(index//3)*16}" fill="{color}">{html.escape(label)}</text>')
    start_label = iso_from_unix(lower_ms, 1000)[:16].replace("T", " ") + "Z"
    end_label = iso_from_unix(upper_ms, 1000)[:16].replace("T", " ") + "Z"
    svg += [f'<text x="{left}" y="{height-8}">{start_label}</text>', f'<text text-anchor="end" x="{width-right}" y="{height-8}">{end_label}</text>', "</svg>"]
    path.write_text("\n".join(svg))
    return len(plotted)


def scan(files, provider, lower_ms, upper_ms):
    groups = {}
    counters = {"files": len(files), "bytes": 0, "candidate_lines": 0, "canonical_events": 0, "malformed_canonical": 0, "window_observations": 0}
    sessions = set()
    for path in files:
        try:
            counters["bytes"] += path.stat().st_size
            with path.open(errors="replace") as log:
                for raw in log:
                    if EVENT not in raw:
                        continue
                    counters["candidate_lines"] += 1
                    try:
                        record = json.loads(raw)
                        extracted = rows_from_record(record, provider, lower_ms, upper_ms)
                    except (json.JSONDecodeError, MalformedCanonical, TypeError, OverflowError, ValueError):
                        counters["malformed_canonical"] += 1
                        continue
                    if extracted is None:
                        continue
                    if extracted is False:
                        continue
                    counters["canonical_events"] += 1
                    if extracted:
                        sessions.add(path.parent.name)
                    for row in extracted:
                        counters["window_observations"] += 1
                        key = (row["provider"], row["profile_epoch"], row["limit_id"], row["window_id"])
                        groups.setdefault(key, []).append(row)
        except OSError as error:
            raise ValueError(f"failed to scan selected canonical file {path}: {error}") from error
    counters["sessions_with_observations"] = len(sessions)
    rows = []
    for group in groups.values():
        rows.extend(collapse_group(group))
    rows.sort(key=lambda row: (row["observed_at_unix_ms"], row["provider"], row["profile_epoch"], row["limit_id"], row["window_id"], row["sequence"]))
    counters["emitted_rows"] = len(rows)
    counters["omitted_unchanged_rows"] = sum(row["omitted_unchanged_before"] for row in rows)
    return rows, counters


def build_parser():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sessions-root", default=str(pathlib.Path.home() / ".local/state/tau/sessions"))
    parser.add_argument("--provider", default="chatgpt")
    parser.add_argument("--since", help="inclusive RFC3339 lower bound; default now minus 14 days")
    parser.add_argument("--until", help="exclusive RFC3339 upper bound; default now")
    parser.add_argument("--out", required=True)
    return parser


def main(argv=None, now=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    clock = now or dt.datetime.now(UTC)
    try:
        upper = parse_instant(args.until) if args.until else clock
        lower = parse_instant(args.since) if args.since else upper - dt.timedelta(days=14)
        lower_ms, upper_ms = unix_ms(lower), unix_ms(upper)
        if not lower_ms < upper_ms:
            parser.error("--since must be before --until")
        if MAX_RANGE_MS < upper_ms - lower_ms:
            parser.error("time range must not exceed 366 days")
        files = selected_event_files(pathlib.Path(args.sessions_root))
    except (ValueError, OverflowError) as error:
        parser.error(str(error))
    output = pathlib.Path(args.out)
    output.mkdir(parents=True, exist_ok=True)
    started = time.perf_counter()
    try:
        rows, counters = scan(files, args.provider, lower_ms, upper_ms)
        write_csv(output / "quota.csv", rows)
        scatter_points = render_svg(output / "quota.svg", rows, args.provider, lower_ms, upper_ms)
    except ValueError as error:
        parser.error(str(error))
    elapsed = time.perf_counter() - started
    lines = [
        f"selected canonical files: {counters['files']}", f"selected file bytes: {counters['bytes']}",
        f"canonical candidate lines: {counters['candidate_lines']}", f"validated canonical events: {counters['canonical_events']}",
        f"malformed canonical candidates: {counters['malformed_canonical']}", f"sessions with selected observations: {counters['sessions_with_observations']}",
        f"window observations in range: {counters['window_observations']}", f"retained CSV evidence rows: {counters['emitted_rows']}",
        f"omitted unchanged rows: {counters['omitted_unchanged_rows']}", f"rendered scatter points after pixel deduplication: {scatter_points}", f"elapsed seconds: {elapsed:.3f}",
        f"time filter: [{iso_from_unix(lower_ms, 1000)}, {iso_from_unix(upper_ms, 1000)})", "latest observations by provider/profile epoch/window:",
    ]
    latest = {}
    for row in rows:
        latest[(row["provider"], row["profile_epoch"], row["limit_id"], row["window_id"])] = row
    for key, row in sorted(latest.items()):
        lines.append(f"- {'/'.join(key)}: {row['remaining_percent']}% remaining at {row['observed_at']}; reset {row['reset_at'] or 'unknown'}")
    text = "\n".join(lines) + "\n"
    (output / "summary.txt").write_text(text)
    print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
