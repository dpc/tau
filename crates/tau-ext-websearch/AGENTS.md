Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-websearch

Before changing provider behavior, endpoint configuration, response-size limits,
concurrency handling, or tests, read `README.md` for this crate's runtime and
security assumptions and the applicable `specs/DESIGN-*.md` records for design/testing expectations.

After major changes to this extension's features, tool/action behavior, configuration options, provider/runtime behavior, or user-visible capabilities, update the built-in self-knowledge skill `tau-self-knowledge-ext-websearch` so Tau can accurately explain the current extension behavior.
