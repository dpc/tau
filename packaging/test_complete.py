#!/usr/bin/env python3
"""Complete-distribution contracts; inert fixtures are not runtime evidence."""

import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import complete
import distribution
import native
import release_assets
import finish_project
from test_native import SourceFixture, NFPM_OUTPUT


class DistributionTests(unittest.TestCase):
    def test_project_retains_only_payload_after_successful_offline_notices(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "target/release").mkdir(parents=True)
            (root / "target/release/tau").write_bytes(b"fixture binary")
            (root / "target/release/compiler-scratch").write_bytes(b"discard")
            (root / "cargo").mkdir()
            (root / "cargo/cache").write_text("discard")
            with patch.object(finish_project.subprocess, "run") as run:
                finish_project.finish("tau", "x86_64-unknown-linux-gnu", work=root)
            self.assertIn("--frozen", run.call_args.args[0])
            self.assertIn("--fail", run.call_args.args[0])
            self.assertEqual((root / "target/release/tau").read_bytes(), b"fixture binary")
            self.assertFalse((root / "cargo").exists())
            self.assertEqual({p.name for p in (root / "target/release").iterdir()}, {"tau"})
    def test_inventory_is_complete_and_architecture_specific(self):
        components = distribution.components("0.1.1")
        self.assertEqual(len(distribution.projects()), 8)
        self.assertEqual(len(components), 9)
        self.assertEqual(sum(p["project"] == "tau-ext-telegram" for p in components), 2)
        for arch in native.ARCHES:
            assets = distribution.package_assets("0.1.1", arch, True)
            self.assertEqual(len(assets), 30)
            self.assertIn(f"tau-full-0.1.1-{arch}.deb", assets)
            self.assertIn(f"tau-ext-pim-0.1.0-tau-0.1.1-{arch}.tar.gz", assets)
            self.assertEqual(len(release_assets.names("0.1.1", arch)), 33)

    def test_upstream_and_release_revision_are_distinct(self):
        component = {"version": "0.1.0"}
        self.assertEqual(complete.package_version(component, "0.1.1", "a" * 40, True),
                         ("0.1.0", "1.tau0.1.1"))
        self.assertEqual(complete.package_version(component, "0.1.2", "a" * 40, True),
                         ("0.1.0", "1.tau0.1.2"))
        self.assertEqual(complete.package_version(component, "0.1.2-rc.1", "a" * 40, True),
                         ("0.1.0", "1.tau0.1.2~rc.1"))
        self.assertEqual(complete.package_version(component, "0.1.1", "a" * 40, False),
                         ("0.0.0~test." + "a" * 40, "1"))

    def test_archive_readback_checks_content_modes_and_inventory(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            payload = root / "payload"
            (payload / "bin").mkdir(parents=True)
            binary = payload / "bin/tau"
            binary.write_bytes(b"inert")
            binary.chmod(0o755)
            complete.archive(payload, root / "good.tar.gz", "bundle", 0)
            for name, kind, mode, content in [
                ("../escape", tarfile.REGTYPE, 0o755, b"inert"),
                ("bundle/bin/tau", tarfile.SYMTYPE, 0o755, b""),
                ("bundle/bin/tau", tarfile.REGTYPE, 0o644, b"inert"),
                ("bundle/bin/tau", tarfile.REGTYPE, 0o755, b"wrong"),
            ]:
                with self.subTest(name=name, mode=mode, content=content):
                    archive = root / "input.tar.gz"
                    with tarfile.open(archive, "w:gz") as output:
                        info = tarfile.TarInfo(name)
                        info.type = kind
                        info.mode = mode
                        info.size = len(content)
                        output.addfile(info, io.BytesIO(content))
                    with self.assertRaisesRegex(ValueError, "archive payload"):
                        complete.verify_archive(payload, archive, "bundle")


class AssemblyTests(SourceFixture, unittest.TestCase):
    def prepare(self):
        sources, builds = self.repo / "sources", self.repo / "builds"
        for project in distribution.projects():
            name = project["name"]
            source = sources / name
            source.mkdir(parents=True)
            manifest = source / project["manifest"]
            manifest.parent.mkdir(parents=True, exist_ok=True)
            content = (
                f'[package]\nname="{project["package"]}"\n'
                f'version="{project.get("version", "1.2.3")}"\nlicense="MPL-2.0"\n'
            )
            manifest.write_text(content)
            (source / "Cargo.toml").write_text(content)
            (source / "Cargo.lock").write_text('[[package]]\nname="dpc-tau-proto"\nversion="0.4.0"\n')
            (source / "LICENSE").write_text("project license fixture")
            build = builds / name
            (build / "target/release").mkdir(parents=True)
            (build / "THIRD_PARTY_NOTICES.html").write_text("full license fixture " * 10)
            for binary in project["binaries"]:
                (build / "target/release" / binary).write_text(f"inert {binary}")
        return sources, builds

    def test_complete_payload_dependencies_notices_and_no_duplicate_ownership(self):
        sources, builds = self.prepare()
        manifest = native.inventory(self.repo, self.sha)
        output = self.repo / "output"
        configs = []

        def nfpm(config, base, stage, assets, epoch):
            configs.append(config)
            for fmt in ("deb", "rpm"):
                (assets / f"{base}.{fmt}").write_text(json.dumps(config))

        with patch.object(native, "run", return_value=NFPM_OUTPUT), \
                patch.object(native, "audit", return_value={"arch": "amd64"}), \
                patch.object(complete, "nfpm", side_effect=nfpm):
            complete.assemble(manifest, sources, builds, builds / "tau/target/release/tau",
                              "amd64", output, "fixture", True)
        self.assertEqual(len(configs), 10)
        full = configs[-1]
        self.assertEqual(full["name"], "tau-full")
        self.assertEqual(full["contents"], [])
        self.assertEqual(len(full["overrides"]["deb"]["depends"]), 9)
        self.assertIn("tau-ext-pim (= 0.1.0-1.tau1.2.3)",
                      full["overrides"]["deb"]["depends"])
        owned = [file["dst"] for config in configs for file in config["contents"]]
        self.assertEqual(len(owned), len(set(owned)))
        self.assertEqual(len(owned), 36)
        self.assertFalse(any("scripts" in config for config in configs))
        with tarfile.open(output / "tau-full-1.2.3-amd64.tar.gz") as archive:
            self.assertEqual(len(archive.getmembers()), 36)
            self.assertTrue(all(m.isfile() and m.uid == m.gid == 0 for m in archive.getmembers()))
        reports = json.loads((output / "source-manifest.json").read_text())["components"]
        self.assertEqual(len(reports), 9)
        self.assertTrue(all(len(p["third_party_notices_sha256"]) == 64 for p in reports))

    def test_unreviewed_upstream_version_and_missing_notice_fail_closed(self):
        sources, builds = self.prepare()
        project = distribution.projects()[1]
        cargo = sources / project["name"] / "Cargo.toml"
        cargo.write_text(cargo.read_text().replace('version="0.1.0"', 'version="0.1.2"'))
        with self.assertRaisesRegex(ValueError, "upstream version"):
            complete.source_metadata(sources / project["name"], project)
        cargo.write_text(cargo.read_text().replace('version="0.1.2"', 'version="0.1.0"'))
        (builds / "tau/THIRD_PARTY_NOTICES.html").unlink()
        manifest = native.inventory(self.repo, self.sha)
        with patch.object(native, "run", return_value=NFPM_OUTPUT), \
                self.assertRaisesRegex(ValueError, "license closure"):
            complete.assemble(manifest, sources, builds,
                              builds / "tau/target/release/tau", "amd64",
                              self.repo / "output", "fixture", False)
        self.assertFalse((self.repo / "output").exists())


class PublicationTests(SourceFixture, unittest.TestCase):
    def candidate(self):
        output = self.repo / "release"
        output.mkdir()
        manifest = native.inventory(self.repo, self.sha)
        manifest.update(purpose="tagged-release", release_tag="v1.2.3")
        products = distribution.components("1.2.3")
        pins_raw = Path(__file__).with_name("build-inputs.json").read_bytes()
        pins = json.loads(pins_raw)
        for arch in native.ARCHES:
            hashes = {}
            for name in distribution.package_assets("1.2.3", arch, True):
                (output / name).write_text("inert fixture")
                hashes[name] = native.sha256((output / name).read_bytes())
            components = []
            for product in products:
                version, revision = complete.package_version(product, "1.2.3", self.sha, True)
                core = product["name"] == "tau"
                source = ({"source_sha": self.sha} if core else next(
                    p for p in manifest["external"] if p["input"] == product["project"]))
                files = {"Cargo.toml", "Cargo.lock", "LICENSE"}
                if core:
                    files.add("crates/tau/Cargo.toml")
                components.append({**product, "elf": {
                    "arch": arch, "binary_sha256": "a" * 64,
                    "interpreter": native.ARCHES[arch][1],
                    "needed": ["libc.so.6"], "max_glibc_symbol": "2.34",
                    "runtime_qualification": "not-performed",
                }, "source": source, "source_file_sha256": dict.fromkeys(files, "a" * 64),
                    "third_party_notices_sha256": "b" * 64,
                    "sdk_lock_versions": {n: ["0.5.0" if core else "0.4.0"]
                                          for n in ("dpc-tau-client", "dpc-tau-proto")},
                                   "package_version": version, "package_revision": revision})
            prefix = f"tau-1.2.3-{arch}"
            native.write_json(output / f"{prefix}-source-manifest.json", {
                **manifest, "components": components,
                "distribution_sha256": native.sha256(distribution.INVENTORY.read_bytes()),
            })
            native.write_json(output / f"{prefix}-toolchain.json", {
                "rustc": "rustc 1.97.0 (fixture)", "cargo": "cargo 1.97.0 (fixture)",
                "cc": "fixture", "readelf": "fixture", "nfpm": NFPM_OUTPUT,
                "cargo_about": "cargo-about 0.9.0",
            })
            native.write_json(output / f"{prefix}-build-manifest.json", {
                "purpose": "tagged-github-release", "source_sha": self.sha,
                "workflow_sha": self.sha, "release_tag": "v1.2.3", "arch": arch,
                "github_prerelease": False, "projects": distribution.projects(),
                "build_inputs": pins, "build_inputs_sha256": native.sha256(pins_raw),
                "derived_image_id": "sha256:" + "a" * 64,
                "metadata_sha256": {
                    f"{name}.json": native.sha256((output / f"{prefix}-{name}.json").read_bytes())
                    for name in ("source-manifest", "toolchain")
                },
                "runtime_qualification": "not-performed",
                "package_asset_sha256": hashes,
            })
        return output

    def test_complete_assets_verify_and_any_omission_or_corruption_fails(self):
        output = self.candidate()
        self.assertEqual(len(release_assets.verify(output, self.repo, self.sha, "v1.2.3")), 66)
        asset = output / "tau-ext-pim-0.1.0-tau-1.2.3-arm64.deb"
        original = asset.read_bytes()
        asset.unlink()
        with self.assertRaisesRegex(ValueError, "inventory"):
            release_assets.verify(output, self.repo, self.sha, "v1.2.3")
        asset.write_bytes(original + b"corrupt")
        with self.assertRaisesRegex(ValueError, "provenance"):
            release_assets.verify(output, self.repo, self.sha, "v1.2.3")
        asset.write_bytes(original)
        buildfile = output / "tau-1.2.3-arm64-build-manifest.json"
        build = json.loads(buildfile.read_text())
        build["workflow_sha"] = "b" * 40
        native.write_json(buildfile, build)
        with self.assertRaisesRegex(ValueError, "provenance"):
            release_assets.verify(output, self.repo, self.sha, "v1.2.3")

    def test_runtime_qualification_is_not_claimed(self):
        output = self.candidate()
        buildfile = output / "tau-1.2.3-amd64-build-manifest.json"
        build = json.loads(buildfile.read_text())
        build["runtime_qualification"] = "passed"
        native.write_json(buildfile, build)
        with self.assertRaisesRegex(ValueError, "provenance"):
            release_assets.verify(output, self.repo, self.sha, "v1.2.3")

    def test_metadata_corruption_and_weakened_qualification_fail(self):
        output = self.candidate()
        sourcefile = output / "tau-1.2.3-amd64-source-manifest.json"
        original = sourcefile.read_bytes()
        for field in ("source", "source_file_sha256", "third_party_notices_sha256", "elf"):
            source = json.loads(original)
            del source["components"][1][field]
            native.write_json(sourcefile, source)
            with self.subTest(field=field), self.assertRaises((ValueError, KeyError)):
                release_assets.verify(output, self.repo, self.sha, "v1.2.3")
        sourcefile.write_bytes(original)
        toolchain = output / "tau-1.2.3-amd64-toolchain.json"
        toolchain.write_text("{}")
        with self.assertRaises((ValueError, KeyError)):
            release_assets.verify(output, self.repo, self.sha, "v1.2.3")


if __name__ == "__main__":
    unittest.main()
