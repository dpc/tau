#!/usr/bin/env python3
"""Probe extracted release payloads in a fresh, network-disabled container."""

import argparse
import json
import os
from pathlib import Path
import resource
import signal
import subprocess
import tarfile
import tempfile
import time

import distribution


def limit_output():
    resource.setrlimit(resource.RLIMIT_FSIZE, (1024 * 1024, 1024 * 1024))


def extract(archive, destination):
    with tarfile.open(archive) as source:
        # Only regular files are emitted by the trusted assembler. Never allow
        # symlink traversal, devices or paths outside the extraction directory.
        for member in source.getmembers():
            if (not member.isfile() or member.name.startswith("/")
                    or ".." in Path(member.name).parts):
                raise ValueError("unexpected archive member")
        source.extractall(destination, filter="data")


def handshake(tau, extension, scratch):
    config = scratch / "config/tau"
    config.mkdir(parents=True)
    (config / "harness.yaml").write_text(
        "extensions:\n  release-probe:\n"
        f"    command: [{json.dumps(str(extension))}]\n"
        "    enable: true\n    require: false\n"
        "    config:\n      __release_handshake_probe__: true\n"
    )
    env = {
        **os.environ, "HOME": str(scratch / "home"),
        "XDG_CONFIG_HOME": str(scratch / "config"),
        "XDG_STATE_HOME": str(scratch / "state"),
        "XDG_CACHE_HOME": str(scratch / "cache"),
        "TAU_LOG": "tau_harness=info,warn",
    }
    log = scratch / "stderr"
    with log.open("wb") as stream:
        child = subprocess.Popen([
            str(tau), "--disable-extensions-all", "--enable-extension", "release-probe",
            "serve", "--session", f"probe-{extension.name}", "--create",
        ], env=env, stdin=subprocess.DEVNULL, stdout=stream, stderr=stream,
            start_new_session=True, preexec_fn=limit_output)
        try:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                text = log.read_text(errors="replace")
                if "admitting extension with protocol minor-version skew" in text:
                    return "protocol-7 Hello admitted; intentional invalid config; not Ready"
                if child.poll() is not None:
                    break
                time.sleep(0.1)
            raise ValueError(f"{extension.name}: no Hello admission: {log.read_bytes()[-4096:]!r}")
        finally:
            # Always terminate the whole supervised process group.
            try:
                os.killpg(child.pid, signal.SIGINT)
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            except ProcessLookupError:
                pass


def probe(packages, version, arch, tagged):
    results = {}
    with tempfile.TemporaryDirectory(prefix="tau-probe-") as tmp:
        root = Path(tmp)
        paths = {}
        for component in distribution.components(version):
            name = component["name"]
            base = distribution.basename(component, version, arch, tagged)
            extract(packages / f"{base}.tar.gz", root / name)
            paths[name] = root / name / base / "bin" / name
            subprocess.run([str(paths[name]), "--help"], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15,
                           preexec_fn=limit_output)
        fullbase = distribution.basename({"name": "tau-full"}, version, arch, tagged)
        extract(packages / f"{fullbase}.tar.gz", root / "full")
        for name, binary in paths.items():
            full_binary = root / "full" / fullbase / "bin" / name
            if full_binary.read_bytes() != binary.read_bytes():
                raise ValueError(f"complete archive differs from individual archive: {name}")
        results["version_output"] = subprocess.check_output(
            [str(paths["tau"]), "--version"], text=True, timeout=15,
            preexec_fn=limit_output,
        )
        results["hello"] = {
            name: handshake(paths["tau"], binary, root / f"state-{name}")
            for name, binary in paths.items()
            if name not in ("tau", "tau-telegram-gateway")
        }
        results["help"] = sorted(paths)
    return results


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packages", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--tagged", action="store_true")
    print(json.dumps(probe(**vars(parser.parse_args())), indent=2))
