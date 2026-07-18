# DECISION-tau-supervisor-integration-test-fixture: Use a real Cargo-built child fixture

Authority: inferred

Real-process lifecycle contracts are tested through a narrow fixture compiled as a
normal Cargo binary target and exposed through
`CARGO_BIN_EXE_tau-supervisor-test-child`. This exercises the actual subprocess
launch boundary without test-only path guessing.

The fixture is an internal test component, not a production entrypoint. Its modes
remain narrow, deterministic, and coupled to `tests/supervisor.rs`.
