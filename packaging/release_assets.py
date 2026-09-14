#!/usr/bin/env python3
"""Fail-closed staging and validation of the complete tagged asset set."""

import argparse
import json
from pathlib import Path
import shutil
import re

import distribution
import native
import complete


METADATA = ("build-manifest", "source-manifest", "toolchain")
DISTRO_CHECKS = {
    "exact-metapackage-dependencies", "dependency-resolution", "payload-ownership",
    "no-lifecycle-scripts", "installed-help", "uninstall", "user-state-preserved",
    "revision-ordering",
}


def digest(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def component_valid(component, product, expected_source, arch):
    source = ({"source_sha": expected_source["source_sha"]} if product["name"] == "tau"
              else next(p for p in expected_source["external"] if p["input"] == product["project"]))
    hashes = component["source_file_sha256"]
    expected_files = {"Cargo.toml", "Cargo.lock", "LICENSE"}
    if product["name"] == "tau":
        expected_files.add("crates/tau/Cargo.toml")
    elf = component["elf"]
    interpreter = native.ARCHES[arch][1]
    allowed = {"libc.so.6", "libm.so.6", "libgcc_s.so.1", "libpthread.so.0",
               "libdl.so.2", Path(interpreter).name}
    sdk = "0.5.0" if product["name"] == "tau" else "0.4.0"
    return all([
        component["source"] == source,
        set(hashes) == expected_files, all(digest(v) for v in hashes.values()),
        digest(component["third_party_notices_sha256"]),
        component["sdk_lock_versions"] == {name: [sdk] for name in ("dpc-tau-client", "dpc-tau-proto")},
        digest(elf["binary_sha256"]), elf["interpreter"] == interpreter,
        elf["runtime_qualification"] == "not-performed",
        set(elf["needed"]) <= allowed, "libc.so.6" in elf["needed"],
        tuple(map(int, elf["max_glibc_symbol"].split("."))) <= (2, 34),
    ])


def names(version, arch):
    return distribution.package_assets(version, arch, True) | {
        f"tau-{version}-{arch}-{name}.json" for name in METADATA
    }


def stage(source, output, version, arch):
    output.mkdir()
    for name in sorted(distribution.package_assets(version, arch, True)):
        shutil.copyfile(source / name, output / name)
    for name in METADATA:
        shutil.copyfile(source / f"{name}.json", output / f"tau-{version}-{arch}-{name}.json")
    if {p.name for p in output.iterdir()} != names(version, arch):
        raise ValueError("incomplete release staging")


def verify(source, repo, source_sha, tag):
    expected_source = native.inventory(repo, source_sha)
    version = native.release_version(tag, expected_source["core"]["version"])
    pins_raw = Path(__file__).with_name("build-inputs.json").read_bytes()
    pins = json.loads(pins_raw)
    expected = set().union(*(names(version, arch) for arch in native.ARCHES))
    if {p.name for p in source.iterdir()} != expected:
        raise ValueError("release assets must exactly match the complete two-architecture inventory")
    if any(p.is_symlink() or not p.is_file() for p in source.iterdir()):
        raise ValueError("release assets must be regular files")
    for arch in native.ARCHES:
        prefix = f"tau-{version}-{arch}"
        build = json.loads((source / f"{prefix}-build-manifest.json").read_text())
        manifest = json.loads((source / f"{prefix}-source-manifest.json").read_text())
        toolchain = json.loads((source / f"{prefix}-toolchain.json").read_text())
        metadata_hashes = build["metadata_sha256"]
        checks = [
            build["purpose"] == "tagged-github-release",
            build["source_sha"] == source_sha, build["workflow_sha"] == source_sha,
            build["release_tag"] == tag, build["arch"] == arch,
            build["github_prerelease"] is native.release_is_prerelease(version),
            manifest["source_sha"] == source_sha,
            manifest["purpose"] == "tagged-release",
            manifest["release_tag"] == tag,
            manifest["source_file_sha256"] == expected_source["source_file_sha256"],
            manifest["core"] == expected_source["core"],
            manifest["external"] == expected_source["external"],
            manifest["distribution_sha256"] == native.sha256(distribution.INVENTORY.read_bytes()),
            set(build["distro_qualification"]) == {"deb", "rpm"},
            set(build["archive_probes"]["hello"]) == set(native.EXTERNAL),
            build["projects"] == distribution.projects(),
            build["build_inputs"] == pins,
            build["build_inputs_sha256"] == native.sha256(pins_raw),
            digest(build["derived_image_id"].removeprefix("sha256:")),
            set(metadata_hashes) == {"source-manifest.json", "toolchain.json"},
            set(toolchain) == {"rustc", "cargo", "cc", "readelf", "nfpm", "cargo_about"},
            toolchain["rustc"].startswith(f"rustc {pins['rust_version']} "),
            toolchain["cargo"].startswith(f"cargo {pins['rust_version']} "),
            toolchain["cargo_about"].strip() == "cargo-about 0.9.0",
            bool(toolchain["cc"].strip()), bool(toolchain["readelf"].strip()),
        ]
        native.check_nfpm_version(toolchain["nfpm"])
        checks.extend(
            digest(value) and native.sha256((source / f"{prefix}-{name}").read_bytes()) == value
            for name, value in metadata_hashes.items()
        )
        products = distribution.components(version)
        components = manifest["components"]
        checks.extend([
            [p["name"] for p in components] == [p["name"] for p in products],
            all(p["elf"]["arch"] == arch for p in components),
            build["archive_probes"]["help"] == sorted(p["name"] for p in products),
        ])
        for component, product in zip(components, products):
            version_revision = complete.package_version(product, version, source_sha, True)
            checks.extend([
                component["version"] == product["version"],
                component["license"] == product["license"],
                (component["package_version"], component["package_revision"]) == version_revision,
                component_valid(component, product, expected_source, arch),
            ])
        installed = {
            p["name"]: "-".join(complete.package_version(p, version, source_sha, True))
            for p in [*products, {"name": "tau-full", "version": version}]
        }
        for fmt, evidence in build["distro_qualification"].items():
            result = evidence["result"]
            checks.extend([
                evidence["base_image"] == pins["qualification_images"][fmt],
                digest(evidence["derived_image_id"].removeprefix("sha256:")),
                result["installed"] == installed, result["format"] == fmt,
                set(result["checks"]) == DISTRO_CHECKS,
                bool(result["baseline_packages"].strip()),
                isinstance(result["install_output"], str), isinstance(result["remove_output"], str),
            ])
        hashes = build["package_asset_sha256"]
        checks.append(set(hashes) == distribution.package_assets(version, arch, True))
        checks.extend(native.sha256((source / name).read_bytes()) == digest
                      for name, digest in hashes.items())
        if not all(checks):
            raise ValueError(f"{arch}: asset provenance/qualification does not match release")
    return sorted(expected)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    staging = commands.add_parser("stage")
    staging.add_argument("--source", type=Path, required=True)
    staging.add_argument("--output", type=Path, required=True)
    staging.add_argument("--version", required=True)
    staging.add_argument("--arch", choices=native.ARCHES, required=True)
    verification = commands.add_parser("verify")
    verification.add_argument("--source", type=Path, required=True)
    verification.add_argument("--repo", type=Path, required=True)
    verification.add_argument("--source-sha", type=native.require_sha, required=True)
    verification.add_argument("--tag", required=True)
    args = vars(parser.parse_args())
    command = args.pop("command")
    if command == "stage":
        stage(**args)
    else:
        print("\n".join(verify(**args)))
