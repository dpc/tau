# DECISION-tau-themes-testing-strategy: Prefer small semantic theme tests

Authority: inferred

This leaf crate uses small behavior-level unit examples for parsing, registry,
fallback, and span-resolution semantics rather than snapshots of entire built-in
themes. Built-in visual choices can then evolve without unrelated expectation churn
while API and resolution regressions remain protected.
