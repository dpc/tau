#!/usr/bin/env python3
"""Trusted package assembly in a fresh, network-disabled native container."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path

import native
import complete


def stamp(data, source_sha, epoch):
    """Fill existing packaging metadata slots only; never relocate ELF linkage."""
    native.require_sha(source_sha)
    replacements = {
        b"__TAU_BUILD_GIT_REVISION_PLACEHOLDER____": source_sha.encode(),
        b"__TAU_BUILD_DIRTY": b"clean____________",
        b"__TAU_BUILD_DATE": datetime.fromtimestamp(epoch, timezone.utc).strftime(
            "%Y-%m-%d %H:%M"
        ).encode(),
    }
    for old, new in replacements.items():
        if len(old) != len(new) or data.count(old) != 1:
            raise ValueError(f"missing, duplicate or incompatible build identity slot: {old!r}")
        data = data.replace(old, new)
    return data


def assemble(source_sha, arch, maintainer, release_tag=None):
    manifest = native.inventory(Path("/sources/tau"), source_sha)
    staged = Path("/work/tau")
    # /build is read-only in this fresh container. Even hostile symlinks resolve
    # only within this container's non-secret filesystem, not the host.
    staged.write_bytes(stamp(
        Path("/builds/tau/target/release/tau").read_bytes(),
        source_sha, manifest["source_date_epoch"],
    ))
    staged.chmod(0o755)
    if release_tag:
        native.release_version(release_tag, manifest["core"]["version"])
        manifest.update(purpose="tagged-release", release_tag=release_tag,
                        github_prerelease=native.release_is_prerelease(manifest["core"]["version"]))
    complete.assemble(manifest, Path("/sources"), Path("/builds"), staged, arch,
                      Path("/output/packages"), maintainer, release_tag is not None)
    Path("/output/toolchain.json").write_text(json.dumps({
        "rustc": native.run("rustc", "--version", "--verbose"),
        "cargo": native.run("cargo", "--version"),
        "cc": native.run("cc", "--version"),
        "readelf": native.run("readelf", "--version"),
        "nfpm": native.run("nfpm", "--version"),
        "cargo_about": native.run("cargo-about", "--version"),
    }, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-sha", required=True, type=native.require_sha)
    parser.add_argument("--arch", required=True, choices=native.ARCHES)
    parser.add_argument("--maintainer", required=True)
    parser.add_argument("--release-tag")
    assemble(**vars(parser.parse_args()))
