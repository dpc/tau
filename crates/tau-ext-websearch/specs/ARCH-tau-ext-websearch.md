# ARCH-tau-ext-websearch: tau-ext-websearch architecture

`std-websearch` runs the `tau-ext-websearch` process and is enabled by default.
The extension adapts hosted MCP web providers into Tau tools. Exa search is the
default model-visible `web_search`; Parallel search and fetch are registered in
the same component but disabled by default for explicit role opt-in and to avoid
two default tools named `web_search`.

Provider trust, endpoint, transport, and redaction behavior is
[SPEC-tau-ext-websearch-provider-boundary](SPEC-tau-ext-websearch-provider-boundary.md).
Concurrency and independent resource caps are
[SPEC-tau-ext-websearch-runtime-safeguards](SPEC-tau-ext-websearch-runtime-safeguards.md).
