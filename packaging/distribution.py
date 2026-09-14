#!/usr/bin/env python3
"""The required native asset set, shared by builders and release publication."""

from pathlib import Path
import re
import tomllib


INVENTORY = Path(__file__).with_name("distribution.toml")


def projects():
    inventory = tomllib.loads(INVENTORY.read_text())
    if inventory["schema"] != 1:
        raise ValueError("unsupported distribution inventory")
    result = inventory["projects"]
    names = [p["name"] for p in result]
    binaries = [b for p in result for b in p["binaries"]]
    if (not result or names[0] != "tau" or len(set(names)) != len(names)
            or len(set(binaries)) != len(binaries)):
        raise ValueError("invalid or duplicate distribution entries")
    for project in result:
        for name in [project["name"], project["package"], *project["binaries"]]:
            if not re.fullmatch(r"[a-z][a-z0-9-]*", name):
                raise ValueError("unsafe distribution name")
        if project["name"] != "tau" and not re.fullmatch(
                r"\d+\.\d+\.\d+", project["version"]):
            raise ValueError("external version must be stable SemVer")
    return result


def components(version):
    return [
        {"name": binary, "project": project["name"],
         "version": version if project["name"] == "tau" else project["version"],
         "license": project["license"]}
        for project in projects() for binary in project["binaries"]
    ]


def basename(component, release_version, arch, tagged):
    if not tagged:
        return f"{component['name']}-test-{arch}"
    if component["name"] in ("tau", "tau-full"):
        return f"{component['name']}-{release_version}-{arch}"
    return f"{component['name']}-{component['version']}-tau-{release_version}-{arch}"


def package_assets(version, arch, tagged):
    products = [*components(version), {"name": "tau-full", "version": version}]
    return {
        f"{basename(component, version, arch, tagged)}.{fmt}"
        for component in products for fmt in ("deb", "rpm", "tar.gz")
    }
