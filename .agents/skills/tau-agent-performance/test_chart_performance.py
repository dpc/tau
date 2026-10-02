#!/usr/bin/env python3
"""Focused oracles for routine performance reporting."""

import collections
import contextlib
import csv
import datetime as dt
import io
import json
import pathlib
import tempfile
import unittest
from unittest import mock

import chart_performance as perf

START = perf.instant("2026-10-01T00:00:00Z")


def header():
    return dict(schema="tau.agent_performance", schema_version=0, record_type="header",
                content_included=False, origin_recorded_at_unix_micros=START)


def prompt(number=0, **changes):
    row = dict(record_type="provider_prompt", agent_id="PRIVATE_AGENT",
               agent_prompt_id=f"PRIVATE_PROMPT_{number}", journal_seq=number,
               model="provider/model-v1", terminal_present=True,
               terminal_at_us=1_000_000, recorded_at_wall_elapsed_us=1_000_000,
               response_received_tokens=10)
    row.update(changes)
    return row


def trace(*rows):
    return io.StringIO("\n".join(json.dumps(r) for r in [header(), *rows]))


class PerformanceTests(unittest.TestCase):
    def select(self, *rows, since=START, until=START + perf.DAY_US):
        counts = collections.Counter()
        values = perf.read_trace(trace(*rows), since, until, counts)
        return perf.aggregate(values, since, until), counts

    def test_medians_exact_versions_and_dedup(self):
        rows, counts = self.select(prompt(), prompt(), prompt(1, response_received_tokens=100),
                                   prompt(2, model="provider/model-v2"),
                                   prompt(3, model="other/model-v1"))
        self.assertEqual(len(rows), 3)
        v1 = next(r for r in rows if r["provider"] == "provider" and r["model"] == "model-v1")
        self.assertEqual(v1["completed_samples"], 2)
        self.assertEqual(v1["median_output_tokens_per_wall_second"], 55)
        self.assertEqual(counts["duplicate_rows"], 1)

    def test_missing_zero_and_incomplete_are_distinct(self):
        rows, counts = self.select(
            prompt(0, terminal_present=False),
            prompt(1, terminal_at_us=None),
            prompt(2, recorded_at_wall_elapsed_us=None),
            prompt(3, recorded_at_wall_elapsed_us=0),
            prompt(4, response_received_tokens=None),
            prompt(5, response_received_tokens=0),
            prompt(6, recorded_at_wall_elapsed_us=-1),
        )
        self.assertEqual(counts["incomplete_prompts"], 1)
        self.assertEqual(counts["missing_terminal_time"], 1)
        self.assertEqual(rows[0]["completed_samples"], 5)
        self.assertEqual(rows[0]["latency_samples"], 3)
        self.assertEqual(rows[0]["rate_samples"], 1)
        self.assertEqual(rows[0]["median_output_tokens_per_wall_second"], 0)

    def test_half_open_partial_current_bucket_and_gaps(self):
        until = START + 13 * 3600 * 1_000_000
        rows, _ = self.select(prompt(0), prompt(1, terminal_at_us=12 * 3600 * 1_000_000),
                              prompt(2, terminal_at_us=13 * 3600 * 1_000_000), until=until)
        self.assertEqual(len(rows), 2)
        self.assertEqual(rows[-1]["bucket_end"], perf.iso(until))
        svg = perf.chart(rows, "median_wall_latency_seconds", "Latency", "seconds", START, until)
        self.assertEqual(svg.count("<circle"), 2)
        # Paths with fill=none are metric connectors; no connector spans the gap.
        self.assertNotIn('fill="none"', svg)
        contiguous, _ = self.select(prompt(), prompt(1, terminal_at_us=6 * 3600 * 1_000_000))
        self.assertIn('fill="none"', perf.chart(contiguous, "median_wall_latency_seconds",
                                               "Latency", "seconds", START, until))

    def test_schema_validation_and_escaping(self):
        bad = header()
        bad["content_included"] = True
        with self.assertRaises(ValueError):
            perf.read_trace(io.StringIO(json.dumps(bad)), START, START + perf.DAY_US,
                            collections.Counter())
        rows, _ = self.select(prompt(model="provider/model<&>"))
        svg = perf.chart(rows, "median_wall_latency_seconds", "Latency", "seconds",
                         START, START + perf.DAY_US)
        self.assertIn("model&lt;&amp;&gt;", svg)
        self.assertNotIn("PRIVATE", svg)

    def test_discovery_skips_failed_and_timed_out_journals_privately(self):
        with tempfile.TemporaryDirectory() as root:
            agents = pathlib.Path(root)
            for name in ["ok", "old", "slow"]:
                (agents / name).mkdir()
                (agents / name / "events.cbor").touch()

            def run(command, **kwargs):
                name = command[3]
                self.assertEqual(command[-1], "agent-performance-jsonl")
                self.assertNotIn("--include-descendants", command)
                if name == "slow":
                    raise perf.subprocess.TimeoutExpired(command, 1)
                if name == "old":
                    return perf.subprocess.CompletedProcess(command, 1)
                kwargs["stdout"].write(trace(prompt()).getvalue())
                return perf.subprocess.CompletedProcess(command, 0)

            args = mock.Mock(agents_dir=agents, tau="tau", timeout=1)
            counts = collections.Counter()
            with mock.patch.object(perf.subprocess, "run", side_effect=run):
                samples = perf.scan(args, START, START + perf.DAY_US, counts)
            self.assertEqual(len(samples), 1)
            self.assertEqual(counts["discovered_journals"], 3)
            self.assertEqual(counts["failed_journals"], 1)
            self.assertEqual(counts["timed_out_journals"], 1)

    def test_default_now_captured_once_includes_today_and_artifacts_are_redacted(self):
        now = dt.datetime(2026, 10, 1, 13, 25, 12, 123456, tzinfo=perf.UTC)
        with tempfile.TemporaryDirectory() as root:
            out = pathlib.Path(root) / "report"

            def scan(args, since, until, counts):
                self.assertEqual(until, perf.instant(now.isoformat()))
                self.assertEqual(since, until - 14 * perf.DAY_US)
                return [(START + 12 * 3600 * 1_000_000, "provider", "model-v1", 2, 5)]

            with mock.patch.object(perf.dt, "datetime", wraps=dt.datetime) as clock:
                clock.now.return_value = now
                with mock.patch.object(perf, "scan", side_effect=scan), contextlib.redirect_stdout(io.StringIO()):
                    perf.main(["--out", str(out)])
                clock.now.assert_called_once_with(perf.UTC)
            with (out / "performance.csv").open() as stream:
                row = next(csv.DictReader(stream))
            self.assertEqual(row["bucket_end"], now.isoformat().replace("+00:00", "Z"))
            self.assertEqual(row["rate_samples"], "1")
            self.assertEqual({p.name for p in out.iterdir()},
                             {"README.md", "summary.txt", "performance.csv", "latency.svg", "throughput.svg"})
            for path in out.iterdir():
                self.assertNotIn("PRIVATE", path.read_text())
            self.assertEqual(out.stat().st_mode & 0o777, 0o700)

    def test_invalid_ranges_and_existing_output_rejected(self):
        with tempfile.TemporaryDirectory() as root:
            for extra in [[], ["--since", "2026-10-01"], ["--since", "2026-10-02T00:00:00Z",
                                                        "--until", "2026-10-01T00:00:00Z"]]:
                with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
                    perf.main(["--out", root, *extra])


if __name__ == "__main__":
    unittest.main()
