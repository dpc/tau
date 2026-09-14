#!/usr/bin/env python3
"""Install/ownership/remove qualification inside a disposable offline userspace."""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile

import distribution
import complete


def run(*args):
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT)
    except subprocess.CalledProcessError as error:
        raise ValueError(f"{args[0]} failed: {error.output[-8192:]!r}") from error


def expected_paths(name):
    if name == "tau-full":
        return set()
    return {
        f"/usr/bin/{name}", f"/usr/share/licenses/{name}/LICENSE",
        f"/usr/share/doc/{name}/source-manifest.json",
        f"/usr/share/doc/{name}/THIRD_PARTY_NOTICES.html",
    }


def qualify(packages, version, arch, tagged, fmt):
    products = [*distribution.components(version), {"name": "tau-full", "version": version}]
    files = [
        str(packages / f"{distribution.basename(p, version, arch, tagged)}.{fmt}")
        for p in products
    ]
    expected_versions = {
        p["name"]: "-".join(complete.package_version(p, version, "0" * 40, tagged))
        for p in products
    }
    # Manual candidate identity comes from the generated source manifest.
    if not tagged:
        source_sha = json.loads((packages / "source-manifest.json").read_text())["source_sha"]
        expected_versions = {
            p["name"]: "-".join(complete.package_version(p, version, source_sha, False))
            for p in products
        }
    full = files[-1]
    if fmt == "deb":
        actual_dependencies = {v.strip() for v in run("dpkg-deb", "-f", full, "Depends").split(",")}
        expected_dependencies = {f"{p['name']} (= {expected_versions[p['name']]})" for p in products[:-1]}
    else:
        actual_dependencies = {
            line for line in run("rpm", "-qp", "--requires", full).splitlines()
            if not line.startswith("rpmlib(")
        }
        expected_dependencies = {f"{p['name']} = {expected_versions[p['name']]}" for p in products[:-1]}
    if actual_dependencies != expected_dependencies:
        raise ValueError("metapackage does not require the exact complete release set")
    for product, package in zip(products, files):
        expected = expected_paths(product["name"])
        if fmt == "deb":
            with tempfile.TemporaryDirectory() as tmp:
                subprocess.run(["dpkg-deb", "--control", package, tmp], check=True)
                if {p.name for p in Path(tmp).iterdir()} & {
                    "preinst", "postinst", "prerm", "postrm", "triggers", "conffiles",
                }:
                    raise ValueError("unexpected package lifecycle/configuration")
        else:
            if run("rpm", "-qp", "--scripts", package).strip():
                raise ValueError("unexpected package script")
            if set(run("rpm", "-qp", "--qf", "[%{FILENAMES}\n]", package).splitlines()) != expected:
                raise ValueError("unexpected RPM payload")
    sentinel = Path("/root/.local/state/tau/release-preservation-probe")
    sentinel.parent.mkdir(parents=True, exist_ok=True)
    sentinel.write_text("preserve user state\n")
    if fmt == "deb":
        install = run("apt-get", "-y", "install", *files)
    else:
        install = run("dnf", "-y", "--disablerepo=*", "install", *files)
    installed = {}
    for product in products:
        name = product["name"]
        expected = expected_paths(name)
        if fmt == "deb":
            actual = {p for p in run("dpkg-query", "-L", name).splitlines() if Path(p).is_file()}
            installed[name] = run("dpkg-query", "-W", "-f=${Version}", name)
        else:
            actual = set(run("rpm", "-q", "--qf", "[%{FILENAMES}\n]", name).splitlines())
            installed[name] = run("rpm", "-q", "--qf", "%{VERSION}-%{RELEASE}", name)
        if actual != expected:
            raise ValueError(f"{name}: installed ownership mismatch: {actual ^ expected}")
        if installed[name] != expected_versions[name]:
            raise ValueError(f"{name}: wrong installed release-set version")
        for file in expected:
            if not Path(file).is_file():
                raise ValueError(f"missing installed payload: {file}")
        if name != "tau-full":
            subprocess.run([f"/usr/bin/{name}", "--help"], check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
    names = [p["name"] for p in products]
    if fmt == "deb":
        baseline = run("dpkg-query", "-W")
        removed = run("apt-get", "-y", "remove", *names)
        run("dpkg", "--compare-versions", "0.1.0-1.tau0.1.9", "lt", "0.1.0-1.tau0.1.10")
    else:
        baseline = run("rpm", "-qa")
        removed = run("dnf", "-y", "--disablerepo=*", "remove", *names)
        order = run("rpm", "--eval", '%{lua:print(rpm.vercmp("0.1.0-1.tau0.1.9", "0.1.0-1.tau0.1.10"))}')
        if order.strip() != "-1":
            raise ValueError("RPM release-set revision ordering failed")
    if sentinel.read_text() != "preserve user state\n":
        raise ValueError("uninstall changed user state")
    for name in names:
        if any(Path(path).exists() for path in expected_paths(name)):
            raise ValueError(f"uninstall left owned payload: {name}")
    return {
        "format": fmt, "installed": installed, "baseline_packages": baseline,
        "install_output": install, "remove_output": removed,
        "checks": ["exact-metapackage-dependencies", "dependency-resolution", "payload-ownership", "no-lifecycle-scripts",
                   "installed-help", "uninstall", "user-state-preserved", "revision-ordering"],
        "limitations": ["Not a service/Ready or default restricted-supervision qualification"],
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packages", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--arch", required=True)
    parser.add_argument("--fmt", choices=("deb", "rpm"), required=True)
    parser.add_argument("--tagged", action="store_true")
    print(json.dumps(qualify(**vars(parser.parse_args())), indent=2))
