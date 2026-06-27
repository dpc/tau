## Design docs

- `design.md` — crate-local design decisions, including the testing strategy;
  read before changing this crate's behavior or tests.

## Test layout

- Keep module unit tests out-of-line in `src/<module>/tests.rs`, with the
  owning module declaring `#[cfg(test)] mod tests;`.
