---
name: tau-cargo-crap
description: >
  Use this skill when selfci, Nix CI, coverage, cargo-crap, CRAP-score,
  crapAbsolute, or crapReport checks fail in Tau, or before changing the
  cargo-crap gates, thresholds, or flagged complex code.
user-invocable: true
advertise: true
---

# Tau cargo-crap

Use this when `selfci check` fails in the `coverage/cargo-crap` step or when working down CRAP-score hotspots.

## Fast diagnosis

```bash
nix build -L .#ci.testsCcov
nix build -L .#ci.crapAbsolute
nix build -L .#ci.crapReport -o result-crap-report
sed -n '1,120p' result-crap-report/cargo-crap.md
```

`.#ci.crapReport` is the non-blocking inventory report. `.#ci.crapAbsolute` is
the blocking gate; `.#ci.crap` preserves the aggregate entry point used by
selfci.

## Duplicate inventory

```bash
nix build -L .#ci.crapDuplicates -o result-crap-duplicates
sed -n '1,100p' result-crap-duplicates/duplicates.txt
```

SelfCI builds this source-only report even without `TAU_CI_FULL=true`. Its
`duplicates.txt` and `duplicates.json` contain candidate pairs, not a
duplicate-count gate. The Nix build fails if analysis fails, but a nonzero pair
count does not fail CI. Report paths are relative to the repository root.
For a local run without Nix output, use the pinned
`cargo crap --workspace --duplicates --top 0 --format human` with the same
`--exclude` patterns as `flake.nix`; `--top 0` hides meaningless CRAP scores
when no LCOV is supplied, without filtering duplicate pairs. The human
report's `No functions found.` refers to that suppressed CRAP table; read the
separate `duplicate candidates` section below it.

The inventory skips root-level Cargo test/bench/example directories and
`src/**/tests/`, `src/**/tests.rs`, and `src/**/*_tests.rs` files. It keeps
production files, including their ordinary helpers; `#[cfg(test)]` modules
are skipped by cargo-crap itself. Treat matches as leads for inspecting both
functions and their callers, not refactoring instructions: renamed enum
accessors, trait implementations, and macro-heavy bodies often share structure
without sharing behavior. To investigate one area, rerun
`cargo crap -p <crate> --duplicates --top 0` with the same exclusions. Do not enable
`[duplicates]` globally: the Markdown debt report and GitHub gate cannot
display pairs. Do not enable TypeSafe triage; it would send function bodies
outside the project.

## Current CI model

- The blocking gates use LCOV from the Nix coverage derivation, not a local `cargo llvm-cov` run.
- `.#ci.crapAbsolute` fails current entries above the severe threshold with `--fail-above`.
- `.#ci.crap` is the aggregate/selfci compatibility output for the absolute gate.
- The absolute gate uses `.cargo-crap.toml`'s threshold of 400 with `--min 100`.
- The Markdown debt report shows uncovered instrumented line ranges; they
  suggest where to add tests but do not measure assertion quality.
- Do not “fix” failures by raising the threshold. Refactor/decompose flagged code or add meaningful coverage.

## cargo-crap pitfalls

- The absolute gate intentionally has no baseline or exceptions: every measured
  production function must remain at or below the configured threshold.
- `--min` filters which current entries cargo-crap evaluates and reports; keep it
  low enough that every function capable of exceeding the absolute limit is
  included.
- Tau configures `tests/**`, `benches/**`, and `examples/**` as default
  exclusions for production-code CRAP gates. Pass `--no-default-excludes`
  only for one-off investigations that need those directories.

## Refactoring flagged code

For code fixes, preserve behavior first and extract coherent semantic operations with focused tests, not arbitrary match fragments. A zero-coverage function needs cyclomatic complexity below 20 to stay under the absolute score of 400, so extraction without tests may just move the hotspot. Prefer adding regression tests for behavior you touch.

## Validation

```bash
treefmt
nix build -L .#ci.crapAbsolute
nix build -L .#ci.crap
selfci check
```

`selfci check` skips the expensive LLVM coverage and cargo-crap lane by
default. Set `TAU_CI_FULL=true` when a maintainer needs that full local CI
lane, including its debt inventory and blocking absolute gate:

```bash
TAU_CI_FULL=true selfci check --candidate <change-id>
```

Do not add a baseline for functions under the limit. A baseline is only
appropriate as an explicitly approved, temporary whitelist of pre-existing
above-limit violations, and such exceptions may only shrink.
