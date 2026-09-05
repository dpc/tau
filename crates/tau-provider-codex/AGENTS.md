Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-provider-codex

- Read the repository root `AGENTS.md` before making changes.
- Read `SECURITY.md` before changing authenticated quota acquisition or
  model-to-pool applicability.
- Read `SECURITY.md` before changing OAuth HTTP, parsing, errors, formatting, or
  logging. OAuth provider fields remain untrusted and must not be logged through
  their raw accessors.
- Read `specs/ARCH-tau-provider-codex.md`, the applicable Linked Specs under `specs/`, and the applicable trust-boundary records under `specs/` before changing
  transport behavior, cancellation, retry/error mapping, diagnostics, cache
  identity, or model metadata.
- Preserve the prompt-cache identity invariant documented in `specs/ARCH-tau-provider-codex.md`: first-party ChatGPT/Codex cache keys are stable per provider base URL, startup-selected Responses mode, and target agent id, independent of prompt originator/provenance.
Read `SECURITY.md` before changing VCR capture/replay or publishing public
provider fixtures.

- Before introducing a new free-form payload kind, or reframing an existing one, in the shared generic user-role `ContentPart::Text` carrier, read [`GATE-new-generic-user-payload-envelopes`](../../specs/GATE-new-generic-user-payload-envelopes.md) and [`SPEC-exact-sentinel-prompt-envelopes`](../../specs/SPEC-exact-sentinel-prompt-envelopes.md); use the shared registry rather than a component-local provenance wrapper. Typed tool results and the system/developer prompt channel are outside that rule.
