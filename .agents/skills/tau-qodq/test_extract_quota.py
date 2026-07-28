#!/usr/bin/env python3
"""Oracle tests for the bundled offline quota extractor."""

import csv
import datetime as dt
import importlib.util
import json
import pathlib
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name("extract_quota.py")
SPEC = importlib.util.spec_from_file_location("extract_quota", SCRIPT)
quota = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(quota)
UTC = dt.timezone.utc


def canonical(observed, *, epoch="epoch-a", sequence=1, used=2500, reset=1_800_000_000, routes=None, **window_updates):
    window = {
        "key": {"limit_id": "codex", "window_id": "primary"},
        "used_basis_points": used, "usage_observed_at_unix_ms": observed,
        "window_seconds": 604800, "reset_at_unix_seconds": reset,
        "remaining_seconds_at_timing_anchor": 42,
        "timing_anchor_observed_at_unix_ms": observed,
        "server_offset_ms": -10, "server_offset_observed_at_unix_ms": observed,
    }
    window.update(window_updates)
    return {
        "type": "published", "recorded_at_micros": observed * 1000,
        "event": {"event": quota.EVENT, "payload": {
            "provider": "chatgpt", "profile_epoch": epoch, "sequence": sequence,
            "route_bindings": routes or [{"model": "chatgpt/a", "limit_ids": ["codex"], "provenance": "turn", "observed_at_unix_ms": observed}, {"model": "chatgpt/b", "limit_ids": ["codex", "other"], "provenance": "turn", "observed_at_unix_ms": observed + 1}],
            "windows": [window],
        }},
    }


class ExtractQuotaTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.temp.name) / "sessions"
        self.root.mkdir()

    def tearDown(self):
        self.temp.cleanup()

    def write_session(self, name, records):
        session = self.root / name
        session.mkdir()
        (session / "events.jsonl").write_text("".join(json.dumps(record, separators=(",", ":")) + "\n" if not isinstance(record, str) else record for record in records))
        return session

    def test_selection_follows_session_symlink_and_colon_name(self):
        target = self.write_session("target:session", [canonical(1_700_000_000_000)])
        selected = pathlib.Path(self.temp.name) / "selected"
        selected.mkdir()
        (selected / "linked:session").symlink_to(target, target_is_directory=True)
        (selected / "decoy.txt").write_text("private")
        files = quota.selected_event_files(selected)
        self.assertEqual(files, [selected / "linked:session" / "events.jsonl"])
        with self.assertRaises(ValueError):
            quota.selected_event_files(selected / "missing")

    def test_half_open_range_and_many_to_many_routes(self):
        lower, upper = 1_700_000_000_000, 1_700_000_000_100
        before = quota.rows_from_record(canonical(lower - 1), "chatgpt", lower, upper)
        at_lower = quota.rows_from_record(canonical(lower), "chatgpt", lower, upper)
        at_upper = quota.rows_from_record(canonical(upper), "chatgpt", lower, upper)
        self.assertEqual(before, [])
        self.assertEqual(at_upper, [])
        self.assertEqual(at_lower[0]["remaining_basis_points"], 7500)
        self.assertEqual(json.loads(at_lower[0]["route_models_json"]), ["chatgpt/a", "chatgpt/b"])
        self.assertEqual(at_lower[0]["server_offset_observed_at_unix_ms"], lower)

    def test_collapse_counts_and_full_group_identity(self):
        base = 1_700_000_000_000
        for count, omitted, emitted in [(1, 0, 1), (2, 0, 2), (3, 1, 2)]:
            rows = [quota.rows_from_record(canonical(base + index, sequence=index), "chatgpt", 0, base + 100)[0] for index in range(count)]
            for row in rows[1:]:
                for field in ("timing_anchor_observed_at_unix_ms", "server_offset_observed_at_unix_ms", "route_observed_at_unix_ms_json"):
                    row[field] = rows[0][field]
            collapsed = quota.collapse_group(rows)
            self.assertEqual(len(collapsed), emitted)
            self.assertEqual(sum(row["omitted_unchanged_before"] for row in collapsed), omitted)
        records = [canonical(base, epoch="a"), canonical(base + 1, epoch="b")]
        session = self.write_session("epochs", records)
        rows, counters = quota.scan([session / "events.jsonl"], "chatgpt", 0, base + 10)
        self.assertEqual(len(rows), 2)
        self.assertEqual({row["profile_epoch"] for row in rows}, {"a", "b"})
        self.assertEqual(counters["omitted_unchanged_rows"], 0)

    def test_zero_use_reset_changes_are_evidence(self):
        base = 1_700_000_000_000
        records = [
            canonical(base, used=0, reset=None, sequence=1),
            canonical(base + 1, used=0, reset=1_800_000_000, sequence=2),
        ]
        rows = [quota.rows_from_record(record, "chatgpt", 0, base + 10)[0] for record in records]
        self.assertEqual(len(quota.collapse_group(rows)), 2)

    def test_timing_only_changes_are_evidence(self):
        base = 1_700_000_000_000
        first = quota.rows_from_record(canonical(base, sequence=1), "chatgpt", 0, base + 10)[0]
        second = quota.rows_from_record(canonical(base + 1, sequence=2), "chatgpt", 0, base + 10)[0]
        for field in quota.CSV_FIELDS:
            if field not in {"observed_at", "observed_at_unix_ms", "sequence", "server_offset_ms"}:
                second[field] = first[field]
        second["server_offset_ms"] = first["server_offset_ms"] - 1
        self.assertEqual(len(quota.collapse_group([first, second])), 2)

    def test_malformed_canonical_and_decoy_accounting(self):
        base = 1_700_000_000_000
        malformed = canonical(base)
        malformed["event"]["payload"]["windows"] = "bad"
        extreme = canonical(base + 1)
        extreme["event"]["payload"]["windows"][0]["reset_at_unix_seconds"] = 10**30
        wrong_type = canonical(base + 2)
        wrong_type["type"] = "from_connection"
        decoy = {"event": {"event": "other", "payload": {"text": f'\"event\":\"{quota.EVENT}\"'}}}
        torn = f'{{"event":{{"event":"{quota.EVENT}"'
        session = self.write_session("bad", [malformed, extreme, wrong_type, decoy, torn])
        rows, counters = quota.scan([session / "events.jsonl"], "chatgpt", 0, base + 10)
        self.assertEqual(rows, [])
        self.assertEqual(counters["malformed_canonical"], 4)
        self.assertEqual(counters["canonical_events"], 0)

    def test_mixed_provider_counts_only_selected_provider(self):
        base = 1_700_000_000_000
        other = canonical(base)
        other["event"]["payload"]["provider"] = "other"
        session = self.write_session("mixed", [other, canonical(base + 1)])
        rows, counters = quota.scan([session / "events.jsonl"], "chatgpt", 0, base + 10)
        self.assertEqual(len(rows), 1)
        self.assertEqual(counters["canonical_events"], 1)

    def test_empty_csv_schema_and_svg_oracle(self):
        output = pathlib.Path(self.temp.name)
        quota.write_csv(output / "empty.csv", [])
        with (output / "empty.csv").open() as source:
            self.assertEqual(next(csv.reader(source)), quota.CSV_FIELDS)
        lower = 1_700_006_400_000  # 2023-11-15 00:00 UTC
        upper = lower + 2 * quota.DAY_MS
        row = quota.rows_from_record(canonical(lower + 1), "chatgpt", lower, upper)[0]
        same_pixel = row.copy()
        same_pixel["observed_at_unix_ms"] += 1
        distinct_pixel = row.copy()
        distinct_pixel["observed_at_unix_ms"] += 1_000_000
        distinct_color = row.copy()
        distinct_color["limit_id"] = "other"
        rows = [row, same_pixel, distinct_pixel, distinct_color]
        quota.write_csv(output / "retained.csv", rows)
        points = quota.render_svg(output / "chart.svg", rows, "chatgpt", lower, upper)
        svg = (output / "chart.svg").read_text()
        with (output / "retained.csv").open() as source:
            self.assertEqual(sum(1 for _ in csv.DictReader(source)), 4)
        self.assertEqual(points, 3)
        self.assertEqual(svg.count("<circle"), 3)
        self.assertNotIn("polyline", svg)
        self.assertEqual(svg.count("stroke-dasharray"), 1)
        self.assertIn("<circle", svg)
        self.assertNotIn("1800000000", svg)  # reset time never sets x-domain/labels

    def test_extreme_observation_and_inverted_range_fail(self):
        record = canonical(1_700_000_000_000)
        record["event"]["payload"]["windows"][0]["usage_observed_at_unix_ms"] = 10**30
        with self.assertRaises(quota.MalformedCanonical):
            quota.rows_from_record(record, "chatgpt", 0, 10**31)
        self.write_session("empty-range", [])
        with self.assertRaises(SystemExit):
            quota.main(["--sessions-root", str(self.root), "--since", "2026-07-28T00:00:00Z", "--until", "2026-07-28T00:00:00Z", "--out", str(pathlib.Path(self.temp.name) / "bad")])
        with self.assertRaises(SystemExit):
            quota.main(["--sessions-root", str(self.root), "--since", "2020-01-01T00:00:00Z", "--until", "2022-01-01T00:00:00Z", "--out", str(pathlib.Path(self.temp.name) / "too-wide")])

    def test_main_uses_one_clock_and_until_only(self):
        now = dt.datetime(2026, 7, 28, tzinfo=UTC)
        self.write_session("empty", [])
        output = pathlib.Path(self.temp.name) / "out"
        quota.main(["--sessions-root", str(self.root), "--out", str(output)], now=now)
        summary = (output / "summary.txt").read_text()
        self.assertIn("[2026-07-14T00:00:00.000Z, 2026-07-28T00:00:00.000Z)", summary)
        output2 = pathlib.Path(self.temp.name) / "out2"
        quota.main(["--sessions-root", str(self.root), "--until", "2026-07-20T00:00:00Z", "--out", str(output2)], now=now)
        self.assertIn("[2026-07-06T00:00:00.000Z, 2026-07-20T00:00:00.000Z)", (output2 / "summary.txt").read_text())


if __name__ == "__main__":
    unittest.main()
