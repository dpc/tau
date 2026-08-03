---
name: tau-cargo-crap
description: >
  Use this skill when selfci, Nix CI, coverage, cargo-crap, CRAP-score,
  crapRegression, crapAbsolute, or crapReport checks fail in Tau, or before
  changing the cargo-crap gates, baseline, thresholds, or flagged complex code.
user-invocable: true
advertise: true
---

# Tau cargo-crap

Use this when `selfci check` fails in the `coverage/cargo-crap` step or when working down CRAP-score hotspots.

## Fast diagnosis

```bash
nix build -L .#ci.testsCcov
nix build -L .#ci.crapRegression
nix build -L .#ci.crapAbsolute
nix build -L .#ci.crapReport -o result-crap-report
sed -n '1,120p' result-crap-report/cargo-crap.md
```

`.#ci.crapReport` is the non-blocking inventory report. `.#ci.crapRegression` and `.#ci.crapAbsolute` are the blocking gates; `.#ci.crap` aggregates them for selfci compatibility.

## Current CI model

- The blocking gates use LCOV from the Nix coverage derivation, not a local `cargo llvm-cov` run.
- `.#ci.crapRegression` compares against `nix/cargo-crap-baseline.json` with `--fail-regression`.
- `.#ci.crapAbsolute` fails current entries above the severe threshold with `--fail-above`.
- `.#ci.crap` is the aggregate/selfci compatibility output that builds both gates.
- The gates are intentionally focused on severe entries with `--threshold 1000 --min 1000`.
- Do not “fix” failures by raising the threshold. Refactor/decompose flagged code or add meaningful coverage.

## Baseline regeneration

Only regenerate the baseline after an intentional CRAP-score change is accepted on the mainline:

```bash
nix build -L .#ci.crapBaseline -o result-crap-baseline
cp result-crap-baseline/cargo-crap-baseline.json nix/cargo-crap-baseline.json
jj file track nix/cargo-crap-baseline.json
```

Generate the baseline through Nix. The LCOV paths in this setup are `/build/source/...`; a local `cargo-crap --lcov result/lcov.info` run from `/home/...` will not match coverage paths and will produce bad coverage data.

## cargo-crap pitfalls

- `--fail-regression` fails only existing functions whose CRAP score increased; new high-CRAP functions are reported but do not fail by that flag alone, so `.#ci.crapAbsolute` also runs `--fail-above`.
- `--min` filters the current entries before baseline comparison. This is why the regression gate is a severe-regression gate, not a full-repo no-regression gate.
- cargo-crap v0.3.0 excludes root-level `tests/**`, `benches/**`, and `examples/**` by default. This is intentional for Tau's production-code CRAP gates; pass `--no-default-excludes` only for one-off investigation where test/bench/example code must be included.
- cargo-crap v0.3.0 baseline matching includes line numbers, so the old v0.2.0 duplicate same-file function-name false regressions are fixed. The `--min 1000` regression gate remains intentional to keep CI focused on severe regressions.

## Refactoring flagged code

For code fixes, preserve behavior first and split dispatch-heavy functions into named helpers with focused tests. A zero-coverage function needs roughly cyclomatic complexity `< 32` to get below a CRAP score of 1000, so extraction without tests may just move the hotspot. Prefer adding regression tests for behavior you touch.

## Validation

```bash
treefmt
nix build -L .#ci.crapRegression
nix build -L .#ci.crapAbsolute
nix build -L .#ci.crap
selfci check
```

`selfci check` skips the expensive LLVM coverage and cargo-crap lane by
default. Set `TAU_CI_FULL=true` when a maintainer needs that full local CI
lane, including its debt inventory and both blocking gates:

```bash
TAU_CI_FULL=true selfci check --candidate <change-id>
```

Do not add `crapBaseline` to routine CI: regenerate it only after an accepted,
intentional CRAP-score change lands on mainline.
