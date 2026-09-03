#!/usr/bin/env python3
"""Report Rust structs that exceed the repository field-count limit."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys

MAXIMUM_FIELDS = 30
SAFE_PATH = re.compile(r"^[A-Za-z0-9_./+-]+$")


def run(
    command: list[str], *, cwd: Path, input_text: str | None = None
) -> subprocess.CompletedProcess[bytes]:
    """Run a subprocess without interpreting path bytes through a shell."""
    return subprocess.run(
        command,
        cwd=cwd,
        input=None if input_text is None else input_text.encode(),
        capture_output=True,
        check=False,
    )


def repository_root() -> Path:
    """Return the current Git repository root or terminate with a diagnostic."""
    result = run(["git", "rev-parse", "--show-toplevel"], cwd=Path.cwd())
    if result.returncode != 0:
        sys.stderr.buffer.write(result.stderr)
        raise SystemExit("Failed to locate the repository for the Rust struct field limit.")
    return Path(os.fsdecode(result.stdout.rstrip(b"\n")))


def tracked_rust_paths(root: Path) -> list[str]:
    """Return every tracked Rust path, preserving unusual path characters."""
    result = run(["git", "ls-files", "-z", "--", "*.rs"], cwd=root)
    if result.returncode != 0:
        sys.stderr.buffer.write(result.stderr)
        raise SystemExit(
            "Failed to enumerate tracked Rust files for the struct field limit."
        )
    return [os.fsdecode(path) for path in result.stdout.split(b"\0") if path]


def path_batches(paths: list[str]) -> list[list[str]]:
    """Split explicit paths below a conservative command-line size."""
    batches: list[list[str]] = []
    batch: list[str] = []
    batch_bytes = 0
    for path in paths:
        path_bytes = len(os.fsencode(path)) + 1
        if batch and batch_bytes + path_bytes > 128 * 1024:
            batches.append(batch)
            batch = []
            batch_bytes = 0
        batch.append(path)
        batch_bytes += path_bytes
    if batch:
        batches.append(batch)
    return batches


def ast_grep_matches(root: Path, paths: list[str]) -> list[dict]:
    """Run the structural rule and decode its JSON-stream findings."""
    rule = Path(__file__).with_name("rules") / "limit-rust-struct-fields.yml"
    matches: list[dict] = []
    for batch in path_batches(paths):
        result = run(
            [
                "ast-grep",
                "scan",
                "--json=stream",
                "--rule",
                os.fspath(rule),
                "--",
                *batch,
            ],
            cwd=root,
        )
        if result.returncode not in (0, 1):
            sys.stderr.buffer.write(result.stderr)
            raise SystemExit("The structural Rust field scan failed.")
        try:
            matches.extend(
                json.loads(line) for line in result.stdout.splitlines() if line
            )
        except json.JSONDecodeError as error:
            raise SystemExit(
                f"The structural Rust field scan returned invalid JSON: {error}"
            ) from error
    return matches


def field_list(match: dict) -> str:
    """Extract the matched struct's complete named or tuple field list."""
    secondary = match["metaVariables"]["multi"].get("secondary", [])
    candidates = [
        item["text"]
        for item in secondary
        if item["text"].startswith(("{", "(")) and item["text"].endswith(("}", ")"))
    ]
    if not candidates:
        raise ValueError("ast-grep did not return the matched struct field list")
    return max(candidates, key=len)


def count_fields(root: Path, field_list_text: str) -> int:
    """Count fields from ast-grep's direct structured field-list captures."""
    tuple_fields = field_list_text.startswith("(")
    pattern = (
        "struct Diagnostic($$$FIELDS);"
        if tuple_fields
        else "struct Diagnostic { $$$FIELDS }"
    )
    source = (
        f"struct Diagnostic{field_list_text};"
        if tuple_fields
        else f"struct Diagnostic {field_list_text}"
    )
    result = run(
        [
            "ast-grep",
            "run",
            "--stdin",
            "--lang",
            "rust",
            "--pattern",
            pattern,
            "--json=stream",
        ],
        cwd=root,
        input_text=source,
    )
    if result.returncode != 0:
        sys.stderr.buffer.write(result.stderr)
        raise ValueError("ast-grep could not parse the matched struct field list")
    captures = json.loads(result.stdout)["metaVariables"]["multi"]["FIELDS"]
    significant = [
        capture["text"]
        for capture in captures
        if not capture["text"].lstrip().startswith(("//", "/*"))
    ]
    if not significant:
        return 0
    commas = significant.count(",")
    return commas if significant[-1] == "," else commas + 1


def display_path(path: str) -> str:
    """Render a path on one diagnostic line without losing its exact value."""
    if SAFE_PATH.fullmatch(path):
        return path
    return json.dumps(path, ensure_ascii=False)


def main() -> int:
    """Scan tracked Rust structs and emit exact actionable diagnostics."""
    root = repository_root()
    matches = ast_grep_matches(root, tracked_rust_paths(root))
    matches.sort(key=lambda match: (match["file"], match["range"]["byteOffset"]["start"]))
    for match in matches:
        try:
            name = match["metaVariables"]["single"]["STRUCT_NAME"]["text"]
            actual_fields = count_fields(root, field_list(match))
        except (KeyError, TypeError, ValueError) as error:
            print(
                f"Failed to interpret the structural Rust field finding: {error}",
                file=sys.stderr,
            )
            return 2
        path = display_path(match["file"])
        line = match["range"]["start"]["line"] + 1
        print(
            f"{path}:{line}: struct {name} has {actual_fields} fields "
            f"(maximum {MAXIMUM_FIELDS}). Split it into smaller, logically coherent "
            "sub-state structs grouped by ownership, lifecycle, or invariants rather "
            "than suppressing this check.",
            file=sys.stderr,
        )
    return int(bool(matches))


if __name__ == "__main__":
    raise SystemExit(main())
