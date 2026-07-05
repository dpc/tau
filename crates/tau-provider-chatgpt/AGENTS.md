# tau-provider-chatgpt

- Read the repository root `AGENTS.md` before making changes.
- Read `ARCHITECTURE.md`, `design.md`, and `SECURITY.md` before changing
  transport behavior, cancellation, retry/error mapping, diagnostics, cache
  identity, or model metadata.
- Preserve the prompt-cache identity invariant documented in `ARCHITECTURE.md`: first-party ChatGPT/Codex cache keys are stable per provider base URL and target agent id, independent of prompt originator/provenance.
