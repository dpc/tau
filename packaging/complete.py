#!/usr/bin/env python3
"""Assemble individual packages and the complete bundle without executing them."""

import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

import distribution
import native


def source_metadata(source, project):
    """Validate upstream metadata before assigning release-set package revisions."""
    data = tomllib.loads((source / project["manifest"]).read_text())
    package = data["package"]
    workspace = tomllib.loads((source / "Cargo.toml").read_text()).get(
        "workspace", {}
    ).get("package", {})

    def value(key):
        result = package[key]
        return workspace[key] if result == {"workspace": True} else result

    if package["name"] != project["package"] or value("license") != project["license"]:
        raise ValueError(f"unexpected package identity/license: {project['name']}")
    version = value("version")
    if project["name"] != "tau" and version != project["version"]:
        raise ValueError(f"unreviewed upstream version: {project['name']}")
    return {
        "version": version, "license": value("license"),
        "sdk_lock_versions": {
            name: sorted({p["version"] for p in tomllib.loads(
                (source / "Cargo.lock").read_text()
            )["package"] if p["name"] == name})
            for name in ("dpc-tau-client", "dpc-tau-proto")
        },
        "source_file_sha256": {
            name: native.sha256((source / name).read_bytes())
            for name in sorted({"Cargo.toml", project["manifest"], "Cargo.lock", "LICENSE"})
        },
    }


def package_version(component, release, source_sha, tagged):
    # Keep upstream version separate from the native release-set revision.
    # Numeric fields make release revisions order correctly in both
    # dpkg and RPM, including 0.1.9 -> 0.1.10. Tilde sorts prereleases first.
    if not tagged:
        return f"0.0.0~test.{source_sha}", "1"
    import re
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[a-zA-Z0-9.]+)?", release):
        raise ValueError("release set must be SemVer without build metadata")
    return component["version"].replace("-", "~", 1), f"1.tau{release.replace('-', '~', 1)}"


def archive(payload, target, prefix, epoch):
    with tarfile.open(target, "w:gz") as output:
        for file in sorted(payload.rglob("*")):
            if not file.is_file() or file.is_symlink():
                continue
            info = output.gettarinfo(file, arcname=f"{prefix}/{file.relative_to(payload)}")
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = epoch
            with file.open("rb") as stream:
                output.addfile(info, stream)


def nfpm(config, basename, stage, assets, epoch):
    config_file = stage / "nfpm.json"
    native.write_json(config_file, config)
    for fmt in ("deb", "rpm"):
        subprocess.run([
            "nfpm", "package", "--config", str(config_file),
            "--packager", fmt, "--target", str(assets / f"{basename}.{fmt}"),
        ], check=True, env={**os.environ, "SOURCE_DATE_EPOCH": str(epoch)})


def assemble(manifest, sources, builds, staged_tau, arch, output, maintainer, tagged):
    """Inputs are read-only exact checkouts and build outputs in an isolated container."""
    if output.exists():
        raise ValueError("output must not already exist")
    native.check_nfpm_version(native.run("nfpm", "--version"))
    release = manifest["core"]["version"]
    epoch = manifest["source_date_epoch"]
    components = distribution.components(release)
    external = {p["input"]: p for p in manifest["external"]}
    projects = {p["name"]: p for p in distribution.projects()}
    metadata = {
        name: source_metadata(sources / name, project)
        for name, project in projects.items()
    }
    if metadata["tau"]["version"] != release:
        raise ValueError("core version does not match source manifest")
    with tempfile.TemporaryDirectory(prefix=".complete-", dir=output.parent) as tmp:
        stage = Path(tmp)
        assets = stage / "assets"
        assets.mkdir()
        full = stage / "full"
        full.mkdir()
        reports = []
        dependencies = {"deb": [], "rpm": []}
        for component in components:
            name, project = component["name"], component["project"]
            base = distribution.basename(component, release, arch, tagged)
            payload = stage / name
            payload.mkdir()
            binary = staged_tau if name == "tau" else builds / project / "target/release" / name
            notice = builds / project / "THIRD_PARTY_NOTICES.html"
            if not notice.is_file() or notice.stat().st_size < 100:
                raise ValueError(f"missing third-party license closure: {project}")
            for src, dst in [
                (binary, payload / f"bin/{name}"),
                (sources / project / "LICENSE", payload / f"share/licenses/{name}/LICENSE"),
                (notice, payload / f"share/doc/{name}/THIRD_PARTY_NOTICES.html"),
            ]:
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(src, dst)
                dst.chmod(0o755 if dst.parent.name == "bin" else 0o644)
            version, revision = package_version(component, release, manifest["source_sha"], tagged)
            report = {
                **component, **metadata[project],
                "tau_release_set": release, "package_version": version,
                "package_revision": revision,
                "source": external.get(project, {"source_sha": manifest["source_sha"]}),
                "elf": native.audit(payload / f"bin/{name}", arch),
                "third_party_notices_sha256": native.sha256(notice.read_bytes()),
            }
            native.write_json(payload / f"share/doc/{name}/source-manifest.json", report)
            reports.append(report)
            contents = [
                {"src": str(file), "dst": f"/usr/{file.relative_to(payload)}"}
                for file in sorted(payload.rglob("*")) if file.is_file()
            ]
            config = {
                "name": name, "arch": arch, "platform": "linux",
                "version": version, "version_schema": "none", "release": revision,
                "maintainer": maintainer, "description": f"{name} (Tau release set {release})",
                "license": component["license"], "contents": contents,
                "overrides": {
                    "deb": {"depends": ["libc6 (>= 2.34)", "libgcc-s1", "ca-certificates"]},
                    "rpm": {"depends": ["glibc >= 2.34", "libgcc", "ca-certificates"]},
                },
            }
            nfpm(config, base, stage, assets, epoch)
            archive(payload, assets / f"{base}.tar.gz", base, epoch)
            shutil.copytree(payload, full, dirs_exist_ok=True)
            dependencies["deb"].append(f"{name} (= {version}-{revision})")
            dependencies["rpm"].append(f"{name} = {version}-{revision}")
        bundle = {"name": "tau-full", "version": release}
        version, revision = package_version(bundle, release, manifest["source_sha"], tagged)
        base = distribution.basename(bundle, release, arch, tagged)
        nfpm({
            "name": "tau-full", "arch": arch, "platform": "linux",
            "version": version, "version_schema": "none", "release": revision,
            "maintainer": maintainer, "description": "Complete Tau native distribution",
            "license": manifest["core"]["license"],
            "contents": [],  # Dependency-only metapackage: no overlapping ownership.
            "overrides": {fmt: {"depends": deps} for fmt, deps in dependencies.items()},
        }, base, stage, assets, epoch)
        archive(full, assets / f"{base}.tar.gz", base, epoch)
        if {p.name for p in assets.iterdir()} != distribution.package_assets(release, arch, tagged):
            raise ValueError("incomplete native distribution")
        native.write_json(assets / "source-manifest.json", {
            **manifest, "components": reports,
            "distribution_sha256": native.sha256(distribution.INVENTORY.read_bytes()),
        })
        assets.rename(output)
