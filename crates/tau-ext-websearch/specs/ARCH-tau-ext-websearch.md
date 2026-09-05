# ARCH-tau-ext-websearch: tau-ext-websearch architecture

`std-websearch` runs the `tau-ext-websearch` process and is enabled by default.
The extension adapts hosted MCP and REST web providers into Tau tools. Default
model-visible `web_search` and `web_fetch` tools rotate independently through
ordered provider pools and perform bounded sequential failover. Search defaults
to Exa, Parallel, and anonymous You.com; fetch defaults to Exa and Parallel.
Exa and Parallel may optionally authenticate without changing pool membership,
and You.com may switch from its anonymous free profile to its authenticated MCP
endpoint. Credentialed Brave search and Tavily/Firecrawl search/fetch adapters
may be added explicitly. Provider-specific Exa and Parallel tools remain
disabled by default for explicit role opt-in. An ordered list with one provider
gives the default tool explicit single-provider behavior.

Each operation's cursor belongs to the extension process, advances once for
each admitted composite call, and is not persisted. Argument validation, busy
rejection, and replay do not advance it. The serial protocol loop reserves
starts before workers launch, so concurrent completion cannot change order.

Every successful provider path projects text at the extension boundary as
one escaped `<tau_web_content>` string, then submit that string in an
invocation-correlated `tool.result_reported`. The harness publishes the canonical
terminal/provider projections, so existing transcript, replay, compaction,
Chat Completions, and Codex/Responses paths therefore retain their normal
semantics without a websearch-specific protocol or persistence representation.
Provider workers transfer their sole terminal outcome to the protocol loop over
an unbounded internal channel. The loop performs checked ordered publication and
retains the in-flight permit until that write succeeds; output failure exits the
extension so harness disconnect cleanup can settle the routed call. Optional
display progress remains detached. Complete ordered attempt history uses
generic `ToolUseState` metadata on progress and terminal events. Cancellation
terminals retain it through optional `ToolCancelled.display`.

Provider trust, endpoint, transport, and redaction behavior is
[SPEC-tau-ext-websearch-provider-boundary](SPEC-tau-ext-websearch-provider-boundary.md).
Concurrency and independent resource caps are
[SPEC-tau-ext-websearch-runtime-safeguards](SPEC-tau-ext-websearch-runtime-safeguards.md).
