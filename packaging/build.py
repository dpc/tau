#!/usr/bin/env python3
"""Build core candidates or trusted tagged releases on a native Linux Docker host."""

import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import re
import resource
import shutil
import subprocess
import tempfile
import uuid

import native


TOOLS = Path(__file__).resolve().parent
TOOL_FILES = (
    "packaging/build.py", "packaging/inside.py", "packaging/native.py",
    "packaging/build-inputs.json", "packaging/Dockerfile", "packaging/.dockerignore",
    ".github/workflows/native-candidates.yml", ".github/workflows/release.yml",
)


def verify_tooling(workflow_sha):
    for name in TOOL_FILES:
        if native.source_file(TOOLS.parent, workflow_sha, name) != (TOOLS.parent / name).read_bytes():
            raise ValueError(f"tooling differs from workflow_sha: {name}")


def inputs(arch):
    raw = (TOOLS / "build-inputs.json").read_bytes()
    pins = json.loads(raw)
    selected = pins["architectures"][arch]
    if pins["schema"] != 1 or pins["nfpm_version"] != native.NFPM_VERSION:
        raise ValueError("unsupported build inputs or nFPM version mismatch")
    if not re.fullmatch(r"quay\.io/pypa/manylinux_2_28_[a-z0-9_]+@sha256:[0-9a-f]{64}",
                        selected["image"]):
        raise ValueError("baseline image must be digest-pinned")
    for field in ("rust_sha256", "nfpm_sha256"):
        if not re.fullmatch(r"[0-9a-f]{64}", selected[field]):
            raise ValueError(f"invalid {field}")
    return pins, selected, native.sha256(raw)


def snapshot(repo, source_sha, destination):
    """Fresh Git metadata: do not copy caller credentials, hooks or replacements."""
    native.require_sha(source_sha)
    env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
               GIT_NO_REPLACE_OBJECTS="1")
    git = ["git", "--no-replace-objects", "-c", "init.templateDir=",
           "-c", "core.hooksPath=/dev/null", "-C", str(destination)]
    destination.mkdir()
    subprocess.run([*git, "init", "-q"], check=True, env=env)
    subprocess.run([*git, "fetch", "--quiet", "--no-tags", "--no-write-fetch-head",
                    "--depth=1", str(repo.absolute()), source_sha], check=True, env=env)
    subprocess.run([*git, "checkout", "--quiet", "--detach", source_sha],
                   check=True, env=env)
    if native.run(*git, "rev-parse", "HEAD").strip() != source_sha:
        raise ValueError("snapshot does not identify selected source")


def image_command(pins, selected, iidfile):
    args = {
        "BASE_IMAGE": selected["image"],
        "RUST_VERSION": pins["rust_version"],
        "RUST_TARGET": selected["rust_target"],
        "RUST_SHA256": selected["rust_sha256"],
        "NFPM_VERSION": pins["nfpm_version"],
        "NFPM_ARCH": selected["nfpm_arch"],
        "NFPM_SHA256": selected["nfpm_sha256"],
    }
    command = ["docker", "build", "--pull", "--iidfile", str(iidfile)]
    for key, value in args.items():
        command.extend(["--build-arg", f"{key}={value}"])
    return [*command, "--file", str(TOOLS / "Dockerfile"), str(TOOLS)]


def docker_container_user():
    try:
        security_options = json.loads(
            native.run("docker", "info", "--format", "{{json .SecurityOptions}}")
        )
    except json.JSONDecodeError as error:
        raise ValueError("Docker returned invalid security options") from error
    if (not isinstance(security_options, list)
            or any(not isinstance(option, str) for option in security_options)):
        raise ValueError("Docker returned invalid security options")
    # A rootless daemon maps container root to its unprivileged host owner.
    # Using the host numeric UID inside that user namespace instead maps to a
    # subordinate host UID, which cannot write caller-owned bind mounts.
    if "name=rootless" in security_options:
        return "0:0"
    return f"{os.getuid()}:{os.getgid()}"


def container_command(image, name, mounts, network=True, user=None):
    if user is None:
        user = f"{os.getuid()}:{os.getgid()}"
    command = [
        "docker", "run", "--rm", "--init", "--name", name,
        "--read-only", "--user", user,
        "--cap-drop=ALL", "--security-opt=no-new-privileges",
        "--pids-limit=512", "--cpus=2", "--memory=12g",
        "--tmpfs", "/tmp:rw,nosuid,nodev,size=1073741824",
    ]
    if not network:
        command.append("--network=none")
    for source, destination, readonly in mounts:
        if "," in str(source):
            raise ValueError("Docker bind source paths cannot contain commas")
        mount = f"type=bind,src={source},dst={destination}"
        command.extend(["--mount", mount + (",readonly" if readonly else "")])
    return [*command, image]


def execute(command, log, timeout, log_limit=64 * 1024 * 1024):
    # Bound even hostile compiler/probe output. The limit applies to this
    # Docker client, not just processes in the container.
    def limit_output():
        resource.setrlimit(resource.RLIMIT_FSIZE, (log_limit, log_limit))

    with log.open("wb") as stream:
        subprocess.run(command, check=True, stdout=stream, stderr=subprocess.STDOUT,
                       timeout=timeout, preexec_fn=limit_output)


def container(image, mounts, command, log, timeout, network=True,
              log_limit=64 * 1024 * 1024, user=None):
    name = f"tau-native-{uuid.uuid4().hex}"
    try:
        execute([*container_command(image, name, mounts, network, user), *command],
                log, timeout, log_limit)
    finally:
        # Killing a timed-out Docker client does not necessarily stop its container.
        subprocess.run(["docker", "rm", "--force", name], check=False,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=30)


def verify_version(text, version, source_sha, epoch):
    date = datetime.fromtimestamp(epoch, timezone.utc).strftime("%Y-%m-%d %H:%M")
    expected = rf"tau {re.escape(version)} \({source_sha[:7]}, {re.escape(date)}\)\n?"
    if not re.fullmatch(expected, text):
        raise ValueError("packaged tau --version does not match clean selected source")


def build(repo, source_sha, workflow_sha, arch, output, maintainer, run_id, run_attempt,
          release_tag=None):
    native.require_sha(source_sha)
    native.require_sha(workflow_sha)
    verify_tooling(workflow_sha)
    pins, selected, pins_sha = inputs(arch)
    if platform.system() != "Linux" or platform.machine() != selected["machine"]:
        raise ValueError("a native matching Linux host is required; no emulation fallback")
    if os.getuid() == 0:
        raise ValueError("run as a non-root host user with Docker access")
    if output.exists():
        raise ValueError("output must not already exist")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".tau-native-", dir=output.parent) as tmp:
        root = Path(tmp)
        source = root / "source"
        snapshot(repo, source_sha, source)
        manifest = native.inventory(source, source_sha)
        if release_tag is not None:
            native.release_version(release_tag, manifest["core"]["version"])
        logs = root / "logs"
        logs.mkdir()
        try:
            server_arch = native.run("docker", "info", "--format", "{{.Architecture}}").strip()
            if server_arch not in (arch, selected["machine"]):
                raise ValueError("Docker server must match the native source architecture")
            container_user = docker_container_user()
            # The image ID comes from this invocation, never a shared mutable tag.
            iidfile = root / "builder.iid"
            execute(image_command(pins, selected, iidfile), logs / "image.log", 1800)
            image_id = iidfile.read_text().strip()
            if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
                raise ValueError("Docker did not return an immutable derived image ID")
            if shutil.disk_usage(root).free < 8 * 1024**3:
                raise ValueError("need at least 8 GiB free after preparing builder (not a capacity guarantee)")
            work = root / "build"
            work.mkdir()
            container(image_id, [(source, "/source", True), (work, "/work", False)],
                       ["env", f"SOURCE_DATE_EPOCH={manifest['source_date_epoch']}",
                        "/opt/rust/bin/cargo", "build", "--locked", "--release", "-p", "dpc-tau"],
                       logs / "cargo.log", 5400, user=container_user)
            assembly = root / "assembly"
            assembly.mkdir()
            package_work = root / "package-work"
            package_work.mkdir()
            container(image_id, [
                (source, "/source", True), (work, "/build", True),
                (TOOLS, "/tooling", True), (package_work, "/work", False),
                (assembly, "/output", False),
            ], ["python3", "/tooling/inside.py", "--source-sha", source_sha,
                  "--arch", arch, "--maintainer", maintainer]
                 + (["--release-tag", release_tag] if release_tag else []),
                       logs / "package.log", 300, network=False, user=container_user)
            probe_work = root / "probe-work"
            probe_work.mkdir()
            container(image_id, [(assembly, "/probe", True), (probe_work, "/work", False)],
                       ["/probe/tau", "--version"], logs / "version.log", 30,
                       network=False, log_limit=8192, user=container_user)
            version = (logs / "version.log").read_text()
            verify_version(version, manifest["core"]["version"], source_sha,
                           manifest["source_date_epoch"])
            assets = assembly / "packages"
            basename = (
                f"tau-{manifest['core']['version']}-{arch}"
                if release_tag else f"tau-test-{arch}"
            )
            expected = {"SHA256SUMS", "source-manifest.json",
                        f"{basename}.deb", f"{basename}.rpm", f"{basename}.tar.gz"}
            if {p.name for p in assets.iterdir()} != expected:
                raise ValueError("unexpected package inventory")
            if any(p.is_symlink() or not p.is_file() for p in assets.iterdir()):
                raise ValueError("package assets must be regular files")
            native.write_json(assets / "build-manifest.json", {
                "schema": 1,
                "purpose": (
                    "tagged-github-release" if release_tag
                    else "manual-non-release-core-candidate"
                ),
                "source_sha": source_sha, "workflow_sha": workflow_sha,
                "release_tag": release_tag,
                "github_prerelease": (
                    native.release_is_prerelease(manifest["core"]["version"])
                    if release_tag else None
                ),
                "run_id": run_id, "run_attempt": run_attempt,
                "source_date_epoch": manifest["source_date_epoch"],
                "build_inputs_sha256": pins_sha, "build_inputs": pins,
                "arch": arch, "derived_image_id": image_id,
                "command": ["cargo", "build", "--locked", "--release", "-p", "dpc-tau"],
                "profile": "release (selected source Cargo.toml)",
                "features": "selected source defaults", "version_output": version,
                "runtime_qualification": "not-performed",
                "restrictions": [
                    "No distro install/uninstall or default restricted supervised startup test",
                    "No external packages; manifest inventory is not qualification",
                    (
                        "GitHub tag and workflow identity are release authority, not signed provenance"
                        if release_tag else
                        "Workflow/source labels are not signed provenance or release authority"
                    ),
                ],
            })
            shutil.copyfile(assembly / "toolchain.json", assets / "toolchain.json")
            shutil.copyfile(logs / "cargo.log", assets / "cargo.log")
            # Regenerate checksums after adding trusted orchestration metadata.
            (assets / "SHA256SUMS").write_text("".join(
                f"{native.sha256(p.read_bytes())}  {p.name}\n"
                for p in sorted(assets.iterdir()) if p.name != "SHA256SUMS"
            ))
            assets.rename(output)
        except (OSError, ValueError, subprocess.SubprocessError):
            # Escape arbitrary source output: never emit active terminal/Actions commands.
            for log in sorted(logs.glob("*.log")):
                with log.open("rb") as stream:
                    stream.seek(max(0, log.stat().st_size - 2048))
                    print(f"{log.name} tail: {stream.read()!r}")
            raise


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--source-sha", type=native.require_sha, required=True)
    parser.add_argument("--workflow-sha", type=native.require_sha, required=True)
    parser.add_argument("--arch", choices=native.ARCHES, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--maintainer", required=True)
    parser.add_argument("--run-id", default="local")
    parser.add_argument("--run-attempt", default="1")
    parser.add_argument("--release-tag")
    args = vars(parser.parse_args())
    args["output"] = args["output"].absolute()
    try:
        build(**args)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f"native build: {error}\n")
