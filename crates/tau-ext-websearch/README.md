# tau-ext-websearch

A Tau extension that registers generic web search and fetch tools. Default
search rotates through Exa, Parallel, and anonymous You.com. Default fetch
rotates through Exa and Parallel. Brave, Tavily, and Firecrawl are optional
credentialed adapters.

The harness-level `agents.web_tools` policy chooses this ordinary search only
when the exact model route does not win with provider-hosted search.
`web_fetch` remains ordinary because OpenAI exposes no caller-directed hosted
fetch primitive. Native failure never switches implementation mid-turn; an
ambiguous provider retry may repeat billable hosted search.

When a role supplies `allowed_domains`, the harness freezes that restriction as
hidden invocation policy rather than exposing it as model arguments. External
fetch rejects a non-HTTP(S), userinfo, IP-literal, or out-of-allowlist target
before provider selection or extractor contact. This gates only the requested
target URL; an extractor may still follow redirects or load subresources, so it
is not a network sandbox.

The authoritative logical policy schema, inheritance/null semantics, candidate
selection rules, and disable/override examples are documented in
[`docs/providers.md`](../../docs/providers.md#chatgptcodex-provider). A
nonempty allowlist makes the default Exa/Parallel/You search pool unavailable.
Configured Tavily and Firecrawl searches are eligible because Tau passes their
documented provider-side domain filters. Tau never presents result
post-filtering as an egress control.


## Tools

- `websearch_hybrid_search` and `websearch_hybrid_fetch` are advertised as
  `web_search` and `web_fetch` and are enabled by default.
- `websearch_exa` / `websearch_exa_fetch` and
  `websearch_parallel_search` / `websearch_parallel_fetch` retain explicit
  provider paths. They are disabled by default.
- Exa uses its anonymously accessible hosted MCP at
  <https://mcp.exa.ai/mcp>. Parallel uses its anonymously accessible Search MCP
  at <https://search.parallel.ai/mcp>. Both providers supply search and fetch.
- You.com search uses the anonymous
  <https://api.you.com/mcp?profile=free> profile. Its documented limit is 100
  searches per day; this profile does not support fetch. Each attempt performs
  the required MCP initialization and carries any returned session id through
  the initialized notification and search call.
- Brave supports search. Tavily and Firecrawl support search and fetch. These
  adapters use named Tau secrets and are never enabled implicitly.

Earlier provider research found opportunistic keyless Tavily and Firecrawl
routes. Tau deliberately does not guess those semantics: the current standard
Tavily REST and Firecrawl v2 REST contracts require bearer credentials, so
these adapters remain credentialed-only.

Hybrid search retains Exa's `query` and optional `num_results` input. Parallel
receives the query and uses its own fixed result budget. Hybrid fetch accepts one
`url`; both adapters convert it to the provider's `urls` array. Explicit
Parallel tools retain their provider-specific pass-through arguments.

No Parallel API key is supported yet: there is no `api_key` config, and the
extension does not send an Authorization header.


## Configuration

The built-in extension is `std-websearch` and is enabled by default. Disable it
to prevent its outbound HTTP calls:

```json5
{
  extensions: {
    "std-websearch": { enable: false },
  },
}
```

Configure anonymous endpoints and ordered provider membership:

```json5
{
  extensions: {
    "std-websearch": {
      config: {
        // Backwards-compatible alias for exa_endpoint.
        endpoint: "https://mcp.exa.ai/mcp?exaApiKey=sk-…",
        exa_endpoint: "https://mcp.exa.ai/mcp",
        parallel_endpoint: "https://search.parallel.ai/mcp",
        you_endpoint: "https://api.you.com/mcp?profile=free",

        // Defaults shown. One entry selects explicit single-provider mode.
        search_providers: ["exa", "parallel", "you"],
        fetch_providers: ["exa", "parallel"],
      },
    },
  },
}
```

Add credentialed adapters by declaring Tau secrets and referring to their
names. API-key bytes do not belong in `config`:

```yaml
extensions:
  std-websearch:
    secrets:
      brave_search: {}
      tavily: {}
      firecrawl: {}
    config:
      search_providers: [exa, parallel, you, brave, tavily, firecrawl]
      fetch_providers: [exa, parallel, tavily, firecrawl]
      brave_api_key_secret: brave_search
      tavily_api_key_secret: tavily
      firecrawl_api_key_secret: firecrawl
      # Optional final/base endpoint overrides:
      brave_endpoint: https://api.search.brave.com/res/v1/web/search
      tavily_endpoint: https://api.tavily.com/
      firecrawl_endpoint: https://api.firecrawl.dev/v2/
```

Brave cannot appear in `fetch_providers`; anonymous You.com cannot either.
Selecting Brave, Tavily, or Firecrawl without its named, non-empty Tau secret
rejects configuration. Tau does not watch configuration or secret files:
restart Tau (or explicitly restart the extension through its supervisor) after
changing them.

Provider lists must be non-empty and contain no duplicates. Search and fetch
have independent extension-process cursors. Successful configuration resets
both cursors to list index zero; cursors are not persisted.

If both `endpoint` and `exa_endpoint` are set, they must have the same value.
Endpoint values are validated when configuration is applied. They must be HTTPS
URLs, except that HTTP loopback URLs are accepted for deterministic tests.
Endpoint userinfo credentials are rejected. Logs do not print raw endpoints
because query strings or fragments can contain credentials. Provider requests
do not follow redirects.


## Scheduling and failures

An accepted hybrid call advances its operation's cursor once, then tries at most
three configured providers in circular order. Attempts are sequential and stop
at the first non-empty success. Provider failures, malformed or oversize
responses, and trimmed-empty text fail over. Local validation, busy rejection,
and replay do not advance a cursor or contact a provider.

All attempts share one 45-second deadline. Each attempt receives the remaining
time divided by the remaining attempts, so unused time carries forward. At most
eight tool calls run concurrently; additional calls fail immediately as busy.
There is no cross-call health cache or circuit breaker.

Failover can submit the same query or URL to multiple services. Every issued
attempt may consume provider quota or incur charges, including failed and empty
attempts. Tau neither hedges, retries one provider, nor refunds or deduplicates
provider accounting. Cancellation prevents later attempts, but an already
issued blocking request may run until its allocated deadline and consume quota.

All-provider errors contain only stable provider/category pairs and are capped
at 1 KiB. Raw provider errors remain subject to existing bounded endpoint
redaction before local diagnostics.


## Result and UI boundary

Every successful result is an ordinary tool-result string enclosed in
`<tau_web_content adapter="exa|parallel|you|brave|tavily|firecrawl" operation="search|fetch"
content_trust="external">…</tau_web_content>`. Tau returns only the first
successful provider's text; it does not merge or rank provider outputs.
Provider content and metadata remain untrusted external claims.

The tool header shows the submitted query or only the fetched URL's parsed host.
It also carries one ordered provider-history chip:

```text
… Exa
✗ Exa → … Parallel
✗ Exa → ✓ Parallel
∅ Exa → ✓ Parallel
⏱ Exa → ✗ Parallel
✗ Exa → ⊘ Parallel
```

Stable markers are `…` in progress, `✓` success, `✗` failure, `∅` empty, `⏱`
deadline, and `⊘` cancellation. Single-provider mode still shows its provider.
The chip contains no raw provider error, endpoint, requested URL, or credential.

Provider output is bounded before decode and projection. Tau makes structural
Unicode visible, replaces exact closing-sentinel collisions, and rejects a
projected result over 512 KiB rather than silently truncating it. This boundary
prevents sentinel breakout but does not sandbox prompt injection or grant
content any identity, routing, instruction, authorization, or egress authority.

Tests use local stubs or loopback HTTP servers and never contact live providers.


## Tracing

```sh
TAU_LOG=websearch=debug tau …
```
