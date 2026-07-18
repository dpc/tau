# DECISION-tau-config-regression-testing: Guard config contracts with local loader tests

Authority: inferred

Crate-local tests of public loaders and normalization helpers, using temporary
config roots, are the primary regression boundary for schema and merge behavior.
They cover file layers and CLI overrides where contracts differ rather than relying
on broader integration tests to expose subtle precedence or canonicalization bugs.

Detailed alias and edge-case matrices remain adjacent to the executable tests.
This decision refines [`ARCH-tau-config`](ARCH-tau-config.md).
