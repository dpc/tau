---
name: tau-self-knowledge-ext-websearch
description: Use this extension skill when the user asks about Tau's std-websearch extension, hosted web_search/web_fetch providers, failover, endpoints, credentials, or web search configuration.
advertise: false
---

# Tau std-websearch extension self-knowledge

`std-websearch` is Tau's built-in generic web search extension. It runs `tau-ext-websearch`, is enabled by default, and adapts hosted web providers into Tau tools.

## Practical provider overview

Tau keeps separate provider pools for `web_search` and `web_fetch`. Their
round-robin cursors are independent: search defaults to Exa, Parallel, and
anonymous You.com; fetch defaults to Exa and Parallel.

| Adapter | Search | Fetch | Tau access | Current provider facts, verified September 4, 2026 |
|---|:---:|:---:|---|---|
| Exa | ✓ | ✓ | Default, anonymous | [Exa MCP][exa-mcp] needs no API key. Its free plan is provider-rate-limited; Tau has no Exa named-secret field. |
| Parallel | ✓ | ✓ | Default, anonymous | [Search MCP][parallel-mcp] is free without auth. A Parallel key raises limits, but Tau does not configure or send one. |
| You.com | ✓ | — | Default, anonymous | The `profile=free` route is [search-only, no-signup, and 100 queries/day][you-mcp]. Tau does not configure You.com's authenticated route. |
| Brave | ✓ | — | Optional named secret | Create an account, activate Search, and supply `brave_api_key_secret`. [Search is currently $5/1,000 calls][brave-plans]; plans include $5 monthly credit, but [there is no standalone free plan and a card is required][brave-faq]. |
| Tavily | ✓ | ✓ | Optional named secret | Create an account and supply `tavily_api_key_secret`. [Researcher is currently 1,000 credits/month with no card; Project is $30 for 4,000 credits][tavily-credits]. Tau's basic search costs one provider credit. |
| Firecrawl | ✓ | ✓ | Optional named secret | Supply `firecrawl_api_key_secret`. [Free is currently 1,000 credits/month with no card; Hobby is $16/month billed yearly for 5,000 credits/month][firecrawl-pricing]. Firecrawl now offers [keyless access][firecrawl-keyless], but Tau's current REST adapter still sends bearer auth and requires a key. |

These provider plans, quotas, and prices are external policies, not Tau
contracts; recheck the linked provider documentation before relying on them.

An admitted call advances only its operation's cursor once, then tries providers
sequentially in circular order until the first non-empty success, with at most
three attempts under one shared 45-second deadline. Tau returns that one result;
it does not merge providers. Every contacted provider may consume quota or incur
cost, including failed or empty attempts. Configuration, provider-list, and
named-secret changes are not watched: restart Tau (or restart `std-websearch`)
before expecting them to take effect; a restart resets both cursors.

Configure optional adapters by declaring their names and referring to those
names—never put key bytes in ordinary config:

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
```

[exa-mcp]: https://exa.ai/docs/reference/exa-mcp
[parallel-mcp]: https://docs.parallel.ai/integrations/mcp/programmatic-use
[you-mcp]: https://you.com/docs/build-with-agents/mcp-server
[brave-plans]: https://api-dashboard.search.brave.com/app/plans
[brave-faq]: https://api-dashboard.search.brave.com/documentation/resources/help-feedback
[tavily-credits]: https://docs.tavily.com/documentation/api-credits
[firecrawl-pricing]: https://www.firecrawl.dev/pricing
[firecrawl-keyless]: https://www.firecrawl.dev/blog/firecrawl-keyless-launch

Harness-level `agents.web_tools` policy may replace ordinary search with an
exact-route provider-hosted implementation at prompt materialization.
Caller-directed `web_fetch` remains external, and selection never switches
mid-turn.

The policy inherits through agent defaults, role groups, roles, and selected
profiles. `search` and `fetch` each contain named candidates with `enable`,
`priority`, `kind: model_provider|tool`, and the corresponding hosted
`access`/`context_size` or internal `tool` name. Candidates select in
`(priority, name)` order. `unavailable: omit` hides an unavailable capability;
`error` rejects the prompt before provider delivery. `allowed_domains: null`
clears an inherited policy, while `[]` denies all web access. The complete
schema is in `docs/providers.md`.

Cached hosted search still contacts the inference provider and may have cost or
privacy consequences; `live` additionally permits current external pages.
Ambiguous provider retry can repeat a paid hosted search. Lite routes never
infer hosted capability from their name and use the ordinary fallback.

Domain restrictions are not a network sandbox. Hosted filtering is delegated
to the exact provider route. External fetch gates only the requested target;
extractor redirects or subresources can leave the allowlist. A nonempty policy
makes the default external search pool unavailable because its adapters do not
advertise provider-side per-call enforcement. Configured Tavily or Firecrawl
can enforce the policy upstream; Tau does not call unsupported adapters and
post-filter.


## Tools

- Internal `websearch_hybrid_search` and `websearch_hybrid_fetch`, model-visible
  as `web_search` and `web_fetch`, are enabled by default. They rotate
  independently. Search defaults to Exa, Parallel, and anonymous You.com;
  fetch defaults to Exa and Parallel. Brave, Tavily, and Firecrawl are
  credentialed optional adapters.
- Exa and Parallel.ai each provide search and fetch. Their four explicit internal
  tools remain registered but disabled by default.

Search accepts `query` and optional `num_results` from 1 to 100; the default is
5. Fetch accepts one `url`. Explicit Parallel tools retain provider-specific
pass-through arguments.


## Terminal display

Tau's terminal tool headers show the submitted `query` for `web_search` and the
requested host from submitted `url` for `web_fetch` throughout progress, success,
error, and busy states. These short labels are escaped and bounded untrusted
metadata from the model tool call. Valid fetch labels omit URL userinfo/query
values; hostless URLs use a fixed marker, and all labels avoid configured Exa/Parallel MCP endpoints, provider
diagnostics, metadata, and content.
The same header shows ordered attempts, for example
`✗ Exa → ✓ Parallel`. Markers distinguish progress, success, failure, empty
text, deadline, and cancellation. The chip contains no raw errors or endpoints.


## Configuration

Configured under `extensions.std-websearch.config`:

```json5
extensions: {
  "std-websearch": {
    secrets: {
      brave_search: {},
      tavily: {},
      firecrawl: {},
    },
    config: {
      exa_endpoint: "https://mcp.exa.ai/mcp",
      // Legacy alias for exa_endpoint:
      endpoint: "https://mcp.exa.ai/mcp",
      parallel_endpoint: "https://search.parallel.ai/mcp",
      you_endpoint: "https://api.you.com/mcp?profile=free",
      search_providers: ["exa", "parallel", "you"],
      fetch_providers: ["exa", "parallel"],
      // Optional: brave, tavily, and firecrawl.
      brave_api_key_secret: "brave_search",
      tavily_api_key_secret: "tavily",
      firecrawl_api_key_secret: "firecrawl",
    },
  },
}
```

Tau does not configure or send a Parallel API key; the built-in Parallel integration uses the default unauthenticated endpoint. If both `endpoint` and `exa_endpoint` are configured, they must be identical. Endpoint values are validated when configuration is applied: HTTPS is required except for loopback HTTP test endpoints, and userinfo credentials are rejected. Provider requests reject HTTP redirects rather than crossing the validated endpoint's scheme or origin; configure the final endpoint URL directly. Raw endpoints are not logged because URLs can contain credentials or query secrets. Provider transport diagnostics and JSON-RPC errors can become model-visible tool errors; Tau sanitizes configured endpoint echoes, request targets, query keys/values, fragments, and userinfo, then bounds the sanitized error text. Oversized JSON-RPC error messages are replaced with a compact deterministic error. HTTP 429 rate limits return `web service rate-limited the request; try again later.` without reading or echoing a provider body. HTTP calls use the platform root store, MCP protocol version `2025-06-18`, and accept JSON or SSE JSON-RPC responses. Composite HTTP attempts use scheduler-owned slices of one admission-anchored 45-second total deadline.

Brave is search-only. Anonymous You.com is search-only. Tavily and Firecrawl
support both operations. Credentialed providers take API keys only through
named Tau secrets. Configuration and secret changes require an extension/Tau
restart; files are not watched.

The ordered lists are independent; one entry selects explicit single-provider
mode. Cursors are extension-process memory, advance once per admitted call, and
reset on restart or successful configuration. Each call tries at most three
providers and shares one 45-second deadline dynamically across its remaining
attempts. Failover may consume quota at every provider it contacts.

Every successful result is an ordinary tool-result string enclosed in `<tau_web_content adapter="exa|parallel|you|brave|tavily|firecrawl" operation="search|fetch" content_trust="external">…</tau_web_content>`. Tau first makes unsafe structural Unicode visible, then replaces only exact `</tau_web_content>` collisions; every other body byte remains literal, including markup, ampersands, quotes, and entity-like text. Titles, URLs, ranks, sources, and all other provider-returned metadata remain untrusted body claims. `adapter` identifies only Tau's configured adaptation path; it does not authenticate page authorship or truth. The sentinel prevents exact lexical breakout, not semantic prompt injection. The extension caps successful HTTP response bodies and decoded provider text, then enforces the 512 KiB limit again on the final framed result; oversize projection is a tool error, never truncation. It allows up to eight in-flight web calls; additional calls fail fast with a busy tool error.
