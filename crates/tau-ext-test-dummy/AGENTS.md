Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-test-dummy

After major changes to this extension's features, tool/action behavior, configuration options, provider/runtime behavior, or user-visible capabilities, update the built-in self-knowledge skill `tau-self-knowledge-ext-test-dummy` so Tau can accurately explain the current extension behavior.

Local design notes:

- `specs/ARCH-tau-ext-test-dummy.md` describes the fixture boundaries and
  behavior invariants, including the trusted local stdio control boundary and
  the release mode's narrowly scoped caller-provisioned fixture-private Unix
  socket.
- `TESTING.md` describes the regression coverage expected for restart modes, replay suppression, and prompt interception.
- `SECURITY.md` is required reading and records the extension's capability,
  trust, and worker-lifecycle boundary.
