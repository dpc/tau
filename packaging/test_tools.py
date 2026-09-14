#!/usr/bin/env python3
"""Real nFPM format tests with inert payloads; NOT runtime qualification."""

import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import native
import complete
import distribution
import test_complete
from test_native import SourceFixture


class PackageToolTests(SourceFixture, unittest.TestCase):
    def test_real_complete_metapackage_metadata_and_revision_ordering(self):
        sources, builds = test_complete.AssemblyTests.prepare(self)
        output = self.repo / "complete-assets"
        with patch.object(native, "audit", return_value={"test_fixture_only": True}):
            complete.assemble(native.inventory(self.repo, self.sha), sources, builds,
                              builds / "tau/target/release/tau", "amd64", output,
                              "Inert complete distribution fixture", True)
        products = distribution.components("1.2.3")
        deb = str(output / "tau-full-1.2.3-amd64.deb")
        rpm = str(output / "tau-full-1.2.3-amd64.rpm")
        versions = {p["name"]: "-".join(complete.package_version(p, "1.2.3", self.sha, True))
                    for p in products}
        self.assertEqual(
            {s.strip() for s in native.run("dpkg-deb", "-f", deb, "Depends").split(",")},
            {f"{name} (= {version})" for name, version in versions.items()},
        )
        self.assertEqual(
            {s for s in native.run("rpm", "-qp", "--requires", rpm).splitlines()
             if not s.startswith("rpmlib(")},
            {f"{name} = {version}" for name, version in versions.items()},
        )
        self.assertEqual(native.run("rpm", "-qp", "--qf", "[%{FILENAMES}\n]", rpm), "")
        for earlier, later in [
            ("0.1.0-1.tau0.1.9", "0.1.0-1.tau0.1.10"),
            ("0.1.0-1.tau0.1.1~rc.1", "0.1.0-1.tau0.1.1"),
            ("0.0.0~test." + self.sha + "-1", "0.0.0"),
        ]:
            subprocess.run(["dpkg", "--compare-versions", earlier, "lt", later], check=True)
            self.assertEqual(native.run(
                "rpm", "--eval", f'%{{lua:print(rpm.vercmp("{earlier}", "{later}"))}}'
            ).strip(), "-1")

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

    def test_prerelease_formats_and_release_maintainer(self):
        (self.repo / "Cargo.toml").write_text(
            '[workspace.package]\nversion="1.2.3-rc.1"\nlicense="MPL-2.0"\n'
            'rust-version="1.97"\n'
        )
        self.git("add", "Cargo.toml")
        self.git("commit", "-qm", "prerelease fixture")
        self.sha = self.git("rev-parse", "HEAD").strip()
        binary = self.repo / "binary"
        binary.write_bytes(b"Inert packaging fixture, not Tau or an ELF executable\n")
        output = self.repo / "release-assets"
        maintainer = "Dawid Ciężarkiewicz <dpc@dpc.pw>"
        with patch.object(native, "audit", return_value={"test_fixture_only": True}):
            native.package(
                self.repo, self.sha, binary, "amd64", output,
                maintainer, "v1.2.3-rc.1",
            )
        deb = output / "tau-1.2.3-rc.1-amd64.deb"
        rpm = output / "tau-1.2.3-rc.1-amd64.rpm"
        deb_version = native.run("dpkg-deb", "-f", str(deb), "Version").strip()
        rpm_version = native.run(
            "rpm", "-qp", "--qf", "%{VERSION}-%{RELEASE}", str(rpm)
        )
        self.assertEqual(deb_version, "1.2.3~rc.1-1")
        self.assertEqual(rpm_version, "1.2.3~rc.1-1")
        self.assertEqual(
            native.run("dpkg-deb", "-f", str(deb), "Maintainer").strip(),
            maintainer,
        )
        manifest = json.loads((output / "source-manifest.json").read_text())
        self.assertTrue(manifest["github_prerelease"])
        subprocess.run(
            ["dpkg", "--compare-versions", deb_version, "lt", "1.2.3-1"],
            check=True,
        )
        self.assertEqual(
            native.run(
                "rpm", "--eval",
                f'%{{lua:print(rpm.vercmp("{rpm_version}", "1.2.3-1"))}}',
            ).strip(),
            "-1",
        )


if __name__ == "__main__":
    unittest.main()
