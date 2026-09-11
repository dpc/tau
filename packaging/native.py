#!/usr/bin/env python3
"""Local, non-release Linux packaging tools. See README.md for trust limits."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib


EXTERNAL = {
    "tau-ext-pim": ["tau-ext-pim"],
    "tau-ext-rostra": ["tau-ext-rostra"],
    "tau-ext-slack": ["tau-ext-slack"],
    "tau-ext-swarm": ["tau-ext-swarm"],
    "tau-ext-telegram": ["tau-ext-telegram", "tau-telegram-gateway"],
    "tau-ext-xmpp": ["tau-ext-xmpp"],
    "tau-ext-zulip": ["tau-ext-zulip"],
}
ARCHES = {
    "amd64": ("Advanced Micro Devices X86-64", "/lib64/ld-linux-x86-64.so.2"),
    "arm64": ("AArch64", "/lib/ld-linux-aarch64.so.1"),
}
NFPM_VERSION = "2.46.3"


def run(*args):
    return subprocess.check_output(args, text=True, env={**os.environ, "LC_ALL": "C"})


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def require_sha(value):
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise ValueError("source_sha must be a full lowercase 40-character Git SHA")
    return value


def source_file(repo, source_sha, name):
    return subprocess.check_output(
        ["git", "--no-replace-objects", "-C", str(repo), "show",
         f"{require_sha(source_sha)}:{name}"]
    )


def inventory(repo, source_sha):
    """Read only immutable Git objects; never execute selected source."""
    require_sha(source_sha)
    resolved = run("git", "--no-replace-objects", "-C", str(repo),
                   "rev-parse", f"{source_sha}^{{commit}}").strip()
    if resolved != source_sha:
        raise ValueError("source_sha must identify a commit, not a tag object")
    lock_bytes = source_file(repo, source_sha, "flake.lock")
    lock = json.loads(lock_bytes)
    inputs = lock["nodes"][lock["root"]]["inputs"]
    packages = []
    for name, binaries in EXTERNAL.items():
        node = inputs[name]
        if not isinstance(node, str):
            raise ValueError(f"{name}: expected a direct locked input")
        locked = lock["nodes"][node]["locked"]
        require_sha(locked["rev"])
        if locked["type"] != "git" or not locked["url"].startswith("https://"):
            raise ValueError(f"{name}: expected an HTTPS Git source")
        if not locked.get("narHash", "").startswith("sha256-"):
            raise ValueError(f"{name}: missing locked NAR hash")
        packages.append(
            {
                "input": name,
                "binaries": binaries,
                "url": locked["url"],
                "source_sha": locked["rev"],
                "nar_hash": locked["narHash"],
                "qualification": "pending-owner-audit",
            }
        )
    cargo_bytes = source_file(repo, source_sha, "Cargo.toml")
    cargo = tomllib.loads(cargo_bytes.decode())["workspace"]["package"]
    return {
        "schema": 1,
        "purpose": "non-release-candidate",
        "source_sha": source_sha,
        "source_date_epoch": int(
            run("git", "--no-replace-objects", "-C", str(repo),
                "show", "-s", "--format=%ct", source_sha)
        ),
        "core": {
            "binary": "tau",
            "version": cargo["version"],
            "license": cargo["license"],
            "rust_version": cargo["rust-version"],
        },
        "source_file_sha256": {
            name: sha256(source_file(repo, source_sha, name))
            for name in ("Cargo.toml", "Cargo.lock", "flake.lock", "LICENSE")
        },
        "external": packages,
    }


def inspect_elf(header, program, dynamic, versions, arch):
    """Conservative candidate ABI filter, not a runtime compatibility proof."""
    machine, interpreter = ARCHES[arch]
    if not re.search(rf"Machine:\s+{re.escape(machine)}\s*$", header, re.MULTILINE):
        raise ValueError("ELF machine does not match requested architecture")
    if not re.search(r"Class:\s+ELF64\s*$", header, re.MULTILINE):
        raise ValueError("expected ELF64")
    if "2's complement, little endian" not in header:
        raise ValueError("expected little-endian ELF")
    if "/nix/store" in program + dynamic:
        raise ValueError("Nix-store runtime reference")
    interpreters = re.findall(r"Requesting program interpreter: ([^\]]+)", program)
    if interpreters != [interpreter]:
        raise ValueError(f"expected GNU interpreter {interpreter}")
    if re.search(r"\((?:RPATH|RUNPATH)\)", dynamic):
        raise ValueError("RPATH/RUNPATH not allowed in baseline candidates")
    needed = re.findall(r"\(NEEDED\).*Shared library: \[([^\]]+)\]", dynamic)
    allowed = {
        "libc.so.6", "libm.so.6", "libgcc_s.so.1",
        "libpthread.so.0", "libdl.so.2", Path(interpreter).name,
    }
    if not needed or not set(needed) <= allowed or "libc.so.6" not in needed:
        raise ValueError(f"unreviewed dynamic dependencies: {needed}")
    if "GLIBC_PRIVATE" in versions or "GLIBC_ABI_" in versions:
        raise ValueError("unreviewed glibc ABI requirement")
    required = {
        tuple(map(int, v.split(".")))
        for v in re.findall(r"\bGLIBC_([0-9]+(?:\.[0-9]+)+)\b", versions)
    }
    if not required or max(required) > (2, 34):
        raise ValueError("required glibc symbol version exceeds 2.34 or is missing")
    return {
        "arch": arch,
        "interpreter": interpreter,
        "needed": sorted(set(needed)),
        "max_glibc_symbol": ".".join(map(str, max(required))),
        "runtime_qualification": "not-performed",
    }


def audit(binary, arch):
    # Do not use ldd: this command inspects, and never executes, candidate code.
    result = inspect_elf(
        run("readelf", "-hW", str(binary)),
        run("readelf", "-lW", str(binary)),
        run("readelf", "-dW", str(binary)),
        run("readelf", "-VW", str(binary)),
        arch,
    )
    data = binary.read_bytes()
    if b"__TAU_BUILD_GIT_REVISION_PLACEHOLDER" in data or b"__TAU_BUILD_DATE" in data:
        raise ValueError("unresolved Tau build identity placeholder")
    result["binary_sha256"] = sha256(data)
    return result


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def check_nfpm_version(output):
    versions = re.findall(r"^GitVersion:[ \t]+(\S+)[ \t]*$", output, re.MULTILINE)
    if versions != [NFPM_VERSION]:
        raise ValueError(f"requires stable nFPM {NFPM_VERSION}")


def package(repo, source_sha, binary, arch, output, maintainer):
    """Wrap only the core, as a visibly unqualified test candidate."""
    manifest = inventory(repo, source_sha)
    if output.exists():
        raise ValueError("output must not exist (never mix or overwrite candidates)")
    check_nfpm_version(run("nfpm", "--version"))
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".tau-package-", dir=output.parent) as tmp:
        stage = Path(tmp)
        payload = stage / "payload"
        payload.mkdir()
        staged_binary = payload / "tau"
        shutil.copyfile(binary, staged_binary)
        staged_binary.chmod(0o755)
        manifest["elf"] = audit(staged_binary, arch)
        manifest["nfpm_version"] = NFPM_VERSION
        manifest["limitations"] = [
            "Binary/source relationship is supplied by caller, not attested",
            "No supervised startup or distro install/uninstall qualification",
            "External packages are inventoried, not built",
            "Not a release or a portability guarantee",
        ]
        # A fixed zero-version prerelease cannot outrank any stable SemVer.
        version = f"0.0.0~test.{source_sha}"
        manifest["package_version"] = version
        license_file = payload / "LICENSE"
        license_file.write_bytes(source_file(repo, source_sha, "LICENSE"))
        manifest_file = payload / "source-manifest.json"
        write_json(manifest_file, manifest)
        contents = [
            {"src": str(staged_binary), "dst": "/usr/bin/tau"},
            {"src": str(license_file), "dst": "/usr/share/licenses/tau/LICENSE"},
            {
                "src": str(manifest_file),
                "dst": "/usr/share/doc/tau/source-manifest.json",
            },
        ]
        config = {
            "name": "tau",
            "arch": arch,
            "platform": "linux",
            "version": version,
            "version_schema": "none",
            "release": "1",
            "maintainer": maintainer,
            "description": "Tau universal executable (unqualified test candidate)",
            "license": manifest["core"]["license"],
            "contents": contents,
            "overrides": {
                "deb": {"depends": ["libc6 (>= 2.34)", "libgcc-s1", "ca-certificates"]},
                "rpm": {"depends": ["glibc >= 2.34", "libgcc", "ca-certificates"]},
            },
        }
        config_file = stage / "nfpm.json"
        write_json(config_file, config)
        assets = stage / "assets"
        assets.mkdir()
        for fmt in ("deb", "rpm"):
            subprocess.run(
                [
                    "nfpm", "package", "--config", str(config_file),
                    "--packager", fmt, "--target", str(assets / f"tau-test-{arch}.{fmt}"),
                ],
                check=True,
                env={**os.environ, "SOURCE_DATE_EPOCH": str(manifest["source_date_epoch"])},
            )
        # Tar metadata is normalized; gzip byte reproducibility is not claimed.
        with tarfile.open(assets / f"tau-test-{arch}.tar.gz", "w:gz") as archive:
            for file in sorted(payload.iterdir()):
                info = archive.gettarinfo(file, arcname=f"tau-test-{arch}/{file.name}")
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mtime = manifest["source_date_epoch"]
                with file.open("rb") as stream:
                    archive.addfile(info, stream)
        shutil.copyfile(manifest_file, assets / "source-manifest.json")
        (assets / "SHA256SUMS").write_text(
            "".join(f"{sha256(p.read_bytes())}  {p.name}\n" for p in sorted(assets.iterdir()))
        )
        assets.rename(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    source = commands.add_parser("inventory")
    source.add_argument("--repo", type=Path, default=Path("."))
    source.add_argument("--source-sha", required=True, type=require_sha)
    elf = commands.add_parser("audit-elf")
    elf.add_argument("--binary", type=Path, required=True)
    elf.add_argument("--arch", choices=ARCHES, required=True)
    pack = commands.add_parser("package-core")
    pack.add_argument("--repo", type=Path, default=Path("."))
    pack.add_argument("--source-sha", required=True, type=require_sha)
    pack.add_argument("--binary", type=Path, required=True)
    pack.add_argument("--arch", choices=ARCHES, required=True)
    pack.add_argument("--output", type=Path, required=True)
    pack.add_argument("--maintainer", required=True)
    args = vars(parser.parse_args())
    command = args.pop("command")
    try:
        if command == "inventory":
            print(json.dumps(inventory(**args), indent=2, sort_keys=True))
        elif command == "audit-elf":
            print(json.dumps(audit(**args), indent=2, sort_keys=True))
        else:
            # nFPM must receive absolute payload paths even with relative CLI paths.
            args["output"] = args["output"].absolute()
            package(**args)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"native packaging: {error}\n")


if __name__ == "__main__":
    main()
