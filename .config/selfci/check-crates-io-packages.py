#!/usr/bin/env python3
"""Check the crates.io publication closure without requiring unpublished crates."""

import json
import re
import subprocess
import tempfile
from pathlib import Path


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
INCLUDE_LITERAL = re.compile(
    r"""include_(?:str|bytes)\s*!\s*\(\s*(?P<literal>
        "(?:\\.|[^"\\])*"
        |
        r(?P<hashes>\#{0,255})"(?P<raw>.*?)"(?P=hashes)
    )\s*,?\s*\)""",
    re.DOTALL | re.VERBOSE,
)
PACKAGED_SNAPSHOTS = {
    "crates/tau-cli/release-resources/cli.yaml": "config/cli.yaml",
    "crates/tau-cli/release-resources/harness.yaml": "config/harness.yaml",
    "crates/tau-harness/release-resources/built-in.cli.yaml": (
        "crates/tau-config/config/built-in.cli.yaml"
    ),
    "crates/tau-harness/release-resources/built-in.harness.yaml": (
        "crates/tau-config/config/built-in.harness.yaml"
    ),
    "crates/tau-harness/release-resources/tau-self-knowledge-config.md": (
        "crates/tau-skills/self-knowledge/tau-self-knowledge-config.md"
    ),
    "crates/tau-harness/release-resources/tau-self-knowledge-ext-pim.harness.yaml": (
        "crates/tau-skills/self-knowledge/"
        "tau-self-knowledge-ext-pim.harness.yaml"
    ),
    "crates/tau-harness/release-resources/tau-self-knowledge-ext-pim.md": (
        "crates/tau-skills/self-knowledge/tau-self-knowledge-ext-pim.md"
    ),
    "crates/tau-harness-tools/release-resources/delegate_prefix.md": (
        "crates/tau-harness/src/harness/prompts/delegate_prefix.md"
    ),
}


def check_packaged_snapshots(workspace: Path) -> None:
    """Require release-owned resource copies to match their canonical sources."""
    for packaged, canonical in PACKAGED_SNAPSHOTS.items():
        packaged_path = workspace / packaged
        canonical_path = workspace / canonical
        if packaged_path.read_bytes() != canonical_path.read_bytes():
            raise SystemExit(
                f"packaged snapshot {packaged} differs from canonical source {canonical}"
            )


def is_test_source(path: Path) -> bool:
    """Identify Rust sources omitted by Cargo's normal publish verification."""
    return "tests" in path.parts or path.name in {"tests.rs", "main_tests.rs"}


def rust_string_literal_value(match: re.Match[str]) -> str:
    """Decode the ordinary or raw Rust string literal captured from a macro."""
    literal = match["literal"]
    if literal.startswith("r"):
        hashes = match["hashes"]
        return literal[2 + len(hashes) : -(1 + len(hashes))]
    content = literal[1:-1]
    decoded = []
    position = 0
    simple_escapes = {
        "0": "\0",
        "t": "\t",
        "n": "\n",
        "r": "\r",
        '"': '"',
        "'": "'",
        "\\": "\\",
    }
    while position < len(content):
        character = content[position]
        if character != "\\":
            decoded.append(character)
            position += 1
            continue

        position += 1
        if position == len(content):
            raise SystemExit("unterminated escape in Rust include path literal")
        escaped = content[position]
        if escaped in simple_escapes:
            decoded.append(simple_escapes[escaped])
            position += 1
        elif escaped == "x":
            digits = content[position + 1 : position + 3]
            if len(digits) != 2 or not all(digit in "0123456789abcdefABCDEF" for digit in digits):
                raise SystemExit("invalid ASCII escape in Rust include path literal")
            value = int(digits, 16)
            if value > 0x7F:
                raise SystemExit("non-ASCII byte escape in Rust include path literal")
            decoded.append(chr(value))
            position += 3
        elif escaped == "u" and content[position + 1 : position + 2] == "{":
            closing = content.find("}", position + 2)
            if closing == -1:
                raise SystemExit("unterminated Unicode escape in Rust include path literal")
            digits = content[position + 2 : closing].replace("_", "")
            if not (1 <= len(digits) <= 6) or not all(
                digit in "0123456789abcdefABCDEF" for digit in digits
            ):
                raise SystemExit("invalid Unicode escape in Rust include path literal")
            value = int(digits, 16)
            if 0xD800 <= value <= 0xDFFF:
                raise SystemExit("invalid Unicode scalar in Rust include path literal")
            try:
                decoded.append(chr(value))
            except ValueError as error:
                raise SystemExit(
                    "invalid Unicode scalar in Rust include path literal"
                ) from error
            position = closing + 1
        elif escaped in {"\n", "\r"}:
            if escaped == "\r" and content[position + 1 : position + 2] == "\n":
                position += 1
            position += 1
            while position < len(content) and content[position].isspace():
                position += 1
        else:
            raise SystemExit(
                f"unsupported escape \\{escaped} in Rust include path literal"
            )
    return "".join(decoded)


def check_literal_includes(
    package_name: str, package_root: Path, packaged_files: set[str]
) -> None:
    """Require normal build-time literal includes to stay inside the archive."""
    root = package_root.resolve()
    for relative in sorted(packaged_files):
        source = package_root / relative
        if source.suffix != ".rs" or is_test_source(source.relative_to(package_root)):
            continue
        content = source.read_text(encoding="utf-8")
        for include in INCLUDE_LITERAL.finditer(content):
            include_path = rust_string_literal_value(include)
            target = (source.parent / include_path).resolve()
            try:
                packaged_target = target.relative_to(root).as_posix()
            except ValueError:
                packaged_target = None
            if packaged_target not in packaged_files:
                raise SystemExit(
                    f"{package_name} packages {relative}, but its literal include "
                    f"{include_path!r} is absent from the archive"
                )


def check_literal_include_regressions() -> None:
    """Exercise supported literal forms and the archive-boundary rejection."""
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        source = root / "src/lib.rs"
        resource = root / "resource.txt"
        source.parent.mkdir()
        resource.write_text("packaged\n", encoding="utf-8")
        source.write_text(
            'const A: &str = include_str ! ("..\\x2fresource.txt");\n'
            'const B: &[u8] = include_bytes!(r#\"../resource.txt\"#);\n',
            encoding="utf-8",
        )
        packaged_files = {"src/lib.rs", "resource.txt"}
        check_literal_includes("parser-regression", root, packaged_files)

        source.write_text(
            'const MISSING: &str = include_str ! (r\"../../outside.txt\", );\n',
            encoding="utf-8",
        )
        try:
            check_literal_includes("parser-regression", root, {"src/lib.rs"})
        except SystemExit:
            pass
        else:
            raise SystemExit("literal include regression failed to reject outside archive")


def main() -> None:
    """Validate metadata, dependency order, and package file selection."""
    workspace = Path.cwd().resolve()
    check_literal_include_regressions()
    check_packaged_snapshots(workspace)
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

        package_files = set(
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
                capture_output=True,
                text=True,
            ).stdout.splitlines()
        )
        package_root = Path(package["manifest_path"]).parent
        check_literal_includes(package_name, package_root, package_files)


if __name__ == "__main__":
    main()
