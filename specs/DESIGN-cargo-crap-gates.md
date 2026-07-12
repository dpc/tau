# DESIGN-cargo-crap-gates: Cargo-crap gates are lowered by refactoring, not exceptions

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
