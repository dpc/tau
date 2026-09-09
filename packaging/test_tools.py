#!/usr/bin/env python3
"""Real nFPM format tests with inert payloads; NOT runtime qualification."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import native
from test_native import SourceFixture


class PackageToolTests(SourceFixture, unittest.TestCase):
    def test_real_formats(self):
        binary = self.repo / "binary"
        binary.write_bytes(b"Inert packaging fixture, not Tau or an ELF executable\n")
        output = self.repo / "assets"
        with patch.object(native, "audit", return_value={"test_fixture_only": True}):
            native.package(self.repo, self.sha, binary, "amd64", output,
                           "Test <test@example.invalid>")
        deb = output / "tau-test-amd64.deb"
        rpm = output / "tau-test-amd64.rpm"
        version = f"0.0.0~test.{self.sha}-1"
        self.assertEqual(native.run("dpkg-deb", "-f", str(deb), "Version").strip(), version)
        self.assertEqual(native.run("dpkg-deb", "-f", str(deb), "Architecture").strip(), "amd64")
        subprocess.run(["dpkg", "--compare-versions", version, "lt", "0.0.0"], check=True)
        self.assertEqual(
            native.run("rpm", "-qp", "--qf", "%{VERSION}-%{RELEASE}", str(rpm)), version
        )
        self.assertEqual(
            native.run("rpm", "--eval", f'%{{lua:print(rpm.vercmp("{version}", "0.0.0"))}}').strip(),
            "-1",
        )
        self.assertEqual(native.run("rpm", "-qp", "--scripts", str(rpm)).strip(), "")
        self.assertEqual(
            set(native.run("rpm", "-qpl", str(rpm)).splitlines()),
            {"/usr/bin/tau", "/usr/share/licenses/tau/LICENSE",
             "/usr/share/doc/tau/source-manifest.json"},
        )
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(["dpkg-deb", "-e", str(deb), tmp], check=True)
            self.assertFalse(
                {"preinst", "postinst", "prerm", "postrm", "triggers"} &
                {p.name for p in Path(tmp).iterdir()}
            )
        with tempfile.TemporaryDirectory() as tmp:
            subprocess.run(["dpkg-deb", "-x", str(deb), tmp], check=True)
            self.assertEqual(
                {str(p.relative_to(tmp)) for p in Path(tmp).rglob("*") if p.is_file()},
                {"usr/bin/tau", "usr/share/licenses/tau/LICENSE",
                 "usr/share/doc/tau/source-manifest.json"},
            )
            self.assertEqual((Path(tmp) / "usr/bin/tau").read_bytes(), binary.read_bytes())
            manifest = json.loads(
                (Path(tmp) / "usr/share/doc/tau/source-manifest.json").read_text()
            )
            self.assertEqual(manifest["source_sha"], self.sha)


if __name__ == "__main__":
    unittest.main()
