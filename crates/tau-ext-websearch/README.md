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

## Maintaining self-knowledge

Keep the built-in [websearch self-knowledge skill](../tau-skills/self-knowledge/tau-self-knowledge-ext-websearch.md)
current when changing adapter inventory, default pools, authentication/config
fields, failover, or restart behavior. That skill owns the user-facing provider,
account, free-tier, and paid-plan matrix; do not duplicate it in architecture
records. Recheck its dated external sources when provider policies change.

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
- Exa uses its hosted MCP at <https://mcp.exa.ai/mcp>. Parallel uses its Search
  MCP at <https://search.parallel.ai/mcp>. Both are anonymously accessible,
  support optional API-key authentication for higher limits, and supply search
  and fetch. Each Parallel attempt performs MCP initialization, returns any
  server-issued session id on the initialized notification and tool call, and
  sends the negotiated protocol header after initialization.
- You.com search defaults to the anonymous
  <https://api.you.com/mcp?profile=free> profile. Its documented limit is 100
  searches per day; this profile does not support fetch. Configuring a You.com
  key selects <https://api.you.com/mcp> by default and authenticates every MCP
  handshake request. Each attempt performs the required MCP initialization and
  carries any returned session id through the initialized notification and
  search call.
- Brave supports search. Tavily and Firecrawl support search and fetch. These
  adapters use named Tau secrets and are never enabled implicitly.

Earlier provider research found opportunistic keyless Tavily and Firecrawl
routes. Tau deliberately does not guess those semantics: the current standard
Tavily REST and Firecrawl v2 REST contracts require bearer credentials, so
these adapters remain credentialed-only.

Hybrid search retains Exa's `query` and optional `num_results` input. Tau maps
that query to Parallel's required `objective` and one-element `search_queries`;
Parallel uses its own fixed result budget. Hybrid fetch accepts one `url`; both
adapters convert it to the provider's `urls` array. Explicit Parallel tools keep
the same Tau-facing inputs and translate them to the current upstream shape.

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

Configure endpoints and ordered provider membership:

```json5
{
  extensions: {
    "std-websearch": {
      config: {
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

`endpoint` remains a backwards-compatible alias for `exa_endpoint`; if both are
set, they must contain the same value.

Add optional authentication and credentialed adapters by declaring Tau secrets
and referring to their names. API-key bytes do not belong in `config`:

```yaml
extensions:
  std-websearch:
    secrets:
      brave_search: {}
      exa: {}
      parallel: {}
      you: {}
      tavily: {}
      firecrawl: {}
    config:
      search_providers: [exa, parallel, you, brave, tavily, firecrawl]
      fetch_providers: [exa, parallel, tavily, firecrawl]
      exa_api_key_secret: exa
      parallel_api_key_secret: parallel
      you_api_key_secret: you
      brave_api_key_secret: brave_search
      tavily_api_key_secret: tavily
      firecrawl_api_key_secret: firecrawl
      # Optional provider-neutral preferences:
      fetch_pdf_parsing: disabled # disabled, fast, auto, or ocr
      fetch_pdf_max_pages: null   # 1..10000; incompatible with disabled
      search_recency: week        # day, week, month, or year
      search_exclude_domains: [ads.example]
      search_country: US          # normalized uppercase alpha-2
      search_language: en         # normalized lowercase BCP-47-like tag
      search_depth: balanced      # fast, balanced, or deep
      search_max_content_chars: null
      fetch_max_content_chars: null
      search_cache_max_age_seconds: null
      fetch_cache_max_age_seconds: null
      # Optional final/base endpoint overrides:
      brave_endpoint: https://api.search.brave.com/res/v1/web/search
      tavily_endpoint: https://api.tavily.com/
      firecrawl_endpoint: https://api.firecrawl.dev/v2/
```

Exa, Parallel, and You.com remain usable without named secrets. When
`you_api_key_secret` is set and `you_endpoint` is omitted, Tau selects the
authenticated `https://api.you.com/mcp` endpoint instead of the anonymous free
profile. Brave cannot appear in `fetch_providers`; You.com cannot either.
Selecting Brave, Tavily, or Firecrawl without its named, non-empty Tau secret
rejects configuration. Tau does not watch configuration or secret files:
restart Tau (or explicitly restart the extension through its supervisor) after
changing them.

Provider lists must be non-empty and contain no duplicates. Search and fetch
have independent extension-process cursors. Successful configuration resets
both cursors to list index zero; cursors are not persisted.

Every preference is optional. Omission preserves the provider's existing
request shape and default cost. A provider applies only controls that its
current API exposes:

| Preference | Exa | Parallel | You.com | Brave | Tavily | Firecrawl |
|---|---|---|---|---|---|---|
| PDF parsing/page cap | — | — | — | — | — | fetch |
| Recency | — | — | search | search | search | search |
| Excluded domains | — | authenticated search | search | — | search | search |
| Country | — | authenticated search | search | search | — | search |
| Language | — | — | search | search | search | — |
| Depth | search (`fast`/`balanced`) | authenticated search | — | — | search | — |
| Search content chars | — | authenticated search | — | — | — | — |
| Fetch content chars | fetch | — | — | — | — | — |
| Search cache age | — | authenticated search | — | — | — | — |
| Fetch cache age | — | — | — | — | — | fetch |

PDF pages accept 1–10,000; content budgets accept 1–524,288 characters; cache
ages accept 0–31,536,000 seconds. Country is normalized to uppercase alpha-2,
language to a bounded lowercase BCP-47-like tag, and excluded domains use the
same lowercase DNS-only, non-IP grammar and 100-entry bound as web allowlists.
Duplicate or malformed exclusions reject the complete configuration.
You.com, Brave, and authenticated Parallel send country only for their
conservatively shared documented set; other valid alpha-2 values are omitted for
those adapters. You.com converts known language tags to its uppercase enum,
Brave sends only exact supported search languages (including `ja` → `jp`), and
other unsupported locale values are omitted rather than causing a
provider-invalid request.
Parallel currently honors search cache ages only from 600 seconds upward and
omits smaller general hints rather than sending a provider-invalid override.

Unsupported preferences are omitted rather than emulated by query rewriting,
post-filtering, or local truncation. Parallel's connection override header is
sent only with a configured API key because its anonymous service ignores those
settings. If a harness-authored `allowed_domains` restriction is present,
Tavily and Firecrawl receive that authoritative include list and omit configured
soft exclusions; Tau does not subtract domain sets or invent an
empty-intersection policy.

`search_depth: deep`, `fetch_pdf_parsing: ocr`, and fresh/live cache settings can
increase provider cost or latency. In particular, Firecrawl currently bills PDF
parsing per page. `fetch_pdf_parsing: disabled` sends `parsers: []`, but requests
only markdown and never projects a returned `rawBase64` file. A response without
markdown remains a bounded provider failure so hybrid failover can continue.
Firecrawl still charges one flat provider credit for that disabled-parsing PDF
attempt. This controls only Firecrawl: another fetch provider may still parse or
bill for the same PDF, and sequential failover may contact more than one
provider.
Provider-side character budgets remain hints; Tau still rejects projected output
over 512 KiB and never silently truncates a successful result.

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
