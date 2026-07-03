# Design decisions

This file records major design decisions currently embodied by this directory's code, and how authoritative each decision is. It is not an architecture overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## AGENTS.md and skill discovery follow symlinks

Status: confirmed, 2026-07-03, user

Tau intentionally follows symlinks while discovering and loading trusted prompt
inputs: AGENTS.md files, AGENTS.*.md files, skill roots, skill directories, and
Markdown skill files. This supports normal dotfile, shared-repository, and
project-local layouts where instruction files or skill collections are linked
from another location.

This is an accepted prompt trust-boundary decision, not a sandbox escape. These
files are already trusted instructions that can steer the agent once loaded, so
refusing symlinks would mainly break legitimate layouts without creating a
meaningful security boundary. Implementations must still bound traversal and
reads, track canonical directories for skill traversal so symlink cycles cannot
recurse forever, and document that users should only run Tau in repositories and
skill roots whose prompt content they trust.

## Cargo-crap gates are lowered by refactoring, not exceptions

Status: confirmed, 2026-06-23, user

Cargo-crap limits are project quality gates. Increasing configured limits,
whether in `.cargo-crap.toml` or in the Nix cargo-crap gate definitions, is not
an acceptable way to make failures pass.

Regenerating or editing `nix/cargo-crap-baseline.json` to add exceptions to the
cargo-crap limits is also not acceptable. The baseline is an opt-out from
refactoring that exists only to keep already-known historical debt from blocking
unrelated work, and it should shrink over time. New or worsening cargo-crap
failures should be addressed by simplifying/decomposing code and/or adding
meaningful tests.

The active Nix absolute gate remains at CRAP 500 while existing historical
offenders above CRAP 100 are being worked down. Once those offenders are below
CRAP 100, the intended next threshold is CRAP 100; that lowering should happen
by refactoring the offenders, not by adding new exceptions.

Tau keeps shared cargo-crap defaults in the repository-root `.cargo-crap.toml`
so local developer runs and Nix CI use the same non-CI-specific policy values.
Nix cargo-crap derivations should pass only run-specific inputs and per-job
overrides that differ from the shared defaults, such as workspace selection,
LCOV path, baseline path, min/top cutoffs, output/format, fail mode, and
intentionally different report/regression thresholds. The filtered Nix source
must include `.cargo-crap.toml` so config changes invalidate cargo-crap CI
outputs. Do not put CI-only LCOV/baseline paths or allowlist exceptions in the
shared config.
