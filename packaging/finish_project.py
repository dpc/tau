#!/usr/bin/env python3
"""Generate offline notices, then retain payloads instead of compiler scratch."""

import argparse
from pathlib import Path
import shutil
import subprocess

import distribution


def finish(project_name, target, work=Path("/work"), source=Path("/source"),
           tooling=Path("/tooling")):
    project = next(p for p in distribution.projects() if p["name"] == project_name)
    subprocess.run([
        "cargo-about", "generate", "--frozen", "--fail",
        "--config", str(tooling / "about.toml"),
        "--manifest-path", str(source / project["manifest"]),
        "--target", target, str(tooling / "notices.hbs"),
        "--output-file", str(work / "THIRD_PARTY_NOTICES.html"),
    ], check=True)
    retained = work / "retained"
    retained.mkdir()
    # This runs inside a fresh offline container, so even a selected-source
    # symlink can resolve only within its non-secret filesystem, not the host.
    for binary in project["binaries"]:
        shutil.copyfile(work / "target/release" / binary, retained / binary)
        (retained / binary).chmod(0o755)
    for name in ("cargo", "home", "target"):
        path = work / name
        if path.is_symlink():
            path.unlink()
        elif path.exists():
            shutil.rmtree(path)
    (work / "target").mkdir()
    retained.rename(work / "target/release")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project-name", required=True)
    parser.add_argument("--target", required=True)
    finish(**vars(parser.parse_args()))
