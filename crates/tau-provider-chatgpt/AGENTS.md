Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-provider-chatgpt

- Read the repository root `AGENTS.md` before making changes.
- Read `SECURITY.md` before changing authenticated quota acquisition or
  model-to-pool applicability.
- Read `specs/ARCH-tau-provider-chatgpt.md`, the applicable `specs/DESIGN-*.md` records, and the applicable trust-boundary records under `specs/` before changing
  transport behavior, cancellation, retry/error mapping, diagnostics, cache
  identity, or model metadata.
- Preserve the prompt-cache identity invariant documented in `specs/ARCH-tau-provider-chatgpt.md`: first-party ChatGPT/Codex cache keys are stable per provider base URL, startup-selected Responses mode, and target agent id, independent of prompt originator/provenance.
Read `SECURITY.md` before changing VCR capture/replay or publishing public
provider fixtures.
