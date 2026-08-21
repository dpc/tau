#!/usr/bin/env python3
"""Oracle tests for the bundled offline quota and token extractor."""

import csv
import datetime as dt
import importlib.util
import json
import os
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name("extract_quota.py")
SPEC = importlib.util.spec_from_file_location("extract_quota", SCRIPT)
quota = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(quota)
UTC = dt.timezone.utc
PROFILES = [("personal", "chatgpt"), ("work", "chatgpt-fedi")]


def canonical(observed, *, provider="chatgpt", epoch="epoch-a", sequence=1, used=2500,
              reset=1_800_000_000, routes=None, **window_updates):
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
            "provider": provider, "profile_epoch": epoch, "sequence": sequence,
            "route_bindings": routes or [{
                "model": f"{provider}/a", "limit_ids": ["codex"], "provenance": "turn",
                "observed_at_unix_ms": observed,
            }],
            "windows": [window],
        }},
    }


def token(recorded, *, provider="chatgpt", agent="main", prompt="sp-1", attempt=1,
          sent=1000, cached=800, output=42, usage=True, cached_field=True):
    payload = {"agent_id": agent, "agent_prompt_id": prompt}
    if attempt != 1:
        payload["provider_attempt"] = attempt
    if usage:
        payload["usage"] = {
            "model": f"{provider}/gpt-5", "prompt_sent_tokens": sent,
            "response_received_tokens": output,
        }
        if cached_field:
            payload["usage"]["prompt_cached_tokens"] = cached
    return {
        "type": "published", "recorded_at_micros": recorded * 1000,
        "event": {"event": quota.TOKEN_EVENT, "payload": payload},
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
        (session / "events.jsonl").write_text("".join(
            json.dumps(record, separators=(",", ":")) + "\n"
            if not isinstance(record, str) else record for record in records
        ))
        return session

    def test_selection_follows_session_symlink(self):
        target = self.write_session("target", [canonical(1_700_000_000_000)])
        selected = pathlib.Path(self.temp.name) / "selected"
        selected.mkdir()
        (selected / "linked").symlink_to(target, target_is_directory=True)
        self.assertEqual(quota.selected_event_files(selected), [selected / "linked" / "events.jsonl"])

    def test_quota_selection_is_labeled_and_half_open(self):
        lower, upper = 1_700_000_000_000, 1_700_000_000_100
        self.assertEqual(
            quota.rows_from_record(canonical(lower - 1), "personal", "chatgpt", lower, upper), []
        )
        row = quota.rows_from_record(canonical(lower), "personal", "chatgpt", lower, upper)[0]
        self.assertEqual(row["profile_label"], "personal")
        self.assertEqual(row["remaining_basis_points"], 7500)
        self.assertEqual(
            quota.rows_from_record(canonical(upper), "personal", "chatgpt", lower, upper), []
        )

    def test_quota_collapse_keeps_process_epochs_separate(self):
        base = 1_700_000_000_000
        session = self.write_session("epochs", [
            canonical(base, epoch="a"), canonical(base + 1, epoch="b"),
        ])
        rows, _, _, counters = quota.scan([session / "events.jsonl"], PROFILES, 0, base + 10)
        self.assertEqual({row["profile_epoch"] for row in rows}, {"a", "b"})
        self.assertEqual(counters["quota_omitted_unchanged_rows"], 0)

    def test_token_uses_canonical_fields_and_utc_hour(self):
        base = 1_700_000_001_234
        session = self.write_session("tokens", [
            token(base, sent=1000, cached=800, output=42),
            token(base + 1, prompt="sp-2", sent=300, cached=0, output=9),
            token(base + quota.HOUR_MS, provider="chatgpt-fedi", prompt="sp-3",
                  sent=50, cached=10, output=5),
        ])
        _, _, rows, counters = quota.scan(
            [session / "events.jsonl"], PROFILES, 0, base + 2 * quota.HOUR_MS
        )
        self.assertEqual(len(rows), 2)
        first = rows[0]
        self.assertEqual(first["profile_label"], "personal")
        self.assertEqual(first["cached_input_tokens"], 800)
        self.assertEqual(first["uncached_input_tokens"], 500)
        self.assertEqual(first["output_tokens"], 51)
        self.assertEqual(first["cached_input_tokens_per_second"], 800 / 3600)
        self.assertEqual(counters["token_unique_terminal_observations"], 3)

    def test_token_missing_cached_input_is_unknown_not_zero(self):
        base = 1_700_000_000_000
        session = self.write_session("missing-cache", [
            token(base, cached_field=False), token(base + 1, prompt="sp-2", usage=False),
        ])
        _, _, rows, counters = quota.scan([session / "events.jsonl"], PROFILES, 0, base + 10)
        self.assertEqual(rows, [])
        self.assertEqual(counters["token_events_missing_cached_tokens"], 1)
        self.assertEqual(counters["token_events_missing_usage"], 1)

    def test_missing_earliest_cache_evidence_omits_explicit_zero_replay(self):
        base = 1_700_000_000_000
        session = self.write_session("missing-earliest-cache", [
            token(base, cached_field=False),
            token(base + 1, cached=0),
        ])
        _, _, rows, counters = quota.scan(
            [session / "events.jsonl"], PROFILES, base, base + quota.HOUR_MS
        )
        self.assertEqual(rows, [])
        self.assertEqual(counters["token_events_missing_cached_tokens"], 1)
        self.assertEqual(counters["token_duplicate_observations"], 1)

    def test_token_deduplicates_canonical_terminal_identity(self):
        base = 1_700_000_000_000
        duplicate = token(base + 1, sent=500, cached=200, output=10)
        conflict = token(base + 2, sent=501, cached=200, output=10)
        session = self.write_session("duplicates", [duplicate, duplicate, conflict])
        _, _, rows, counters = quota.scan([session / "events.jsonl"], PROFILES, 0, base + 10)
        self.assertEqual(rows[0]["cached_input_tokens"], 200)
        self.assertEqual(rows[0]["uncached_input_tokens"], 300)
        self.assertEqual(counters["token_duplicate_observations"], 2)
        self.assertEqual(counters["token_conflicting_duplicates"], 1)
        self.assertEqual(counters["token_unique_terminal_observations"], 1)

    def test_token_deduplicates_before_time_range(self):
        lower = 1_700_000_000_000
        session = self.write_session("cross-boundary-duplicate", [
            token(lower - 1, sent=500, cached=200, output=10),
            token(lower + 1, sent=500, cached=200, output=10),
        ])
        _, _, rows, counters = quota.scan(
            [session / "events.jsonl"], PROFILES, lower, lower + quota.HOUR_MS
        )
        self.assertEqual(rows, [])
        self.assertEqual(counters["token_duplicate_observations"], 1)
        self.assertEqual(counters["token_events_out_of_range"], 1)

    def test_token_duplicate_tie_break_uses_microseconds(self):
        base = 1_700_000_000_000
        later = token(base, sent=500, cached=200, output=10)
        later["recorded_at_micros"] += 500
        earlier = token(base, sent=500, cached=100, output=10)
        earlier["recorded_at_micros"] += 100
        session = self.write_session("microsecond-tie", [later, earlier])
        _, _, rows, _ = quota.scan([session / "events.jsonl"], PROFILES, base, base + quota.HOUR_MS)
        self.assertEqual(rows[0]["cached_input_tokens"], 100)
        self.assertEqual(rows[0]["uncached_input_tokens"], 400)

    def test_conflicting_token_duplicates_count_identity_once(self):
        base = 1_700_000_000_000
        later_a = token(base + 3, cached=100)
        middle_b = token(base + 2, cached=200)
        earliest_a = token(base + 1, cached=100)
        session = self.write_session("conflicting-duplicates", [later_a, middle_b, earliest_a])
        _, _, rows, counters = quota.scan(
            [session / "events.jsonl"], PROFILES, base, base + quota.HOUR_MS
        )
        self.assertEqual(rows[0]["cached_input_tokens"], 100)
        self.assertEqual(counters["token_duplicate_observations"], 2)
        self.assertEqual(counters["token_conflicting_duplicates"], 1)

    def test_malformed_and_unselected_records_are_accounted(self):
        base = 1_700_000_000_000
        malformed = token(base)
        malformed["event"]["payload"]["usage"]["prompt_cached_tokens"] = "bad"
        session = self.write_session("bad", [
            malformed, token(base + 1, provider="other"), f'{{"event":"{quota.EVENT}"',
        ])
        _, _, _, counters = quota.scan([session / "events.jsonl"], PROFILES, 0, base + 10)
        self.assertEqual(counters["token_malformed_candidates"], 1)
        self.assertEqual(counters["token_events_unselected_model"], 1)
        self.assertEqual(counters["quota_malformed_candidates"], 1)

    def test_empty_artifacts_and_combined_legends(self):
        output = pathlib.Path(self.temp.name)
        quota.write_csv(output / "empty.csv", [])
        with (output / "empty.csv").open() as source:
            self.assertEqual(next(csv.reader(source)), quota.QUOTA_CSV_FIELDS)
        lower, upper = 1_700_006_400_000, 1_700_006_400_000 + quota.DAY_MS
        quota.render_svg(output / "quota.svg", [], PROFILES, lower, upper)
        quota.render_token_svg(output / "tokens.svg", [], PROFILES, lower, upper)
        quota_svg = (output / "quota.svg").read_text()
        token_svg = (output / "tokens.svg").read_text()
        self.assertIn("personal", quota_svg)
        self.assertNotIn("quota pool", quota_svg)
        self.assertNotIn("<circle", quota_svg)
        self.assertIn('text-anchor="end" x="118"', quota_svg)
        self.assertIn("work", token_svg)
        self.assertIn("Cache hits", token_svg)
        self.assertIn("Cache misses", token_svg)
        self.assertIn("Output tokens", token_svg)
        self.assertNotIn("all input", token_svg)
        self.assertNotIn("all tokens", token_svg)
        self.assertIn("six-hour total / 21,600", token_svg)

    def test_quota_svg_selects_hourly_maximum_default_pool_observation(self):
        lower, upper = 1_700_006_400_000, 1_700_006_400_000 + quota.DAY_MS
        first = quota.rows_from_record(
            canonical(lower + 1, sequence=1, used=1000), "personal", "chatgpt", lower, upper
        )[0]
        lower_remaining = quota.rows_from_record(
            canonical(lower + 2, epoch="epoch-b", sequence=2, used=2000),
            "personal", "chatgpt", lower, upper,
        )[0]
        additional = lower_remaining.copy()
        additional["limit_id"] = "other"
        displayed = quota.quota_display_rows([first, lower_remaining, additional])
        self.assertEqual(len(displayed), 1)
        self.assertEqual(displayed[0]["used_basis_points"], 1000)
        output = pathlib.Path(self.temp.name) / "quota.svg"
        self.assertEqual(quota.render_svg(output, [first, lower_remaining, additional], PROFILES, lower, upper), 1)
        svg = output.read_text()
        self.assertEqual(svg.count('<line class="series"'), 3)
        self.assertNotIn("epoch-b", svg)
        self.assertNotIn("other", svg)

    def test_six_hour_token_display_uses_shared_log1p_rates_and_gap_breaks(self):
        lower = 1_700_006_400_000
        observations = [
            quota.token_observation(
                token(lower + quota.HOUR_MS, sent=1000, cached=600, output=100), PROFILES
            ),
            quota.token_observation(
                token(lower + 2 * quota.HOUR_MS, sent=3000, cached=1400, output=300), PROFILES
            ),
        ]
        hours = quota.aggregate_tokens(observations)
        buckets = quota.aggregate_token_six_hour_buckets(hours)
        self.assertEqual(len(buckets), 1)
        self.assertEqual(buckets[0]["cached_input_tokens_per_second"], 2000 / 21_600)
        self.assertEqual(buckets[0]["uncached_input_tokens_per_second"], 2000 / 21_600)
        self.assertEqual(buckets[0]["output_tokens_per_second"], 400 / 21_600)
        later = {**buckets[0], "bucket_start_unix_ms": lower + 2 * quota.SIX_HOURS_MS}
        output = pathlib.Path(self.temp.name) / "tokens.svg"
        self.assertEqual(
            quota.render_token_svg(output, [buckets[0], later], PROFILES, lower, lower + quota.DAY_MS),
            6,
        )
        svg = output.read_text()
        self.assertEqual(svg.count('<line class="series"'), 11)
        self.assertNotIn("<circle", svg)
        self.assertNotIn("all input", svg)
        self.assertNotIn("all tokens", svg)
        self.assertEqual(svg.count("six-hour total / 21,600 tokens/s (log1p scale; 0 preserved)"), 1)
        self.assertIn('stroke-dasharray="8 5"', svg)
        self.assertIn('stroke-dasharray="2 4"', svg)

    def test_token_svg_has_six_profile_metric_series_on_one_log1p_axis(self):
        lower = 1_700_006_400_000
        daily = {
            "bucket_start_unix_ms": lower,
            "profile_label": "personal",
            "provider": "chatgpt",
            "cached_input_tokens_per_second": 100,
            "uncached_input_tokens_per_second": 10,
            "output_tokens_per_second": 0,
        }
        rows = [
            {**daily, "bucket_start_unix_ms": lower + bucket * quota.SIX_HOURS_MS,
             "profile_label": label, "provider": provider}
            for bucket in range(2)
            for label, provider in PROFILES
        ]
        output = pathlib.Path(self.temp.name) / "tokens.svg"
        self.assertEqual(quota.render_token_svg(output, rows, PROFILES, lower, lower + quota.DAY_MS), 12)
        svg = output.read_text()
        self.assertEqual(svg.count('<path class="series"'), 6)
        self.assertEqual(svg.count('transform="rotate(-90'), 1)
        self.assertIn(">0</text>", svg)
        self.assertIn(",502.0", svg)
        for color in ("#2563eb", "#dc2626"):
            for dash in ("", "8 5", "2 4"):
                self.assertIn(
                    f'<path class="series" stroke="{color}" stroke-dasharray="{dash}"', svg
                )

    def test_token_six_hour_buckets_start_at_utc_six_hour_boundaries(self):
        lower = 1_700_006_400_000
        rows = [
            {
                "hour_start_unix_ms": lower + hour * quota.HOUR_MS,
                "profile_label": "personal",
                "provider": "chatgpt",
                "cached_input_tokens": 1,
                "uncached_input_tokens": 1,
                "output_tokens": 1,
                "accepted_terminal_observations": 1,
            }
            for hour in (5, 6, 11, 12)
        ]
        buckets = quota.aggregate_token_six_hour_buckets(rows)
        self.assertEqual(
            [bucket["bucket_start_unix_ms"] for bucket in buckets],
            [lower, lower + 6 * quota.HOUR_MS, lower + 12 * quota.HOUR_MS],
        )

    def test_quota_display_uses_hourly_maximum_observations(self):
        lower = 1_700_006_400_000
        records = [canonical(lower + day * quota.DAY_MS + 1) for day in range(3)]
        for record in records[1:]:
            payload = record["event"]["payload"]
            payload["route_bindings"][0]["observed_at_unix_ms"] = lower + 1
            window = payload["windows"][0]
            window["timing_anchor_observed_at_unix_ms"] = lower + 1
            window["server_offset_observed_at_unix_ms"] = lower + 1
        session = self.write_session("quota-days", records)
        collapsed, observations, _, _ = quota.scan(
            [session / "events.jsonl"], PROFILES, lower, lower + 3 * quota.DAY_MS
        )
        self.assertEqual(len(collapsed), 2)
        displayed = quota.quota_display_rows(observations)
        self.assertEqual(len(displayed), 3)
        output = pathlib.Path(self.temp.name) / "quota.svg"
        self.assertEqual(
            quota.render_svg(output, observations, PROFILES, lower, lower + 3 * quota.DAY_MS), 3
        )
        self.assertEqual((output.read_text()).count('<line class="series"'), 5)

    def test_terminal_projection_skips_unselected_output_items(self):
        record = token(1_700_006_400_000)
        record["event"]["payload"]["output_items"] = [{"content": "not selected"}]
        name, projected = quota.projected_canonical_record(json.dumps(record))
        self.assertEqual(name, quota.TOKEN_EVENT)
        payload = projected["event"]["payload"]
        self.assertNotIn("output_items", payload)
        self.assertTrue(quota.token_observation(projected, PROFILES)["complete_usage"])

    def test_terminal_projection_rejects_malformed_selected_and_skipped_values(self):
        record = token(1_700_006_400_000)
        selected = json.dumps(record).replace(
            '"prompt_cached_tokens": 800}', '"prompt_cached_tokens": 800,}'
        )
        record["event"]["payload"]["output_items"] = [{"content": "not selected"}]
        skipped = json.dumps(record).replace(
            '[{"content": "not selected"}]', '[{"content": "not selected"},]'
        )
        skipped_non_json_whitespace = json.dumps(record).replace(
            '[{"content": "not selected"}]', '[\u00a0{"content": "not selected"}]'
        )
        for raw in (selected, skipped, skipped_non_json_whitespace):
            with self.assertRaises(quota.MalformedCanonical):
                quota.projected_canonical_record(raw)

    def test_singleton_series_uses_a_full_line_style_cycle(self):
        svg = []
        quota.append_daily_series(
            svg, [{"time": 0}], "time", quota.HOUR_MS, lambda _: 50, lambda _: (0, 100),
            lambda _: 10, "#000", "8 5"
        )
        self.assertIn('x1="0.0" x2="100.0"', svg[0])
        self.assertIn('stroke-dasharray="8 5"', svg[0])

    def test_fourteen_day_charts_guide_and_label_every_utc_day(self):
        lower = 1_700_006_400_000
        output = pathlib.Path(self.temp.name) / "quota.svg"
        quota.render_svg(output, [], PROFILES, lower, lower + 14 * quota.DAY_MS)
        svg = output.read_text()
        self.assertEqual(svg.count('stroke-dasharray="3 3"'), 14)
        self.assertEqual(svg.count('y="546"'), 14)

    def test_main_writes_both_charts_and_readme(self):
        now = dt.datetime(2026, 7, 28, tzinfo=UTC)
        self.write_session("empty", [])
        output = pathlib.Path(self.temp.name) / "out"
        quota.main([
            "--sessions-root", str(self.root),
            "--profile", "personal=chatgpt", "--profile", "work=chatgpt-fedi",
            "--out", str(output),
        ], now=now)
        self.assertTrue((output / "quota.csv").exists())
        self.assertTrue((output / "quota.svg").exists())
        self.assertTrue((output / "tokens.csv").exists())
        self.assertTrue((output / "tokens.svg").exists())
        self.assertIn("epoch is lifetime/process evidence", (output / "README.md").read_text())
        self.assertIn("every UTC day boundary", (output / "README.md").read_text())
        self.assertIn("one shared log1p Y axis", (output / "summary.txt").read_text())

    def test_invalid_ranges_and_profiles_fail(self):
        self.write_session("empty", [])
        output = pathlib.Path(self.temp.name) / "out"
        with self.assertRaises(SystemExit):
            quota.main([
                "--sessions-root", str(self.root), "--since", "2026-07-28T00:00:00Z",
                "--until", "2026-07-28T00:00:00Z", "--out", str(output),
            ])
        with self.assertRaises(ValueError):
            quota.parse_profiles(["no-equals"])
        with self.assertRaises(SystemExit):
            quota.main([
                "--sessions-root", str(self.root), "--since", "2026-07-27T23:30:00Z",
                "--until", "2026-07-28T00:00:00Z", "--out", str(output),
            ])

    def test_log1p_scale_ticks_label_actual_rates_and_preserve_zero(self):
        ticks, ceiling = quota.log1p_ticks(16_500)
        self.assertEqual(ticks, [("0", 0), ("1", 1), ("10", 10), ("100", 100), ("1k", 1000), ("10k", 10_000), ("16.5k", 16_500)])
        self.assertEqual(ceiling, 16_500)

    def test_documented_script_invocation_is_executable(self):
        self.assertTrue(os.access(SCRIPT, os.X_OK))
        subprocess.run([str(SCRIPT), "--help"], check=True, capture_output=True, text=True)


if __name__ == "__main__":
    unittest.main()
