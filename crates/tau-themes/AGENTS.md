Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

## Linked Specs

- the applicable Linked Specs under `specs/` — crate-local architecture,
  decisions, requirements, and specifications, including the testing strategy;
  read before changing this crate's behavior or tests.

## Test layout

- Keep module unit tests out-of-line in `src/<module>/tests.rs`, with the
  owning module declaring `#[cfg(test)] mod tests;`.
