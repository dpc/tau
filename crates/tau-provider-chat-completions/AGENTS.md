# tau-provider-chat-completions

- Read the repository root `AGENTS.md` before making changes.
- Read `ARCHITECTURE.md` before changing request construction, streaming
  parsing, replay behavior, tool-call handling, or provider cache/prompt identity
  semantics.
- Read `SECURITY.md` before changing streamed provider response handling,
  diagnostics, progress metadata, or any path that might expose provider/tool
  content outside the transcript/tool-call boundary.
