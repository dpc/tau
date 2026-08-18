---
name: tau-self-knowledge-ext-websearch
description: Use this extension skill when the user asks about Tau's std-websearch extension, hybrid or explicit Exa/Parallel web_search/web_fetch tools, failover, MCP endpoints, or web search configuration.
advertise: false
---

# Tau std-websearch extension self-knowledge

`std-websearch` is Tau's built-in generic web search extension. It runs `tau-ext-websearch`, is enabled by default, and proxies Exa and Parallel.ai hosted MCP search/fetch providers into Tau tools.


## Tools

- Internal `websearch_hybrid_search` and `websearch_hybrid_fetch`, model-visible
  as `web_search` and `web_fetch`, are enabled by default. They rotate
  independently between Exa and Parallel and fail over sequentially.
- Exa and Parallel each provide search and fetch. Their four explicit internal
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
    config: {
      exa_endpoint: "https://mcp.exa.ai/mcp",
      // Legacy alias for exa_endpoint:
      endpoint: "https://mcp.exa.ai/mcp",
       parallel_endpoint: "https://search.parallel.ai/mcp",
       search_providers: ["exa", "parallel"],
       fetch_providers: ["exa", "parallel"],
    },
  },
}
```

Tau does not configure or send a Parallel API key; the built-in Parallel integration uses the default unauthenticated endpoint. If both `endpoint` and `exa_endpoint` are configured, they must be identical. Endpoint values are validated when configuration is applied: HTTPS is required except for loopback HTTP test endpoints, and userinfo credentials are rejected. Provider requests reject HTTP redirects rather than crossing the validated endpoint's scheme or origin; configure the final endpoint URL directly. Raw endpoints are not logged because URLs can contain credentials or query secrets. Provider transport diagnostics and JSON-RPC errors can become model-visible tool errors; Tau sanitizes configured endpoint echoes, request targets, query keys/values, fragments, and userinfo, then bounds the sanitized error text. Oversized JSON-RPC error messages are replaced with a compact deterministic error. HTTP 429 rate limits return `web service rate-limited the request; try again later.` without reading or echoing a provider body. HTTP calls use the platform root store, MCP protocol version `2025-06-18`, and accept JSON or SSE JSON-RPC responses. Composite HTTP attempts use scheduler-owned slices of one admission-anchored 45-second total deadline.

The ordered lists are independent; one entry selects explicit single-provider
mode. Cursors are extension-process memory, advance once per admitted call, and
reset on restart or successful configuration. Each call tries at most three
providers and shares one 45-second deadline dynamically across its remaining
attempts. Failover may consume quota at every provider it contacts.

Every successful result is an ordinary tool-result string enclosed in `<tau_web_content adapter="exa|parallel" operation="search|fetch" content_trust="external">…</tau_web_content>`. Tau first makes unsafe structural Unicode visible, then replaces only exact `</tau_web_content>` collisions; every other body byte remains literal, including markup, ampersands, quotes, and entity-like text. Titles, URLs, ranks, sources, and all other provider-returned metadata remain untrusted body claims. `adapter` identifies only Tau's configured adaptation path; it does not authenticate page authorship or truth. The sentinel prevents exact lexical breakout, not semantic prompt injection. The extension caps successful MCP response bodies and decoded provider text, then enforces the 512 KiB limit again on the final framed result; oversize projection is a tool error, never truncation. It allows up to eight in-flight web calls; additional calls fail fast with a busy tool error.
