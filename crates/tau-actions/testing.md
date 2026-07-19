# Testing tau-actions

`tau-actions` unit tests own schema validation budgets and invariants, command
tokenization, usage rendering, every argument-kind parser branch, and exact
bounded parse diagnostics. Consumer crates own behavior outside that reusable
boundary: `tau-cli` tests dynamic completion and schema-generation replacement,
while `tau-core` and `tau-harness` test owner-bound routing, replacement, and
disconnect cleanup.
