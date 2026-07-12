# DESIGN-tau-supervisor-test-fixture: Integration-test child fixture

Status: inferred

`tau-supervisor` verifies process lifecycle, stdio framing, environment
filtering, stderr policy, and direct-child cleanup with integration tests that
spawn a real local child process.

The fixture lives at `src/bin/tau-supervisor-test-child.rs` intentionally. Cargo
then builds it as a normal binary target and exposes its path to integration
tests through `CARGO_BIN_EXE_tau-supervisor-test-child`, avoiding test-only path
guessing and ensuring the fixture exercises the same binary-launch mechanics as
supervised children. The binary is an internal test fixture, not a production
entrypoint.

Keep fixture behavior narrow and deterministic. Add new fixture modes only for
contracts that require a real subprocess boundary and keep those modes coupled to
`tests/supervisor.rs`.
