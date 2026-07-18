Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-provider-chat-completions

- Read `SECURITY.md` before changing HTTP transport, proxy/TLS behavior,
  cancellation, response bounds, or provider diagnostics.

- Read the repository root `AGENTS.md` before making changes.
- Read `specs/ARCH-tau-provider-chat-completions.md` before changing request construction, streaming
  parsing, replay behavior, tool-call handling, or provider cache/prompt identity
  semantics.
- Read the applicable trust-boundary records under `specs/` before changing streamed provider response handling,
  diagnostics, provider response stats, or any path that might expose
  provider/tool content outside the transcript/tool-call boundary.
