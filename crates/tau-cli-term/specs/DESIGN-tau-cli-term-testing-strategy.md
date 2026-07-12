# DESIGN-tau-cli-term-testing-strategy: Testing strategy

Status: inferred

`tau-cli-term` relies primarily on focused Rust unit tests for prompt parsing,
completion behavior, bounded subprocess execution, and terminal-action helpers.
Tests should prefer externally meaningful behavior at module boundaries over
private implementation details: command timeout/output-limit tests should run
real short-lived child processes with bounded durations, completion tests should
assert candidate/replacement behavior, and renderer-like behavior should stay
in lower-level terminal crates.

Subprocess lifecycle tests should cover success, timeout, stdout overflow,
inherited-pipe handling, stdin/stdout interaction, and process-group cleanup
when those contracts change, because these paths protect the interactive prompt
from wedging or leaking external commands.
