# Design decisions

This file records major design decisions currently embodied by this directory's code, and how authoritative each decision is. It is not an architecture overview, ADR log, todo list, roadmap, implementation guide, or changelog.

## Focused config regression tests

Status: inferred

`tau-config` uses focused crate-local unit tests as the primary regression
boundary for configuration behavior. Tests exercise the effective public loader
entry points and the normalization helpers behind them, especially for layered
precedence, alias canonicalization, alias/canonical conflicts, role merging,
extension-name validation, and atomic-write edge cases.

This is intentional for the current crate shape because most risks are subtle
schema and merge-contract regressions that can be reproduced with small
temporary config directories. When adding new config fields, legacy aliases, or
merge semantics, update the table-style alias coverage and add behavior-level
loader tests for both file layers and CLI overrides where the contract differs.
