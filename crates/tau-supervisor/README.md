# tau-supervisor

`tau-supervisor` contains supervised child-process and stdio transport glue used to prototype and test supervision contracts independently from the harness.

This crate is not currently wired into the production harness extension supervisor path. Production extension spawning still lives in `tau-harness`; changes here should not be treated as production reliability coverage until the harness either integrates this crate or duplicates the same contracts with its own tests.

See [ARCH-tau-supervisor](specs/ARCH-tau-supervisor.md) for lifecycle, stdio, environment, cleanup, and trusted-child boundary contracts.
