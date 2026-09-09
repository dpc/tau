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

## Current CI model

- The blocking gates use LCOV from the Nix coverage derivation, not a local `cargo llvm-cov` run.
- `.#ci.crapAbsolute` fails current entries above the severe threshold with `--fail-above`.
- `.#ci.crap` is the aggregate/selfci compatibility output for the absolute gate.
- The absolute gate uses `.cargo-crap.toml`'s threshold of 400 with `--min 100`.
- Do not “fix” failures by raising the threshold. Refactor/decompose flagged code or add meaningful coverage.

## cargo-crap pitfalls

- The absolute gate intentionally has no baseline or exceptions: every measured
  production function must remain at or below the configured threshold.
- `--min` filters which current entries cargo-crap evaluates and reports; keep it
  low enough that every function capable of exceeding the absolute limit is
  included.
- cargo-crap v0.3.0 excludes root-level `tests/**`, `benches/**`, and `examples/**` by default. This is intentional for Tau's production-code CRAP gates; pass `--no-default-excludes` only for one-off investigation where test/bench/example code must be included.

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
