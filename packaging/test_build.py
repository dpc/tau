#!/usr/bin/env python3
"""Native build orchestration tests; Docker/native execution is not simulated proof."""

import json
from datetime import datetime, timezone
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import build
import inside
import native
from test_native import SourceFixture


class IdentityTests(unittest.TestCase):
    def test_metadata_stamping_preserves_layout(self):
        raw = (b"ELF-prefix\0__TAU_BUILD_GIT_REVISION_PLACEHOLDER____\0"
               b"__TAU_BUILD_DIRTY\0__TAU_BUILD_DATE\0ELF-suffix")
        stamped = inside.stamp(raw, "a" * 40, 0)
        self.assertEqual(len(raw), len(stamped))
        self.assertEqual(stamped, b"ELF-prefix\0" + b"a" * 40 +
                         b"\0clean____________\0001970-01-01 00:00\0ELF-suffix")
        with self.assertRaisesRegex(ValueError, "identity slot"):
            inside.stamp(b"no placeholders", "a" * 40, 0)
        for slot in (
            b"__TAU_BUILD_GIT_REVISION_PLACEHOLDER____",
            b"__TAU_BUILD_DIRTY", b"__TAU_BUILD_DATE",
        ):
            with self.subTest(slot=slot), self.assertRaisesRegex(ValueError, "duplicate"):
                inside.stamp(raw + b"\0" + slot, "a" * 40, 0)
        with self.assertRaisesRegex(ValueError, "source_sha"):
            inside.stamp(raw, "HEAD", 0)

    def test_clean_version_identity_only(self):
        good = "tau 1.2.3 (aaaaaaa, 1970-01-01 00:00)\n"
        build.verify_version(good, "1.2.3", "a" * 40, 0)
        for text in [
            good.replace("aaaaaaa", "bbbbbbb"), good.replace("1.2.3", "1.2.4"),
            good.replace("aaaaaaa", "aaaaaaa-modified"),
            good.replace("1970-01-01 00:00", "1970-01-01 00:01"),
            good + "::set-output name=release::true", "tau --help output",
        ]:
            with self.subTest(text=text), self.assertRaisesRegex(ValueError, "--version"):
                build.verify_version(text, "1.2.3", "a" * 40, 0)


class SnapshotTests(SourceFixture, unittest.TestCase):
    def test_snapshot_ignores_dirty_files_credentials_and_replacements(self):
        original = native.inventory(self.repo, self.sha)
        (self.repo / "Cargo.toml").write_text("dirty caller worktree")
        self.git("config", "credential.helper", "sensitive-helper")
        self.git("config", "http.extraheader", "Authorization: secret-fixture")
        self.git("add", ".")
        self.git("commit", "-qm", "replacement")
        self.git("replace", self.sha, self.git("rev-parse", "HEAD").strip())
        destination = self.repo / "snapshot"
        build.snapshot(self.repo, self.sha, destination)
        self.assertEqual(native.inventory(destination, self.sha), original)
        config = (destination / ".git/config").read_text()
        self.assertNotIn("secret-fixture", config)
        self.assertNotIn("sensitive-helper", config)
        self.assertFalse((destination / ".git/refs/replace").exists())

    def test_tooling_must_match_recorded_workflow_objects(self):
        tools = self.repo / "packaging"
        tools.mkdir(exist_ok=True)
        (tools / "build.py").write_text("trusted fixture")
        self.git("add", ".")
        self.git("commit", "-qm", "tooling")
        workflow_sha = self.git("rev-parse", "HEAD").strip()
        with patch.object(build, "TOOLS", tools), \
                patch.object(build, "TOOL_FILES", ("packaging/build.py",)):
            build.verify_tooling(workflow_sha)
            (tools / "build.py").write_text("changed fixture")
            with self.assertRaisesRegex(ValueError, "tooling differs"):
                build.verify_tooling(workflow_sha)


class PolicyTests(unittest.TestCase):
    def test_both_architectures_have_pinned_native_inputs(self):
        for arch, machine in (("amd64", "x86_64"), ("arm64", "aarch64")):
            pins, selected, digest = build.inputs(arch)
            self.assertEqual(selected["machine"], machine)
            self.assertEqual(selected["rust_target"], f"{machine}-unknown-linux-gnu")
            self.assertEqual(pins["rust_version"], "1.97.0")
            self.assertEqual(pins["nfpm_version"], "2.46.3")
            self.assertEqual(len(digest), 64)
            command = build.image_command(pins, selected, Path("/tmp/per-run/builder.iid"))
            self.assertIn(f"BASE_IMAGE={selected['image']}", command)
            self.assertIn(f"RUST_SHA256={selected['rust_sha256']}", command)
            self.assertIn(f"NFPM_SHA256={selected['nfpm_sha256']}", command)
            self.assertEqual(command[-1], str(build.TOOLS))
            self.assertIn("--iidfile", command)
            self.assertNotIn("--tag", command)
        dockerfile = (build.TOOLS / "Dockerfile").read_text()
        self.assertEqual(dockerfile.count("sha256sum --check --strict"), 3)
        self.assertNotIn("COPY", dockerfile)
        self.assertIn("CARGO_BUILD_JOBS=2", dockerfile)
        self.assertIn("--features cli --bin cargo-about", dockerfile)
        self.assertIn("/opt/cargo-about/bin/cargo-about --version", dockerfile)

    def test_runtime_cannot_mount_credentials_or_docker_socket(self):
        command = build.container_command("image", "name", [
            (Path("/tmp/source"), "/source", True),
            (Path("/tmp/work"), "/work", False),
            (Path("/tmp/tooling"), "/tooling", True),
        ], network=False)
        for flag in ("--read-only", "--network=none", "--cap-drop=ALL",
                     "--security-opt=no-new-privileges", "--memory=12g", "--cpus=2"):
            self.assertIn(flag, command)
        self.assertIn("type=bind,src=/tmp/source,dst=/source,readonly", command)
        self.assertIn("type=bind,src=/tmp/tooling,dst=/tooling,readonly", command)
        self.assertNotIn("--privileged", command)
        self.assertFalse(any("docker.sock" in arg or "GITHUB_TOKEN" in arg for arg in command))
        with self.assertRaisesRegex(ValueError, "commas"):
            build.container_command("image", "name", [(Path("/tmp/a,b"), "/work", False)])

    def test_rootless_docker_uses_namespaced_root_for_bind_ownership(self):
        with patch.object(native, "run", return_value=json.dumps([
                "name=seccomp,profile=builtin", "name=rootless", "name=cgroupns"
        ])):
            self.assertEqual(build.docker_container_user(), "0:0")
        with patch.object(native, "run", return_value=json.dumps(["name=seccomp"])), \
                patch.object(build.os, "getuid", return_value=123), \
                patch.object(build.os, "getgid", return_value=456):
            self.assertEqual(build.docker_container_user(), "123:456")
        for invalid in ("not JSON", "{}", '["name=rootless", 1]'):
            with self.subTest(invalid=invalid), patch.object(native, "run", return_value=invalid), \
                    self.assertRaisesRegex(ValueError, "security options"):
                build.docker_container_user()

    def test_timeout_removes_container(self):
        with patch.object(build, "execute", side_effect=subprocess.TimeoutExpired("docker", 1)), \
                patch.object(subprocess, "run") as run:
            with self.assertRaises(subprocess.TimeoutExpired):
                build.container("image", [], ["fixture"], Path("/tmp/not-used"), 1)
        self.assertEqual(run.call_args.args[0][:3], ["docker", "rm", "--force"])

    def test_manual_workflow_has_no_release_lane_or_candidate_tooling(self):
        # actionlint separately validates YAML/Actions syntax. These checks lock
        # the specific trust choices rather than treating textual YAML as a parser.
        workflow = (build.TOOLS.parent / ".github/workflows/native-candidates.yml").read_text()
        self.assertIn("workflow_dispatch:", workflow)
        self.assertIn("contents: read", workflow)
        self.assertIn("github.ref == 'refs/heads/master'", workflow)
        self.assertEqual(workflow.count("persist-credentials: false"), 2)
        self.assertIn("ref: ${{ github.workflow_sha }}", workflow)
        self.assertIn("ref: ${{ inputs.source_sha }}", workflow)
        self.assertIn("python3 tooling/packaging/build.py", workflow)
        self.assertIn("retention-days: 14", workflow)
        self.assertIn("ubuntu-24.04-arm", workflow)
        for forbidden in ("secrets.", "contents: write", "id-token:", "actions/cache",
                          "self-hosted", "source/packaging", "pull_request_target:", "push:"):
            self.assertNotIn(forbidden, workflow)
        uses = [line.split("uses: ", 1)[1].split()[0]
                for line in workflow.splitlines() if "uses: " in line]
        self.assertEqual(len(uses), 3)
        for action in uses:
            self.assertRegex(action, r"^actions/[a-z-]+@[0-9a-f]{40}$")

    def test_release_workflow_uses_only_tag_source_and_scoped_write_authority(self):
        workflow = (build.TOOLS.parent / ".github/workflows/release.yml").read_text()
        self.assertIn('tags:\n      - "v*"', workflow)
        self.assertNotIn("workflow_dispatch:", workflow)
        self.assertNotIn("inputs.", workflow)
        self.assertIn("ref: ${{ github.sha }}", workflow)
        self.assertIn("SOURCE_SHA: ${{ github.sha }}", workflow)
        self.assertIn("--release-tag \"$RELEASE_TAG\"", workflow)
        self.assertIn("needs: build", workflow)
        self.assertEqual(workflow.count("contents: write"), 1)
        self.assertIn("release_assets.py stage", workflow)
        self.assertIn("python3 packaging/publish.py", workflow)
        self.assertIn('--source-sha "$SOURCE_SHA" --workflow-sha "$SOURCE_SHA"', workflow)
        self.assertIn("Dawid Ciężarkiewicz <dpc@dpc.pw>", workflow)
        uses = [line.split("uses: ", 1)[1].split()[0]
                for line in workflow.splitlines() if "uses: " in line]
        self.assertEqual(len(uses), 4)
        for action in uses:
            self.assertRegex(action, r"^actions/[a-z-]+@[0-9a-f]{40}$")


class OrchestrationTests(SourceFixture, unittest.TestCase):
    def exercise(self, bad_version=False, release=False):
        output = self.repo / "output"
        workflow_sha = self.sha if release else "c" * 40
        calls = []
        real_run = native.run
        real_snapshot = build.snapshot

        def snapshot(repo, sha, destination):
            if isinstance(repo, Path):
                real_snapshot(repo, sha, destination)
            else:
                destination.mkdir()

        def tool_run(*args):
            if args == ("docker", "info", "--format", "{{.Architecture}}"):
                return "x86_64"
            if args == ("docker", "info", "--format", "{{json .SecurityOptions}}"):
                return '["name=rootless"]'
            if args[:3] == ("docker", "image", "inspect"):
                self.fail("a concurrently mutated tag must never select the builder")
            return real_run(*args)

        def image_execute(command, log, timeout):
            self.assertEqual(command[:2], ["docker", "build"])
            log.write_text("trusted image fixture")
            # A concurrent invocation may move any shared tag to another image;
            # only this invocation's private iidfile identifies this build.
            self.assertNotIn("--tag", command)
            iidfile = Path(command[command.index("--iidfile") + 1])
            self.assertEqual(iidfile.parent, log.parent.parent)
            iidfile.write_text("sha256:" + "b" * 64)

        def container(image, mounts, command, log, timeout, **kwargs):
            self.assertEqual(image, "sha256:" + "b" * 64)
            calls.append((mounts, command, kwargs))
            log.write_text("container fixture")
            paths = {dst: src for src, dst, _ in mounts}
            if "/output" in paths:
                packages = paths["/output"] / "packages"
                packages.mkdir()
                for name in build.distribution.package_assets("1.2.3", "amd64", release) | {"source-manifest.json"}:
                    (packages / name).write_text("inert fixture")
                (paths["/output"] / "toolchain.json").write_text("{}")
            if "/probe" in paths:
                epoch = native.inventory(self.repo, self.sha)["source_date_epoch"]
                date = datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y-%m-%d %H:%M")
                log.write_text(json.dumps({"version_output": "bad identity" if bad_version else
                               f"tau 1.2.3 ({self.sha[:7]}, {date})\n"}))

        with patch.object(build, "verify_tooling"), \
                patch.object(build, "snapshot", side_effect=snapshot), \
                patch.object(build, "qualify_distros", return_value={"fixture": True}), \
                patch.object(build.platform, "system", return_value="Linux"), \
                patch.object(build.platform, "machine", return_value="x86_64"), \
                patch.object(build.os, "getuid", return_value=1000), \
                patch.object(build.shutil, "disk_usage", return_value=type("Space", (), {"free": 20 * 1024**3})()), \
                patch.object(native, "run", side_effect=tool_run), \
                patch.object(build, "execute", side_effect=image_execute), \
                patch.object(build, "container", side_effect=container):
            if bad_version:
                with self.assertRaisesRegex(ValueError, "--version"):
                    build.build(self.repo, self.sha, "c" * 40, "amd64", output,
                                "Test <test@example.invalid>", "123", "2")
                self.assertFalse(output.exists())
            else:
                build.build(self.repo, self.sha, workflow_sha, "amd64", output,
                             "Test <test@example.invalid>", "123", "2",
                             "v1.2.3" if release else None)
                report = json.loads((output / "build-manifest.json").read_text())
                self.assertEqual(report["source_sha"], self.sha)
                self.assertEqual(report["workflow_sha"], workflow_sha)
                self.assertEqual(report["runtime_qualification"], "extracted-archive-help-and-Hello-only")
                self.assertEqual(report["run_id"], "123")
                self.assertEqual(report["derived_image_id"], "sha256:" + "b" * 64)
                self.assertEqual(
                    report["purpose"],
                    "tagged-github-release" if release
                    else "manual-non-release-complete-candidate",
                )
                self.assertEqual(report["release_tag"], "v1.2.3" if release else None)
                self.assertEqual(report["github_prerelease"], False if release else None)
                for line in (output / "SHA256SUMS").read_text().splitlines():
                    digest, name = line.split("  ")
                    self.assertEqual(digest, native.sha256((output / name).read_bytes()))
        self.assertFalse(list(self.repo.glob(".tau-native-*")))
        self.assertEqual(len(calls), 26)
        self.assertTrue(all(call[2]["user"] == "0:0" for call in calls))
        self.assertTrue(all(call[2]["network"] is False for call in calls[2:24:3]))
        self.assertTrue(calls[-2][2]["network"] is False)
        self.assertTrue(calls[-1][2]["network"] is False)
        self.assertFalse(any(dst == "/output" for _, dst, _ in calls[-1][0]))
        self.assertIn("--locked", calls[0][1])
        self.assertEqual(calls[0][1], ["cargo", "fetch", "--locked"])

    def test_manual_orchestration_labels_separate_source_and_workflow(self):
        self.exercise()

    def test_bad_version_does_not_publish_assets(self):
        self.exercise(bad_version=True)

    def test_tagged_release_uses_source_version_and_release_labels(self):
        self.exercise(release=True)

    def test_mismatched_release_tag_does_not_build(self):
        with patch.object(build, "verify_tooling"), \
                patch.object(build.platform, "system", return_value="Linux"), \
                patch.object(build.platform, "machine", return_value="x86_64"), \
                patch.object(build.os, "getuid", return_value=1000):
            with self.assertRaisesRegex(ValueError, "exactly match"):
                build.build(self.repo, self.sha, self.sha, "amd64",
                            self.repo / "output", "Tau project maintainers",
                            "123", "1", "v1.2.4")

    def test_release_source_must_equal_tooling_before_any_execution(self):
        with patch.object(build, "verify_tooling") as tooling, \
                self.assertRaisesRegex(ValueError, "identical source_sha"):
            build.build(self.repo, self.sha, "c" * 40, "amd64",
                        self.repo / "output", "fixture", "123", "1", "v1.2.3")
        tooling.assert_not_called()


if __name__ == "__main__":
    unittest.main()
