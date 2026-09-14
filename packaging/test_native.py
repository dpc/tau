#!/usr/bin/env python3
"""Unit checks run without containers, nFPM, network, or candidate execution."""

import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import native


HEADER = """  Class: ELF64
  Data: 2's complement, little endian
  Machine: Advanced Micro Devices X86-64
"""
PROGRAM = "      [Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]"
DYNAMIC = """ 0x1 (NEEDED) Shared library: [libc.so.6]
 0x1 (NEEDED) Shared library: [libgcc_s.so.1]
 0x1 (NEEDED) Shared library: [libpthread.so.0]
 0x1 (NEEDED) Shared library: [libdl.so.2]
"""
VERSIONS = "Name: GLIBC_2.2.5\nName: GLIBC_2.34\nName: GCC_3.0"
NFPM_OUTPUT = "GitVersion:    2.46.3\nGitCommit: unknown\n"


class ElfTests(unittest.TestCase):
    def inspect(self, **changes):
        fields = dict(header=HEADER, program=PROGRAM, dynamic=DYNAMIC,
                      versions=VERSIONS, arch="amd64")
        fields.update(changes)
        return native.inspect_elf(**fields)

    def test_baseline_is_not_runtime_qualification(self):
        result = self.inspect()
        self.assertEqual(result["max_glibc_symbol"], "2.34")
        self.assertEqual(result["runtime_qualification"], "not-performed")

    def test_arm64(self):
        self.assertEqual(self.inspect(
            header=HEADER.replace("Advanced Micro Devices X86-64", "AArch64"),
            program=PROGRAM.replace("/lib64/ld-linux-x86-64.so.2",
                                    "/lib/ld-linux-aarch64.so.1"),
            arch="arm64",
        )["arch"], "arm64")

    def test_rejections(self):
        cases = [
            ({"header": HEADER.replace("ELF64", "ELF32")}, "expected ELF64"),
            ({"arch": "arm64"}, "machine"),
            ({"header": HEADER.replace("little endian", "big endian")}, "little-endian"),
            ({"program": ""}, "GNU interpreter"),
            ({"program": PROGRAM.replace("/lib64/", "/nix/store/abc-glibc/lib/")}, "Nix-store"),
            ({"program": PROGRAM.replace("ld-linux-x86-64", "ld-musl-x86_64")}, "GNU interpreter"),
            ({"dynamic": DYNAMIC + "(RUNPATH) [/nix/store/abc/lib]"}, "Nix-store"),
            ({"dynamic": DYNAMIC + "(RPATH) [/usr/lib]"}, "RPATH/RUNPATH"),
            ({"dynamic": DYNAMIC + "(NEEDED) Shared library: [libssl.so.3]"}, "dependencies"),
            ({"dynamic": ""}, "dependencies"),
            ({"versions": VERSIONS + "\nGLIBC_2.35"}, "glibc symbol"),
            ({"versions": VERSIONS + "\nGLIBC_PRIVATE"}, "glibc ABI"),
            ({"versions": VERSIONS + "\nGLIBC_ABI_DT_RELR"}, "glibc ABI"),
            ({"versions": ""}, "glibc symbol"),
            # The GNU interpreter, dependency and RPATH checks all pass here.
            # Only the broad Nix runtime-reference guard rejects this ELF tag.
            ({"dynamic": DYNAMIC + "(AUDIT) Audit library: [/nix/store/abc/lib/audit.so]"},
             "Nix-store"),
        ]
        for fields, reason in cases:
            with self.subTest(fields=fields), self.assertRaisesRegex(ValueError, reason):
                self.inspect(**fields)

    def test_audit_does_not_execute_binary(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "tau"
            binary.write_bytes(b"non-executable fixture")
            with patch.object(native, "run", side_effect=[
                HEADER, PROGRAM, DYNAMIC, VERSIONS
            ]) as run:
                native.audit(binary, "amd64")
                self.assertTrue(all(c.args[0] == "readelf" for c in run.call_args_list))

    def test_placeholders_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "tau"
            binary.write_bytes(b"__TAU_BUILD_DATE")
            with patch.object(native, "run", side_effect=[
                HEADER, PROGRAM, DYNAMIC, VERSIONS
            ]), self.assertRaisesRegex(ValueError, "placeholder"):
                native.audit(binary, "amd64")


class SourceFixture:
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Packaging Test")
        self.git("config", "user.email", "test@example.invalid")
        nodes = {
            "root": {"inputs": {name: name + "-node" for name in native.EXTERNAL}}
        }
        for name in native.EXTERNAL:
            nodes[name + "-node"] = {"locked": {
                "type": "git", "url": "https://example.invalid/source.git",
                "rev": "a" * 40, "narHash": "sha256-fixture",
            }}
        self.lock = {"root": "root", "nodes": nodes}
        (self.repo / "flake.lock").write_text(json.dumps(self.lock))
        (self.repo / "Cargo.toml").write_text(
            '[workspace.package]\nversion="1.2.3"\nlicense="MPL-2.0"\n'
            'rust-version="1.97"\n'
        )
        (self.repo / "Cargo.lock").write_text("# fixture lock")
        (self.repo / "crates/tau").mkdir(parents=True)
        (self.repo / "crates/tau/Cargo.toml").write_text(
            '[package]\nname="dpc-tau"\nversion.workspace=true\n'
        )
        (self.repo / "LICENSE").write_text("fixture license")
        self.git("add", ".")
        self.git("commit", "-qm", "fixture")
        self.sha = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.repo), *args], text=True,
        )


class SourceTests(SourceFixture, unittest.TestCase):
    def test_explicit_application_version_overrides_workspace(self):
        (self.repo / "crates/tau/Cargo.toml").write_text(
            '[package]\nname="dpc-tau"\nversion="1.2.4"\n'
        )
        self.git("add", ".")
        self.git("commit", "-qm", "explicit version")
        sha = self.git("rev-parse", "HEAD").strip()
        self.assertEqual(native.inventory(self.repo, sha)["core"]["version"], "1.2.4")
        native.release_version("v1.2.4", native.inventory(self.repo, sha)["core"]["version"])
        with self.assertRaises(ValueError):
            native.release_version("v1.2.3", native.inventory(self.repo, sha)["core"]["version"])

    def test_release_tag_must_exactly_match_workspace_version(self):
        self.assertEqual(native.release_version("v1.2.3", "1.2.3"), "1.2.3")
        for tag in ("1.2.3", "v1.2.4", "v1.2.3-extra", "release-v1.2.3"):
            with self.subTest(tag=tag), self.assertRaisesRegex(ValueError, "exactly match"):
                native.release_version(tag, "1.2.3")

    def test_github_prerelease_matches_semver_prerelease_component(self):
        for version, expected in (
            ("1.2.3", False),
            ("1.2.3+build.1", False),
            ("1.2.3-rc.1", True),
            ("1.2.3-rc.1+build.1", True),
        ):
            with self.subTest(version=version):
                self.assertEqual(native.release_is_prerelease(version), expected)

    def test_exact_source_not_dirty_worktree(self):
        (self.repo / "flake.lock").write_text("not json")
        result = native.inventory(self.repo, self.sha)
        self.assertEqual(result["core"]["version"], "1.2.3")
        self.assertEqual(len(result["external"]), 7)
        self.assertEqual(sum(len(p["binaries"]) for p in result["external"]), 8)
        self.assertEqual(result["source_sha"], self.sha)
        self.assertEqual(result["purpose"], "non-release-candidate")

    def test_missing_input_fails_not_partial_inventory(self):
        del self.lock["nodes"]["root"]["inputs"]["tau-ext-zulip"]
        (self.repo / "flake.lock").write_text(json.dumps(self.lock))
        self.git("add", ".")
        self.git("commit", "-qm", "missing")
        with self.assertRaises(KeyError):
            native.inventory(self.repo, self.git("rev-parse", "HEAD").strip())

    def test_arbitrary_ref_and_shell_input_rejected(self):
        for value in ["HEAD", self.sha[:12], "a" * 39, "A" * 40, "$(touch /tmp/no)"]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                native.inventory(self.repo, value)

    def test_replacement_objects_cannot_change_inventory(self):
        original = native.inventory(self.repo, self.sha)
        (self.repo / "Cargo.toml").write_text(
            '[workspace.package]\nversion="9.9.9"\nlicense="MIT"\n'
            'rust-version="1.97"\n'
        )
        self.git("add", ".")
        self.git("commit", "-qm", "replacement")
        replacement = self.git("rev-parse", "HEAD").strip()
        self.git("replace", self.sha, replacement)
        self.assertIn("9.9.9", self.git("show", f"{self.sha}:Cargo.toml"))
        self.assertEqual(native.inventory(self.repo, self.sha), original)

    def test_package_failure_does_not_publish_partial_output(self):
        binary = self.repo / "binary"
        binary.write_bytes(b"fixture")
        output = self.repo / "output"
        real_run = native.run
        real_subprocess_run = subprocess.run
        formats = []

        def tool_run(*args):
            return NFPM_OUTPUT if args[0] == "nfpm" else real_run(*args)

        def package_run(args, **kwargs):
            if args[0] != "nfpm":
                return real_subprocess_run(args, **kwargs)
            fmt = args[args.index("--packager") + 1]
            formats.append(fmt)
            target = Path(args[args.index("--target") + 1])
            if fmt == "rpm":
                self.assertTrue((target.parent / "tau-test-amd64.deb").is_file())
                raise subprocess.CalledProcessError(1, args)
            target.write_bytes(b"first staged package")

        with patch.object(native, "run", side_effect=tool_run), \
                patch.object(native, "audit", return_value={"test_fixture_only": True}), \
                patch.object(subprocess, "run", side_effect=package_run):
            with self.assertRaises(subprocess.CalledProcessError):
                native.package(self.repo, self.sha, binary, "amd64", output,
                               "Test <test@example.invalid>")
        self.assertEqual(formats, ["deb", "rpm"])
        self.assertFalse(output.exists())
        self.assertFalse(list(self.repo.glob(".tau-package-*")))

    def test_complete_candidate_has_no_activation_and_checksums(self):
        binary = self.repo / "binary"
        binary.write_bytes(b"fixture only, not a real ELF")
        output = self.repo / "output"
        real_run = native.run
        real_subprocess_run = subprocess.run
        configs = []

        def tool_run(*args):
            if args[0] == "nfpm":
                return NFPM_OUTPUT
            return real_run(*args)

        def package_run(args, **kwargs):
            if args[0] != "nfpm":
                return real_subprocess_run(args, **kwargs)
            config = json.loads(Path(args[args.index("--config") + 1]).read_text())
            configs.append(config)
            Path(args[args.index("--target") + 1]).write_bytes(b"package fixture")

        with patch.object(native, "run", side_effect=tool_run), \
                patch.object(native, "audit", return_value={"runtime_qualification": "not-performed"}), \
                patch.object(subprocess, "run", side_effect=package_run):
            native.package(self.repo, self.sha, binary, "amd64", output,
                           "Test <test@example.invalid>")
        self.assertEqual(len(configs), 2)
        for config in configs:
            self.assertNotIn("scripts", config)
            self.assertEqual(
                {p["dst"] for p in config["contents"]},
                {"/usr/bin/tau", "/usr/share/licenses/tau/LICENSE",
                 "/usr/share/doc/tau/source-manifest.json"},
            )
            self.assertEqual(config["version"], f"0.0.0~test.{self.sha}")
        for line in (output / "SHA256SUMS").read_text().splitlines():
            digest, name = line.split("  ")
            self.assertEqual(digest, native.sha256((output / name).read_bytes()))
        self.assertEqual(len(list(output.iterdir())), 5)
        with tarfile.open(output / "tau-test-amd64.tar.gz") as archive:
            self.assertEqual(set(archive.getnames()), {
                "tau-test-amd64/tau", "tau-test-amd64/LICENSE",
                "tau-test-amd64/source-manifest.json",
            })
            epoch = native.inventory(self.repo, self.sha)["source_date_epoch"]
            for member in archive.getmembers():
                self.assertEqual(member.uid, 0)
                self.assertEqual(member.gid, 0)
                self.assertEqual(member.uname, "")
                self.assertEqual(member.gname, "")
                self.assertEqual(member.mtime, epoch)

    def test_tagged_release_uses_source_version_and_release_inventory(self):
        binary = self.repo / "binary"
        binary.write_bytes(b"fixture only, not a real ELF")
        output = self.repo / "output"
        real_run = native.run
        real_subprocess_run = subprocess.run
        configs = []

        def tool_run(*args):
            return NFPM_OUTPUT if args[0] == "nfpm" else real_run(*args)

        def package_run(args, **kwargs):
            if args[0] != "nfpm":
                return real_subprocess_run(args, **kwargs)
            configs.append(json.loads(Path(args[args.index("--config") + 1]).read_text()))
            Path(args[args.index("--target") + 1]).write_bytes(b"package fixture")

        with patch.object(native, "run", side_effect=tool_run), \
                patch.object(native, "audit",
                             return_value={"runtime_qualification": "not-performed"}), \
                patch.object(subprocess, "run", side_effect=package_run):
            native.package(self.repo, self.sha, binary, "amd64", output,
                           "Tau project maintainers", "v1.2.3")

        self.assertEqual([config["version"] for config in configs], ["1.2.3", "1.2.3"])
        self.assertEqual([config["version_schema"] for config in configs], ["semver", "semver"])
        self.assertEqual(
            {path.name for path in output.iterdir()},
            {
                "SHA256SUMS", "source-manifest.json",
                "tau-1.2.3-amd64.deb", "tau-1.2.3-amd64.rpm",
                "tau-1.2.3-amd64.tar.gz",
            },
        )
        manifest = json.loads((output / "source-manifest.json").read_text())
        self.assertEqual(manifest["purpose"], "tagged-release")
        self.assertEqual(manifest["release_tag"], "v1.2.3")
        self.assertFalse(manifest["github_prerelease"])
        self.assertNotIn("Not a release", manifest["limitations"])
        with tarfile.open(output / "tau-1.2.3-amd64.tar.gz") as archive:
            self.assertEqual(
                set(archive.getnames()),
                {
                    "tau-1.2.3-amd64/tau",
                    "tau-1.2.3-amd64/LICENSE",
                    "tau-1.2.3-amd64/source-manifest.json",
                },
            )


class ToolVersionTests(unittest.TestCase):
    def test_exact_stable_nfpm_only(self):
        native.check_nfpm_version(NFPM_OUTPUT)
        for output in [
            "GitVersion: 2.46.3-rc1", "GitVersion: 2.46.3+build",
            "GitVersion: 2.46.30", "GitVersion: 2.46.2",
            "other tool mentions 2.46.3", "nfpm version 2.46.3",
            NFPM_OUTPUT + "GitVersion: 2.46.3\n",
        ]:
            with self.subTest(output=output), self.assertRaisesRegex(ValueError, "stable nFPM"):
                native.check_nfpm_version(output)


class RepositoryOracleTests(unittest.TestCase):
    def test_committed_external_source_inventory(self):
        repo = Path(__file__).resolve().parent.parent
        sha = native.run("git", "--no-replace-objects", "-C", str(repo),
                         "rev-parse", "HEAD").strip()
        expected = {
            "tau-ext-pim": ["tau-ext-pim"],
            "tau-ext-rostra": ["tau-ext-rostra"],
            "tau-ext-slack": ["tau-ext-slack"],
            "tau-ext-swarm": ["tau-ext-swarm"],
            "tau-ext-telegram": ["tau-ext-telegram", "tau-telegram-gateway"],
            "tau-ext-xmpp": ["tau-ext-xmpp"],
            "tau-ext-zulip": ["tau-ext-zulip"],
        }
        lock = json.loads(native.source_file(repo, sha, "flake.lock"))
        roots = lock["nodes"][lock["root"]]["inputs"]
        self.assertEqual({name for name in roots if name.startswith("tau-ext-")}, set(expected))
        inventory = native.inventory(repo, sha)["external"]
        self.assertEqual({p["input"]: p["binaries"] for p in inventory}, expected)
        for item in inventory:
            locked = lock["nodes"][roots[item["input"]]]["locked"]
            self.assertEqual(item["source_sha"], locked["rev"])
            self.assertEqual(item["url"], locked["url"])
            self.assertEqual(item["nar_hash"], locked["narHash"])


if __name__ == "__main__":
    unittest.main()
