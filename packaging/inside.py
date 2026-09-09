#!/usr/bin/env python3
"""Trusted package assembly in a fresh, network-disabled native container."""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import shutil

import native


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


def assemble(source_sha, arch, maintainer):
    manifest = native.inventory(Path("/source"), source_sha)
    staged = Path("/work/tau")
    # /build is read-only in this fresh container. Even hostile symlinks resolve
    # only within this container's non-secret filesystem, not the host.
    staged.write_bytes(stamp(
        Path("/build/target/release/tau").read_bytes(),
        source_sha, manifest["source_date_epoch"],
    ))
    staged.chmod(0o755)
    native.package(Path("/source"), source_sha, staged, arch,
                   Path("/output/packages"), maintainer)
    # Keep a copy for the separate no-network version probe; do not execute
    # candidate code in the assembly process/container.
    shutil.copyfile(staged, "/output/tau")
    Path("/output/tau").chmod(0o755)
    Path("/output/toolchain.json").write_text(json.dumps({
        "rustc": native.run("rustc", "--version", "--verbose"),
        "cargo": native.run("cargo", "--version"),
        "cc": native.run("cc", "--version"),
        "readelf": native.run("readelf", "--version"),
        "nfpm": native.run("nfpm", "--version"),
    }, indent=2) + "\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-sha", required=True, type=native.require_sha)
    parser.add_argument("--arch", required=True, choices=native.ARCHES)
    parser.add_argument("--maintainer", required=True)
    assemble(**vars(parser.parse_args()))
