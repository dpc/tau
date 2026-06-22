# tau-ext-test-dummy

After major changes to this extension's features, tool/action behavior, configuration options, provider/runtime behavior, or user-visible capabilities, update the built-in self-knowledge skill `tau-self-knowledge-ext-test-dummy` so Tau can accurately explain the current extension behavior.

Local design notes:

- `ARCHITECTURE.md` describes the fixture boundaries and behavior invariants.
- `SECURITY.md` describes why this disabled-by-default test extension has only a trusted local stdio boundary.
- `TESTING.md` describes the regression coverage expected for restart modes, replay suppression, and prompt interception.
