# DECISION-tau-cli-testing-boundaries: Test externally meaningful CLI boundaries

Authority: unconfirmed

CLI tests use focused deterministic checks at externally meaningful renderer,
routing, storage, process, and event boundaries. Renderer tests use representative
semantic themes rather than snapshots of built-in themes, and transition tests
observe the first flush-delimited frame without a post-operation redraw that could
hide a race.

This keeps regressions local and deterministic while still exercising the boundary
that users observe. Detailed coverage evolves in
[`testing.md`](../testing.md).
