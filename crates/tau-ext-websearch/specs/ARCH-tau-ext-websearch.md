# ARCH-tau-ext-websearch: tau-ext-websearch architecture

`std-websearch` runs the `tau-ext-websearch` process and is enabled by default.
The extension adapts hosted MCP web providers into Tau tools. Exa search is the
default model-visible `web_search`; Parallel search and fetch are registered in
the same component but disabled by default for explicit role opt-in and to avoid
two default tools named `web_search`.

All three successful paths project provider text at the extension boundary as
one escaped `<tau_web_content>` string, then submit that string in an
invocation-correlated `tool.result_reported`. The harness publishes the canonical
terminal/provider projections, so existing transcript, replay, compaction,
Chat Completions, and Codex/Responses paths therefore retain their normal
semantics without a websearch-specific protocol or persistence representation.

Provider trust, endpoint, transport, and redaction behavior is
[SPEC-tau-ext-websearch-provider-boundary](SPEC-tau-ext-websearch-provider-boundary.md).
Concurrency and independent resource caps are
[SPEC-tau-ext-websearch-runtime-safeguards](SPEC-tau-ext-websearch-runtime-safeguards.md).
