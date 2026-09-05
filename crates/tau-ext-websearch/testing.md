# Websearch testing

Tests never contact live providers. Use trait stubs for dispatch, validation,
concurrency, and lifecycle; use loopback HTTP only for actual wire headers,
payloads, status handling, redaction, and body caps. Lifecycle tests shut down
deterministically via Disconnect, release blocked workers, and drain expected
results; expected harness-side pipe closure must not panic.

Cover registration/default policy, endpoint parsing, rejection, and
application, provider argument forwarding and local rejection, no-Authorization
behavior, JSON/SSE decode, independent response/output/error caps, redaction,
saturation with responsive control handling, and replay suppression.
Cover independent search/fetch cursor order, admission-order reservation,
single-provider lists, normalized failover classes, empty-text failover,
three-attempt and total-deadline bounds, cancellation races, quota-attempt
counts, all-provider failure, and an adapter added without scheduler changes.
Production output lifecycle coverage blocks the real writer, exhausts the
64-frame detached FIFO, and requires one exact worker result/error after checked
admission resumes. Forced mandatory-write failure must exit the extension loop
without falsely publishing a terminal.

Display-state lifecycle tests must assert the same safe query/fetch target appears
on progress, success, error, and busy terminals. Exercise the production
paths, including a configured tool-name prefix. Treat query/URL labels as
untrusted metadata: cover control/layout escaping, byte bounds that preserve whole
escaped units, fetch-host projection without URL userinfo/query secrets, and the
absence of configured provider endpoints or returned content.
Composite lifecycle tests also assert exact ordered `…`, `✓`, `✗`, `∅`, `⏱`,
and `⊘` attempt chips on progress and every terminal, including cancellation
replay and long-list compaction.

Use loopback servers to verify that HTTP 429 takes the shared generic
rate-limit path for both hosted clients and never projects hostile, oversized
error bodies or endpoint secrets.

Successful-result coverage must exercise Exa and Parallel search/fetch and use
exact local fixtures for You.com, Brave, Tavily, and Firecrawl request,
authentication, response-normalization, and redaction behavior. Use
exact canonical `<tau_web_content>` attributes and loopback wire assertions for
the MCP providers' fetch argument adaptation. Adversarial
coverage keeps provider titles/URLs and attempted markup literal, replaces only
exact closing sentinels, makes unsafe Unicode visible, checks the exact final
512 KiB post-framing boundary
and oversize rejection, and proves identical preservation through Chat
Completions and Codex/Responses tool-result lowering.

Provider-preference coverage must retain an omission oracle for every existing
request fixture, exact wire tests for each supported mapping, authenticated and
anonymous Parallel header behavior, allowlist-over-exclusion authority, and
Firecrawl PDF fixtures proving `parsers: []` never projects `rawBase64`.
