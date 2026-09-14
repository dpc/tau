#!/usr/bin/env python3
"""Check the crates.io publication closure without requiring unpublished crates."""

import json
import subprocess


PUBLICATION_ORDER = [
    "dpc-tau-actions",
    "dpc-tau-blocking-notify-channel",
    "dpc-tau-themes",
    "dpc-tau-util-fs-err",
    "dpc-tau-vcr",
    "dpc-tau-proto",
    "dpc-tau-client",
    "dpc-tau-config",
    "dpc-tau-core",
    "dpc-tau-delivery-memory",
    "dpc-tau-skills",
    "dpc-tau-socket",
    "dpc-tau-term-screen",
    "dpc-tau-ext-rhai",
    "dpc-tau-ext-std-notifications",
    "dpc-tau-ext-test-dummy",
    "dpc-tau-ext-utils",
    "dpc-tau-ext-websearch",
    "dpc-tau-provider",
    "dpc-tau-ext-shell",
    "dpc-tau-cli-picker",
    "dpc-tau-cli-term-raw",
    "dpc-tau-provider-chat-completions",
    "dpc-tau-provider-codex",
    "dpc-tau-provider-responses",
    "dpc-tau-session-inspect",
    "dpc-tau-cli-term",
    "dpc-tau-ext-provider-builtin",
    "dpc-tau-harness",
    "dpc-tau-harness-tools",
    "dpc-tau-test-support",
    "dpc-tau-cli",
    "dpc-tau",
]
EXCLUDED_PACKAGES = {
    "dpc-tau-e2e-tests",
    "dpc-tau-summary-eval",
    "dpc-tau-supervisor",
}


def main() -> None:
    """Validate metadata, dependency order, and package file selection."""
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    packages = {package["name"]: package for package in metadata["packages"]}
    expected = set(PUBLICATION_ORDER)
    actual = set(packages) - EXCLUDED_PACKAGES
    if actual != expected:
        raise SystemExit(
            f"publication package set changed: expected {sorted(expected)!r}, "
            f"found {sorted(actual)!r}"
        )

    positions = {name: position for position, name in enumerate(PUBLICATION_ORDER)}
    for package_name in PUBLICATION_ORDER:
        package = packages[package_name]
        for field in ("description", "homepage", "license", "repository", "rust_version"):
            if not package[field]:
                raise SystemExit(f"{package_name} is missing package metadata {field}")
        for dependency in package["dependencies"]:
            dependency_name = dependency["name"]
            if dependency_name not in packages:
                continue
            if dependency_name in EXCLUDED_PACKAGES:
                raise SystemExit(
                    f"{package_name} requires excluded package {dependency_name}"
                )
            required_version = f"={packages[dependency_name]['version']}"
            if dependency["req"] != required_version:
                raise SystemExit(
                    f"{package_name} requires {dependency_name} {dependency['req']!r}; "
                    f"expected {required_version!r}"
                )
            if dependency_name != package_name and (
                positions[dependency_name] >= positions[package_name]
            ):
                raise SystemExit(
                    f"{dependency_name} must precede dependent {package_name}"
                )

        subprocess.run(
            [
                "cargo",
                "package",
                "--allow-dirty",
                "--list",
                "--package",
                package_name,
            ],
            check=True,
            stdout=subprocess.DEVNULL,
        )


if __name__ == "__main__":
    main()
