# DESIGN-tau-themes-testing-strategy: Theme behavior is protected by focused unit tests

Status: inferred

`tau-themes` is a leaf styling crate, so its verification is centered on crate
unit tests for semantic behavior: color parsing, JSON5 theme parsing, rejection
of unknown fields, built-in theme registry consistency, default-style fallback,
and nested span inheritance/override rules.

Tests should prefer small behavioral examples over snapshots of entire themes,
because built-in visual choices are expected to evolve without changing the
crate's API or resolution semantics.
