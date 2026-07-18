# DECISION-tau-cli-term-testing-strategy: Own behavior-level terminal tests locally

Authority: inferred

Focused unit tests in `tau-cli-term` own prompt parsing, completion behavior,
bounded subprocess execution, and terminal-action helpers. They exercise externally
meaningful module boundaries with real bounded child processes where subprocess
lifecycle matters; renderer behavior remains in lower-level terminal crates.

This keeps interactive prompt regressions local while retaining cross-platform
cleanup coverage without coupling tests to private implementation details.
