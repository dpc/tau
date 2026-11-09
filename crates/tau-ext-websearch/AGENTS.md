Before changing this crate, discover and read the applicable Linked Specs in `specs/` and every wider `specs/` scope, then follow relevant links. Use the `linked-specs` skill when updating them and `linked-specs-review` when reviewing.

# tau-ext-websearch

- Typed web results use `WebContent` and the shared `ToolResponse` serializer:
  [SPEC-context-output-formatting](../tau-harness/specs/SPEC-context-output-formatting.md#canonical-tool-text).
  Injected user-role context follows the separate
  [envelope contract](../tau-harness/specs/SPEC-context-output-formatting.md#registered-injected-context-envelopes).
  Do not wrap new typed web results in historical `tau_web_content` envelopes.

Before changing provider behavior, endpoint configuration, response-size limits,
concurrency handling, or tests, read `README.md` for this crate's runtime and
security assumptions, the applicable Linked Specs under `specs/`, and
[`testing.md`](testing.md).

After major changes to this extension's features, tool/action behavior, configuration options, provider/runtime behavior, or user-visible capabilities, update the built-in self-knowledge skill `tau-self-knowledge-ext-websearch` so Tau can accurately explain the current extension behavior.

- Before introducing a new free-form payload kind, or reframing an existing one, in the shared generic user-role `ContentPart::Text` carrier, read [`GATE-new-generic-user-payload-envelopes`](../../specs/GATE-new-generic-user-payload-envelopes.md) and [`SPEC-context-output-formatting`](../tau-harness/specs/SPEC-context-output-formatting.md#registered-injected-context-envelopes); use the shared registry rather than a component-local provenance wrapper. Typed tool results and the system/developer prompt channel are outside that rule.
