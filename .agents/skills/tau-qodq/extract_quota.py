#!/usr/bin/env python3
"""Extract redacted quota and token-usage evidence from canonical events."""

import argparse
import csv
import datetime as dt
import html
import json
import math
import pathlib
import time

DAY_MS = 86_400_000
HOUR_MS = 3_600_000
SIX_HOURS_MS = 6 * HOUR_MS
MAX_RANGE_MS = 366 * DAY_MS
EVENT = "harness.provider_quota_changed"
TOKEN_EVENT = "provider.response_finished"
UTC = dt.timezone.utc
COLORS = ["#2563eb", "#dc2626", "#059669", "#7c3aed", "#ea580c", "#0891b2"]
DEFAULT_QUOTA_SERIES = ("codex", "primary")
QUOTA_CSV_FIELDS = [
    "observed_at", "observed_at_unix_ms", "profile_label", "provider", "profile_epoch",
    "sequence", "limit_id", "window_id", "used_basis_points", "used_percent",
    "remaining_basis_points", "remaining_percent", "window_seconds", "reset_at",
    "reset_at_unix_seconds", "remaining_seconds_at_timing_anchor",
    "timing_anchor_observed_at_unix_ms", "server_offset_ms",
    "server_offset_observed_at_unix_ms", "route_models_json",
    "route_provenances_json", "route_observed_at_unix_ms_json",
    "omitted_unchanged_before",
]
# Kept as an alias for callers of the previous one-chart extractor.
CSV_FIELDS = QUOTA_CSV_FIELDS
TOKEN_CSV_FIELDS = [
    "hour_start", "hour_start_unix_ms", "hour_end", "profile_label", "provider",
    "cached_input_tokens", "uncached_input_tokens", "output_tokens",
    "cached_input_tokens_per_second", "uncached_input_tokens_per_second",
    "output_tokens_per_second", "accepted_terminal_observations",
]


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


def require_int(value, label):
    if isinstance(value, bool) or not isinstance(value, int):
        raise MalformedCanonical(f"{label} must be an integer")
    return value


def optional_int(value, label):
    if value is None:
        return ""
    return require_int(value, label)


def skip_json_whitespace(text, index):
    while index < len(text) and text[index] in " \t\r\n":
        index += 1
    return index


def skip_json_string(text, index):
    if index >= len(text) or text[index] != '"':
        raise MalformedCanonical("expected JSON string")
    index += 1
    while index < len(text):
        character = text[index]
        if character == '"':
            return index + 1
        if ord(character) < 0x20:
            raise MalformedCanonical("control character in JSON string")
        if character == "\\":
            index += 1
            if index >= len(text):
                raise MalformedCanonical("unterminated JSON escape")
            escape = text[index]
            if escape == "u":
                digits = text[index + 1:index + 5]
                if len(digits) != 4 or any(digit not in "0123456789abcdefABCDEF" for digit in digits):
                    raise MalformedCanonical("invalid JSON unicode escape")
                index += 4
            elif escape not in '"\\/bfnrt':
                raise MalformedCanonical("invalid JSON escape")
        index += 1
    raise MalformedCanonical("unterminated JSON string")


def skip_json_value(text, index):
    """Validate and skip one JSON value without decoding its strings or objects."""
    index = skip_json_whitespace(text, index)
    if index >= len(text):
        raise MalformedCanonical("missing JSON value")
    character = text[index]
    if character == '"':
        return skip_json_string(text, index)
    if character == "{":
        index += 1
        first_member = True
        while True:
            index = skip_json_whitespace(text, index)
            if index < len(text) and text[index] == "}":
                if first_member:
                    return index + 1
                raise MalformedCanonical("trailing JSON object comma")
            index = skip_json_string(text, index)
            first_member = False
            index = skip_json_whitespace(text, index)
            if index >= len(text) or text[index] != ":":
                raise MalformedCanonical("missing JSON object colon")
            index = skip_json_value(text, index + 1)
            index = skip_json_whitespace(text, index)
            if index >= len(text) or text[index] not in ",}":
                raise MalformedCanonical("missing JSON object separator")
            if text[index] == "}":
                return index + 1
            index += 1
    if character == "[":
        index += 1
        first_value = True
        while True:
            index = skip_json_whitespace(text, index)
            if index < len(text) and text[index] == "]":
                if first_value:
                    return index + 1
                raise MalformedCanonical("trailing JSON array comma")
            index = skip_json_value(text, index)
            first_value = False
            index = skip_json_whitespace(text, index)
            if index >= len(text) or text[index] not in ",]":
                raise MalformedCanonical("missing JSON array separator")
            if text[index] == "]":
                return index + 1
            index += 1
    end = index
    while end < len(text) and text[end] not in ",]} \t\r\n":
        end += 1
    try:
        json.loads(text[index:end])
    except json.JSONDecodeError as error:
        raise MalformedCanonical("invalid JSON scalar") from error
    return end


def projected_object_fields(text, index, wanted):
    """Return spans for selected object fields while skipping all other values."""
    index = skip_json_whitespace(text, index)
    if index >= len(text) or text[index] != "{":
        raise MalformedCanonical("expected JSON object")
    index += 1
    fields = {}
    first_member = True
    while True:
        index = skip_json_whitespace(text, index)
        if index < len(text) and text[index] == "}":
            if first_member:
                return fields, index + 1
            raise MalformedCanonical("trailing JSON object comma")
        key_start = index
        index = skip_json_string(text, index)
        first_member = False
        try:
            key = json.loads(text[key_start:index])
        except json.JSONDecodeError as error:
            raise MalformedCanonical("invalid JSON object key") from error
        index = skip_json_whitespace(text, index)
        if index >= len(text) or text[index] != ":":
            raise MalformedCanonical("missing JSON object colon")
        value_start = index + 1
        value_end = skip_json_value(text, value_start)
        if key in wanted:
            fields[key] = (value_start, value_end)
        index = value_end
        index = skip_json_whitespace(text, index)
        if index >= len(text) or text[index] not in ",}":
            raise MalformedCanonical("missing JSON object separator")
        if text[index] == "}":
            return fields, index + 1
        index += 1


def projected_value(text, fields, name):
    try:
        start, end = fields[name]
    except KeyError as error:
        raise MalformedCanonical(f"missing projected {name}") from error
    try:
        return json.loads(text[start:end])
    except json.JSONDecodeError as error:
        raise MalformedCanonical(f"invalid projected {name}") from error


def projected_canonical_record(raw):
    """Decode only canonical quota fields or terminal usage fields from one line."""
    record, end = projected_object_fields(raw, 0, {"type", "recorded_at_micros", "event"})
    if skip_json_whitespace(raw, end) != len(raw):
        raise MalformedCanonical("trailing JSON data")
    event_start, _ = record.get("event", (None, None))
    if event_start is None:
        raise MalformedCanonical("missing event")
    event, _ = projected_object_fields(raw, event_start, {"event", "payload"})
    if "event" not in event:
        return None, None
    name = projected_value(raw, event, "event")
    if name not in (EVENT, TOKEN_EVENT):
        return name, None
    result = {
        "type": projected_value(raw, record, "type"),
        "recorded_at_micros": projected_value(raw, record, "recorded_at_micros"),
        "event": {"event": name},
    }
    if name == EVENT:
        payload_start, payload_end = event.get("payload", (None, None))
        if payload_start is None:
            raise MalformedCanonical("missing quota payload")
        try:
            result["event"]["payload"] = json.loads(raw[payload_start:payload_end])
        except json.JSONDecodeError as error:
            raise MalformedCanonical("invalid quota payload") from error
    elif name == TOKEN_EVENT:
        payload_start, _ = event.get("payload", (None, None))
        if payload_start is None:
            raise MalformedCanonical("missing token payload")
        payload, _ = projected_object_fields(
            raw, payload_start, {"agent_id", "agent_prompt_id", "provider_attempt", "usage"}
        )
        selected = {
            name: projected_value(raw, payload, name)
            for name in ("agent_id", "agent_prompt_id")
            if name in payload
        }
        if "provider_attempt" in payload:
            selected["provider_attempt"] = projected_value(raw, payload, "provider_attempt")
        if "usage" in payload:
            usage_start, _ = payload["usage"]
            usage, _ = projected_object_fields(
                raw, usage_start,
                {"model", "prompt_sent_tokens", "prompt_cached_tokens", "response_received_tokens"},
            )
            selected["usage"] = {
                name: projected_value(raw, usage, name)
                for name in (
                    "model", "prompt_sent_tokens", "prompt_cached_tokens", "response_received_tokens"
                )
                if name in usage
            }
        result["event"]["payload"] = selected
    return name, result


def selected_event_files(root):
    if not root.exists() or not root.is_dir():
        raise ValueError(f"sessions root is not a readable directory: {root}")
    files = []
    try:
        sessions = sorted(root.iterdir(), key=lambda path: path.name)
    except OSError as error:
        raise ValueError(f"cannot enumerate sessions root {root}: {error}") from error
    for session in sessions:
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


def rows_from_record(record, profile_label, provider, lower_ms, upper_ms):
    """Read only a selected canonical quota snapshot; return in-range windows."""
    record = require_dict(record, "record")
    event = require_dict(record.get("event"), "event")
    if event.get("event") != EVENT:
        return None
    if record.get("type") != "published":
        raise MalformedCanonical("canonical record type must be published")
    recorded = require_int(record.get("recorded_at_micros"), "recorded_at_micros")
    iso_from_unix(recorded, 1_000_000)
    payload = require_dict(event.get("payload"), "payload")
    event_provider = require_string(payload.get("provider"), "provider")
    if event_provider != provider:
        return False
    epoch = require_string(payload.get("profile_epoch"), "profile_epoch")
    sequence = require_int(payload.get("sequence"), "sequence")
    bindings = require_list(payload.get("route_bindings"), "route_bindings")
    windows = require_list(payload.get("windows"), "windows")
    result = []
    for window in windows:
        window = require_dict(window, "window")
        key = require_dict(window.get("key"), "window key")
        limit_id = require_string(key.get("limit_id"), "limit_id")
        window_id = require_string(key.get("window_id"), "window_id")
        observed = require_int(window.get("usage_observed_at_unix_ms"), "usage observation")
        observed_at = iso_from_unix(observed, 1000)
        used = require_int(window.get("used_basis_points"), "used_basis_points")
        duration = require_int(window.get("window_seconds"), "window_seconds")
        if not (0 <= used <= 10_000) or duration < 0:
            raise MalformedCanonical("invalid quota usage or window duration")
        reset = optional_int(window.get("reset_at_unix_seconds"), "reset time")
        reset_at = iso_from_unix(reset, 1) if reset != "" else ""
        anchor = optional_int(window.get("timing_anchor_observed_at_unix_ms"), "timing anchor")
        offset_at = optional_int(
            window.get("server_offset_observed_at_unix_ms"), "offset observation"
        )
        if anchor != "":
            iso_from_unix(anchor, 1000)
        if offset_at != "":
            iso_from_unix(offset_at, 1000)
        models, provenances, route_times = route_evidence(bindings, limit_id)
        if observed < lower_ms or not observed < upper_ms:
            continue
        result.append({
            "observed_at": observed_at, "observed_at_unix_ms": observed,
            "profile_label": profile_label, "provider": event_provider, "profile_epoch": epoch,
            "sequence": sequence, "limit_id": limit_id, "window_id": window_id,
            "used_basis_points": used, "used_percent": used / 100,
            "remaining_basis_points": 10_000 - used, "remaining_percent": 100 - used / 100,
            "window_seconds": duration, "reset_at": reset_at, "reset_at_unix_seconds": reset,
            "remaining_seconds_at_timing_anchor": optional_int(
                window.get("remaining_seconds_at_timing_anchor"), "remaining seconds"
            ),
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
    return tuple((field, row[field]) for field in QUOTA_CSV_FIELDS if field not in excluded)


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
            latest["omitted_unchanged_before"] = len(run) - 2
            kept.append(latest)
        index = end
    return kept


def profile_for_model(model, profiles):
    for label, provider in profiles:
        if model.startswith(f"{provider}/"):
            return label, provider
    return None


def token_observation(record, profiles):
    """Read canonical terminal usage without reading output items or other payload fields."""
    record = require_dict(record, "record")
    event = require_dict(record.get("event"), "event")
    if event.get("event") != TOKEN_EVENT:
        return None
    if record.get("type") != "published":
        raise MalformedCanonical("canonical record type must be published")
    recorded_micros = require_int(record.get("recorded_at_micros"), "recorded_at_micros")
    recorded_ms = recorded_micros // 1000
    iso_from_unix(recorded_ms, 1000)
    payload = require_dict(event.get("payload"), "payload")
    usage = payload.get("usage")
    if usage is None:
        return "missing_usage"
    usage = require_dict(usage, "usage")
    model = usage.get("model")
    if model is None:
        return "unselected_model"
    model = require_string(model, "usage model")
    profile = profile_for_model(model, profiles)
    if profile is None:
        return "unselected_model"
    agent_id = require_string(payload.get("agent_id"), "agent id")
    prompt_id = require_string(payload.get("agent_prompt_id"), "agent prompt id")
    attempt = payload.get("provider_attempt", 1)
    if not isinstance(attempt, int) or isinstance(attempt, bool) or attempt < 1:
        raise MalformedCanonical("provider attempt must be a positive integer")
    label, provider = profile
    observation = {
        "profile_label": label, "provider": provider,
        "recorded_at_micros": recorded_micros, "recorded_at_unix_ms": recorded_ms,
        "identity": (label, agent_id, prompt_id, attempt), "complete_usage": False,
    }
    # The current canonical schema serializes this non-optional field. Older
    # records without it remain identity evidence but not cached-input evidence.
    if "prompt_cached_tokens" not in usage:
        return observation
    prompt = require_int(usage.get("prompt_sent_tokens"), "prompt sent tokens")
    cached = require_int(usage.get("prompt_cached_tokens"), "prompt cached tokens")
    output = require_int(usage.get("response_received_tokens"), "response received tokens")
    if prompt < 0 or cached < 0 or output < 0 or cached > prompt:
        raise MalformedCanonical("invalid token usage")
    return {
        **observation, "complete_usage": True, "cached_input_tokens": cached,
        "uncached_input_tokens": prompt - cached, "output_tokens": output,
    }


def write_csv(path, rows, fields=QUOTA_CSV_FIELDS):
    with path.open("w", newline="") as output:
        writer = csv.DictWriter(output, fields)
        writer.writeheader()
        writer.writerows(rows)


def day_start(value):
    return value // DAY_MS * DAY_MS


def day_label(value):
    return dt.datetime.fromtimestamp(value / 1000, UTC).strftime("%m-%d")


def svg_frame(title, y_label, lower_ms, upper_ms, y_ticks, y_position):
    width, left, right, top = 1200, 130, 25, 74
    height, bottom = 560, 58
    span = max(1, upper_ms - lower_ms)
    x = lambda value: left + (value - lower_ms) * (width - left - right) / span
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">',
        '<rect width="100%" height="100%" fill="white"/>',
        '<style>text{font:13px monospace}.grid{stroke:#ddd}.axis{fill:#222}.series{fill:none;stroke-width:2.5;stroke-linejoin:round;stroke-linecap:round}</style>',
        f'<text x="{left}" y="22" font-weight="bold">{html.escape(title)}</text>',
        f'<text class="axis" text-anchor="middle" x="22" y="{(top + height - bottom) / 2:.1f}" transform="rotate(-90 22 {(top + height - bottom) / 2:.1f})">{html.escape(y_label)}</text>',
    ]
    for label, value in y_ticks:
        y = y_position(value)
        svg += [
            f'<line class="grid" x1="{left}" x2="{width-right}" y1="{y:.1f}" y2="{y:.1f}"/>',
            f'<text text-anchor="end" x="{left - 12}" y="{y + 4:.1f}">{html.escape(label)}</text>',
        ]
    midnight = day_start(lower_ms)
    while midnight < upper_ms:
        guide = max(midnight, lower_ms)
        day_end = min(midnight + DAY_MS, upper_ms)
        svg.append(
            f'<line class="grid" stroke-dasharray="3 3" x1="{x(guide):.1f}" '
            f'x2="{x(guide):.1f}" y1="{top}" y2="{height-bottom}"/>'
        )
        svg.append(
            f'<text text-anchor="middle" x="{x((guide + day_end) // 2):.1f}" '
            f'y="{height - 14}">{day_label(midnight)}</text>'
        )
        midnight += DAY_MS
    return svg, x, width, height, left, right, top, bottom


def append_legend(svg, title, entries, x, y):
    cursor = x
    if title:
        svg.append(f'<text x="{x}" y="{y}" font-weight="bold">{html.escape(title)}</text>')
        cursor += len(title) * 8 + 16
    for label, color, dash in entries:
        svg.append(
            f'<line class="series" stroke="{color}" stroke-dasharray="{dash}" '
            f'x1="{cursor}" x2="{cursor + 24}" y1="{y - 4}" y2="{y - 4}"/>'
        )
        cursor += 31
        svg.append(f'<text x="{cursor}" y="{y}">{html.escape(label)}</text>')
        cursor += len(label) * 8 + 20


def append_daily_series(svg, rows, time_field, interval_ms, x_value, x_interval, y_value, color, dash):
    """Append real-observation paths, splitting at every missing display interval."""
    previous_time = None
    points = []
    rendered = 0

    def append_run():
        if len(points) == 1:
            start, end = x_interval(rows[run_start])
            _, y = points[0]
            svg.append(
                f'<line class="series" stroke="{color}" stroke-dasharray="{dash}" '
                f'x1="{start:.1f}" x2="{end:.1f}" y1="{y:.1f}" y2="{y:.1f}"/>'
            )
        elif points:
            path = " ".join(
                [f"M{points[0][0]:.1f},{points[0][1]:.1f}"]
                + [f"L{x:.1f},{y:.1f}" for x, y in points[1:]]
            )
            svg.append(f'<path class="series" stroke="{color}" stroke-dasharray="{dash}" d="{path}"/>')

    run_start = 0
    for index, row in enumerate(rows):
        timestamp = row[time_field]
        if previous_time is None or timestamp != previous_time + interval_ms:
            append_run()
            run_start = index
            points = [(x_value(row), y_value(row))]
        else:
            points.append((x_value(row), y_value(row)))
        previous_time = timestamp
        rendered += 1
    append_run()
    return rendered


def quota_display_rows(rows):
    """Select the highest actual default-pool remainder per subscription and UTC hour."""
    candidates = {}
    for row in rows:
        if (row["limit_id"], row["window_id"]) != DEFAULT_QUOTA_SERIES:
            continue
        key = (row["profile_label"], row["provider"], row["observed_at_unix_ms"] // HOUR_MS * HOUR_MS)
        previous = candidates.get(key)
        if previous is None or (
            row["remaining_percent"], row["observed_at_unix_ms"], row["sequence"], row["profile_epoch"]
        ) > (
            previous["remaining_percent"], previous["observed_at_unix_ms"],
            previous["sequence"], previous["profile_epoch"],
        ):
            candidates[key] = row
    return [
        {**row, "display_hour_start_unix_ms": hour}
        for (label, provider, hour), row in sorted(candidates.items())
    ]


def render_svg(path, rows, profiles, lower_ms, upper_ms):
    """Render hourly maximum default-pool remainder; CSV retains all evidence."""
    colors = {label: COLORS[index % len(COLORS)] for index, (label, _) in enumerate(profiles)}
    groups = {}
    for row in quota_display_rows(rows):
        groups.setdefault(row["profile_label"], []).append(row)
    y = lambda value: 74 + (100 - value) * (560 - 74 - 58) / 100
    svg, x, width, height, left, right, top, bottom = svg_frame(
        "Quota remaining", "Remaining (%)", lower_ms, upper_ms,
        [(f"{percent}%", percent) for percent in range(0, 101, 20)], y,
    )
    append_legend(svg, "", [(label, colors[label], "") for label, _ in profiles], 730, 22)
    rendered = 0
    for label, _ in profiles:
        rendered += append_daily_series(
            svg, groups.get(label, []), "display_hour_start_unix_ms", HOUR_MS,
            lambda row: x(row["display_hour_start_unix_ms"] + HOUR_MS // 2),
            lambda row: (
                x(max(lower_ms, row["display_hour_start_unix_ms"])),
                x(min(upper_ms, row["display_hour_start_unix_ms"] + HOUR_MS)),
            ),
            lambda row: y(row["remaining_percent"]),
            colors[label], "",
        )
    svg.append("</svg>")
    path.write_text("\n".join(svg))
    return rendered


def log1p_ticks(maximum):
    """Return actual-rate labels for a log1p scale whose zero remains at the baseline."""
    if maximum <= 0:
        return [("0", 0)], 1
    first_exponent = min(0, math.floor(math.log10(maximum)))
    ticks = [("0", 0)] + [
        (f"{10 ** exponent:g}" if exponent < 3 else f"{10 ** (exponent - 3):g}k", 10 ** exponent)
        for exponent in range(first_exponent, math.floor(math.log10(maximum)) + 1)
    ]
    if ticks[-1][1] != maximum:
        ticks.append((f"{maximum:g}" if maximum < 1000 else f"{maximum / 1000:g}k", maximum))
    return ticks, maximum


def aggregate_token_six_hour_buckets(rows):
    """Reduce hourly CSV evidence to UTC-aligned six-hour presentation buckets."""
    buckets = {}
    for row in rows:
        bucket_start = row["hour_start_unix_ms"] // SIX_HOURS_MS * SIX_HOURS_MS
        key = (bucket_start, row["profile_label"], row["provider"])
        bucket = buckets.setdefault(key, {
            "cached_input_tokens": 0, "uncached_input_tokens": 0, "output_tokens": 0,
            "accepted_terminal_observations": 0,
        })
        for field in ("cached_input_tokens", "uncached_input_tokens", "output_tokens"):
            bucket[field] += row[field]
        bucket["accepted_terminal_observations"] += row["accepted_terminal_observations"]
    return [
        {
            "bucket_start_unix_ms": bucket_start,
            "profile_label": label,
            "provider": provider,
            **bucket,
            "cached_input_tokens_per_second": bucket["cached_input_tokens"] / 21_600,
            "uncached_input_tokens_per_second": bucket["uncached_input_tokens"] / 21_600,
            "output_tokens_per_second": bucket["output_tokens"] / 21_600,
        }
        for (bucket_start, label, provider), bucket in sorted(buckets.items())
    ]


def render_token_svg(path, rows, profiles, lower_ms, upper_ms):
    colors = {label: COLORS[index % len(COLORS)] for index, (label, _) in enumerate(profiles)}
    categories = [
        ("cached_input_tokens_per_second", "Cache hits", ""),
        ("uncached_input_tokens_per_second", "Cache misses", "8 5"),
        ("output_tokens_per_second", "Output tokens", "2 4"),
    ]
    maximum = max((row[field] for row in rows for field, _, _ in categories), default=0)
    ticks, ceiling = log1p_ticks(maximum)
    y = lambda value: 74 + (math.log1p(ceiling) - math.log1p(value)) * (560 - 74 - 58) / math.log1p(ceiling)
    svg, x, _, _, _, _, _, _ = svg_frame(
        "Accepted token usage", "six-hour total / 21,600 tokens/s (log1p scale; 0 preserved)",
        lower_ms, upper_ms, ticks, y,
    )
    append_legend(svg, "subscription:", [(label, colors[label], "") for label, _ in profiles], 600, 22)
    append_legend(svg, "metric:", [(label, "#222", dash) for _, label, dash in categories], 600, 46)
    groups = {}
    for row in rows:
        groups.setdefault(row["profile_label"], []).append(row)
    rendered = 0
    for field, _, dash in categories:
        for subscription, _ in profiles:
            rendered += append_daily_series(
                svg, groups.get(subscription, []), "bucket_start_unix_ms", SIX_HOURS_MS,
                lambda row: x(row["bucket_start_unix_ms"] + SIX_HOURS_MS // 2),
                lambda row: (
                    x(max(lower_ms, row["bucket_start_unix_ms"])),
                    x(min(upper_ms, row["bucket_start_unix_ms"] + SIX_HOURS_MS)),
                ),
                lambda row, field=field, y=y: y(row[field]), colors[subscription], dash,
            )
    svg.append("</svg>")
    path.write_text("\n".join(svg))
    return rendered


def aggregate_tokens(observations):
    windows = {}
    for observation in observations:
        hour = observation["recorded_at_unix_ms"] // HOUR_MS * HOUR_MS
        key = (hour, observation["profile_label"], observation["provider"])
        bucket = windows.setdefault(key, {
            "cached_input_tokens": 0, "uncached_input_tokens": 0, "output_tokens": 0,
            "accepted_terminal_observations": 0,
        })
        for field in ("cached_input_tokens", "uncached_input_tokens", "output_tokens"):
            bucket[field] += observation[field]
        bucket["accepted_terminal_observations"] += 1
    rows = []
    for (hour, label, provider), bucket in sorted(windows.items()):
        rows.append({
            "hour_start": iso_from_unix(hour, 1000), "hour_start_unix_ms": hour,
            "hour_end": iso_from_unix(hour + HOUR_MS, 1000),
            "profile_label": label, "provider": provider, **bucket,
            "cached_input_tokens_per_second": bucket["cached_input_tokens"] / 3600,
            "uncached_input_tokens_per_second": bucket["uncached_input_tokens"] / 3600,
            "output_tokens_per_second": bucket["output_tokens"] / 3600,
        })
    return rows


def scan(files, profiles, lower_ms, upper_ms):
    profile_map = dict(profiles)
    if len(profile_map) != len(profiles) or len(set(profile_map.values())) != len(profiles):
        raise ValueError("each --profile label and provider must be unique")
    quota_groups, quota_observations, token_identities, token_usage_variants = {}, [], {}, {}
    counters = {
        "files": len(files), "bytes": 0, "quota_candidate_lines": 0,
        "quota_validated_events": 0, "quota_malformed_candidates": 0,
        "quota_window_observations": 0, "token_candidate_lines": 0,
        "token_validated_events": 0, "token_malformed_candidates": 0,
        "token_events_missing_usage": 0, "token_events_missing_cached_tokens": 0,
        "token_events_unselected_model": 0, "token_events_out_of_range": 0,
        "token_duplicate_observations": 0, "token_conflicting_duplicates": 0,
    }
    sessions = set()
    for path in files:
        try:
            counters["bytes"] += path.stat().st_size
            with path.open(errors="replace") as log:
                for line_number, raw in enumerate(log, 1):
                    if EVENT not in raw and TOKEN_EVENT not in raw:
                        continue
                    try:
                        event_name, record = projected_canonical_record(raw)
                    except (json.JSONDecodeError, MalformedCanonical, TypeError, ValueError):
                        if EVENT in raw:
                            counters["quota_candidate_lines"] += 1
                            counters["quota_malformed_candidates"] += 1
                        if TOKEN_EVENT in raw:
                            counters["token_candidate_lines"] += 1
                            counters["token_malformed_candidates"] += 1
                        continue
                    if event_name == EVENT:
                        counters["quota_candidate_lines"] += 1
                        for label, provider in profiles:
                            try:
                                extracted = rows_from_record(record, label, provider, lower_ms, upper_ms)
                            except (MalformedCanonical, TypeError, OverflowError, ValueError):
                                counters["quota_malformed_candidates"] += 1
                                break
                            if extracted is False:
                                continue
                            counters["quota_validated_events"] += 1
                            if extracted:
                                sessions.add(path.parent.name)
                            for row in extracted:
                                counters["quota_window_observations"] += 1
                                quota_observations.append(row)
                                key = (
                                    row["profile_label"], row["provider"], row["profile_epoch"],
                                    row["limit_id"], row["window_id"],
                                )
                                quota_groups.setdefault(key, []).append(row)
                            break
                    elif event_name == TOKEN_EVENT:
                        counters["token_candidate_lines"] += 1
                        try:
                            extracted = token_observation(record, profiles)
                        except (MalformedCanonical, TypeError, OverflowError, ValueError):
                            counters["token_malformed_candidates"] += 1
                            continue
                        if extracted == "missing_usage":
                            counters["token_events_missing_usage"] += 1
                        elif extracted == "unselected_model":
                            counters["token_events_unselected_model"] += 1
                        else:
                            if extracted["complete_usage"]:
                                counters["token_validated_events"] += 1
                            previous = token_identities.get(extracted["identity"])
                            tie_break = (
                                extracted["recorded_at_micros"], path.as_posix(), line_number
                            )
                            if previous is None:
                                token_identities[extracted["identity"]] = (tie_break, extracted)
                            else:
                                counters["token_duplicate_observations"] += 1
                                if tie_break < previous[0]:
                                    token_identities[extracted["identity"]] = (tie_break, extracted)
                            if extracted["complete_usage"]:
                                token_usage_variants.setdefault(extracted["identity"], set()).add(
                                    tuple(extracted[field] for field in (
                                        "cached_input_tokens", "uncached_input_tokens", "output_tokens"
                                    ))
                                )
        except OSError as error:
            raise ValueError(f"failed to scan selected canonical file {path}: {error}") from error
    quota_rows = []
    for group in quota_groups.values():
        quota_rows.extend(collapse_group(group))
    quota_rows.sort(key=lambda row: (
        row["observed_at_unix_ms"], row["profile_label"], row["provider"],
        row["profile_epoch"], row["limit_id"], row["window_id"], row["sequence"],
    ))
    retained_token_observations = [item[1] for item in token_identities.values()]
    counters["token_events_missing_cached_tokens"] = sum(
        not item["complete_usage"] for item in retained_token_observations
    )
    complete_token_observations = [
        item for item in retained_token_observations if item["complete_usage"]
    ]
    token_observations = [
        item for item in complete_token_observations
        if lower_ms * 1000 <= item["recorded_at_micros"] < upper_ms * 1000
    ]
    counters["token_events_out_of_range"] = len(complete_token_observations) - len(token_observations)
    counters["token_conflicting_duplicates"] = sum(
        len(variants) > 1 for variants in token_usage_variants.values()
    )
    token_rows = aggregate_tokens(token_observations)
    counters["sessions_with_quota_observations"] = len(sessions)
    counters["quota_emitted_rows"] = len(quota_rows)
    counters["quota_omitted_unchanged_rows"] = sum(
        row["omitted_unchanged_before"] for row in quota_rows
    )
    counters["token_unique_terminal_observations"] = len(token_observations)
    counters["token_hour_rows"] = len(token_rows)
    return quota_rows, quota_observations, token_rows, counters


def parse_profiles(values):
    if not values:
        return [("chatgpt", "chatgpt")]
    profiles = []
    for value in values:
        label, separator, provider = value.partition("=")
        if not separator or not label or not provider:
            raise ValueError("--profile must be LABEL=PROVIDER")
        profiles.append((label, provider))
    return profiles


def build_parser():
    parser = argparse.ArgumentParser()
    parser.add_argument("--sessions-root", default=str(pathlib.Path.home() / ".local/state/tau/sessions"))
    parser.add_argument(
        "--profile", action="append", metavar="LABEL=PROVIDER",
        help="repeatable configured subscription label and provider/model prefix; defaults to chatgpt=chatgpt",
    )
    parser.add_argument("--provider", help=argparse.SUPPRESS)  # handled in main for compatibility
    parser.add_argument("--since", help="inclusive RFC3339 lower bound; default now minus 14 days")
    parser.add_argument("--until", help="exclusive RFC3339 upper bound; default now")
    parser.add_argument("--out", required=True)
    return parser


def write_artifact_readme(path, profiles, lower_ms, upper_ms):
    selected = ", ".join(f"`{label}` (`{provider}`)" for label, provider in profiles)
    path.write_text(f"""# Tau quota and token-usage evidence

Range: `{iso_from_unix(lower_ms, 1000)}` through `{iso_from_unix(upper_ms, 1000)}` (exclusive).

Selected configured subscriptions: {selected}.

* `quota.csv` retains collapsed selected canonical quota evidence across every
  pool, window, and process epoch. `quota.svg` displays only the canonical
  default `codex/primary` series: the highest actual `remaining_percent`
  observation for each subscription and UTC hour. The SVG never connects a missing hour, and does
  not expose pool or process-epoch identifiers. Other pools and all epochs remain
  separate CSV evidence; an epoch is lifetime/process evidence, not an account identity.
  It guides and labels every UTC day boundary.
* `tokens.csv` retains accepted terminal usage in one-hour rows. `tokens.svg` reduces
  that evidence to UTC-aligned six-hour buckets on one shared logarithmic `log1p` Y axis:
  Cache hits (`prompt_cached_tokens`), Cache misses
  (`prompt_sent_tokens - prompt_cached_tokens`), and Output tokens
  (`response_received_tokens`). Each six-hour total divides by 21,600 for tokens/s.
  Subscription color identifies the selected profile and line style identifies the
  metric. The chart maps each actual rate as `log(1 + rate) / log(1 + maximum
  displayed rate)`, so an observed zero stays at the baseline without inventing a
  positive value. Y ticks show actual tokens/s. Missing six-hour buckets are missing
  evidence, not zero use.
  A terminal without the canonical `prompt_cached_tokens` field is omitted from all
  three token categories rather than treated as Cache hits zero. Token labels select
  a configured provider/model prefix; they do not identify an account or credential.
* `summary.txt` records selected files, byte count, malformed records, omissions, and
  deduplication. It contains no provider captures, prompts, or output items.
""")


def main(argv=None, now=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    clock = now or dt.datetime.now(UTC)
    try:
        profiles = parse_profiles(args.profile)
        if args.provider:
            if args.profile:
                raise ValueError("--provider cannot be combined with --profile")
            profiles = [(args.provider, args.provider)]
        upper = (
            parse_instant(args.until)
            if args.until
            else clock.replace(hour=0, minute=0, second=0, microsecond=0)
        )
        lower = parse_instant(args.since) if args.since else upper - dt.timedelta(days=14)
        lower_ms, upper_ms = unix_ms(lower), unix_ms(upper)
        if not lower_ms < upper_ms:
            parser.error("--since must be before --until")
        if MAX_RANGE_MS < upper_ms - lower_ms:
            parser.error("time range must not exceed 366 days")
        if lower_ms % DAY_MS or upper_ms % DAY_MS:
            parser.error("--since and --until must be aligned to UTC days for daily guides")
        files = selected_event_files(pathlib.Path(args.sessions_root))
    except (ValueError, OverflowError) as error:
        parser.error(str(error))
    output = pathlib.Path(args.out)
    output.mkdir(parents=True, exist_ok=True)
    started = time.perf_counter()
    try:
        quota_rows, quota_observations, token_rows, counters = scan(files, profiles, lower_ms, upper_ms)
        write_csv(output / "quota.csv", quota_rows)
        write_csv(output / "tokens.csv", token_rows, TOKEN_CSV_FIELDS)
        quota_display = quota_display_rows(quota_observations)
        quota_points = render_svg(
            output / "quota.svg", quota_display, profiles, lower_ms, upper_ms
        )
        token_buckets = aggregate_token_six_hour_buckets(token_rows)
        token_points = render_token_svg(output / "tokens.svg", token_buckets, profiles, lower_ms, upper_ms)
        write_artifact_readme(output / "README.md", profiles, lower_ms, upper_ms)
    except ValueError as error:
        parser.error(str(error))
    elapsed = time.perf_counter() - started
    lines = [
        f"selected profiles: {', '.join(f'{label}={provider}' for label, provider in profiles)}",
        "quota SVG display selection: canonical default codex/primary; maximum actual "
        "remaining percent per subscription and UTC hour, with missing hours as line breaks",
        "quota SVG guides: every UTC day boundary",
        "token SVG display selection: UTC six-hour Cache hits (prompt_cached_tokens), Cache misses "
        "(prompt_sent_tokens - prompt_cached_tokens), and Output tokens "
        "(response_received_tokens); each six-hour total divides by 21,600",
        "token SVG encoding: one shared log1p Y axis with observed zero at the baseline; "
        "subscription color; metric line style; missing UTC six-hour buckets are line breaks",
        f"selected canonical files: {counters['files']}",
        f"selected file bytes: {counters['bytes']}",
        f"quota candidate lines: {counters['quota_candidate_lines']}",
        f"validated selected quota events: {counters['quota_validated_events']}",
        f"malformed quota candidates: {counters['quota_malformed_candidates']}",
        f"sessions with selected quota observations: {counters['sessions_with_quota_observations']}",
        f"quota window observations in range: {counters['quota_window_observations']}",
        f"retained quota CSV evidence rows: {counters['quota_emitted_rows']}",
        f"omitted unchanged quota rows: {counters['quota_omitted_unchanged_rows']}",
        f"rendered quota hourly default-series observations: {quota_points}",
        f"token candidate lines: {counters['token_candidate_lines']}",
        f"validated selected token event copies with complete usage: {counters['token_validated_events']}",
        f"malformed token candidates: {counters['token_malformed_candidates']}",
        f"token events without canonical usage: {counters['token_events_missing_usage']}",
        f"token events with missing Cache hits field: {counters['token_events_missing_cached_tokens']}",
        f"token events for unselected or missing model: {counters['token_events_unselected_model']}",
        f"token events outside range: {counters['token_events_out_of_range']}",
        f"duplicate token terminal observations discarded: {counters['token_duplicate_observations']}",
        f"conflicting duplicate token identities: {counters['token_conflicting_duplicates']}",
        f"unique token terminal observations: {counters['token_unique_terminal_observations']}",
        f"UTC one-hour token rows: {counters['token_hour_rows']}",
        f"UTC six-hour token display rows: {len(token_buckets)}",
        f"rendered six-hour token values across three categories: {token_points}",
        f"elapsed seconds: {elapsed:.3f}",
        f"time filter: [{iso_from_unix(lower_ms, 1000)}, {iso_from_unix(upper_ms, 1000)})",
        "latest quota observation by configured subscription:",
    ]
    latest = {}
    for row in quota_display:
        key = (row["profile_label"], row["provider"])
        if key not in latest or row["observed_at_unix_ms"] > latest[key]["observed_at_unix_ms"]:
            latest[key] = row
    for key, row in sorted(latest.items()):
        lines.append(
            f"- {key[0]} subscription, latest displayed {DEFAULT_QUOTA_SERIES[0]}/"
            f"{DEFAULT_QUOTA_SERIES[1]} quota: {row['remaining_percent']}% "
            f"remaining at {row['observed_at']}; "
            f"reset {row['reset_at'] or 'unknown'}"
        )
    text = "\n".join(lines) + "\n"
    (output / "summary.txt").write_text(text)
    print(text, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
