# Shared provider outbound security boundary

Provider debug captures are private sensitive artifacts that can contain full
prompts, tool results, model output, provider-controlled error bodies, and
reflected credentials. Tau does not intentionally serialize auth headers or
API-key configuration, but upstream responses and user/provider-configured
request fields can reflect secrets. Compression is not redaction.

The shared capture API accepts validated `SessionId` and `AgentPromptId` values
and constructs filenames through the `tau-config` capture contract. Its worker
requires existing real session roots and refuses symlinked session, debug, and
capture directories. Bounded `try_send` admission, zstd compression, and every
filesystem operation remain best-effort; overload, write failure, or nonjoining
process exit can omit a capture or leave a truncated final stream. Changes to
capture validation, content, compression, queueing, paths, or writes require
focused security and independent privacy review.

The `cache-diagnostic` class is a bounded scalar exception to raw payload
capture, not a public log. Its producer admits no arbitrary provider fields or
payloads, and known configured credentials are excluded from allowlisted model
identity strings. Full-record reservations include in-flight work and cap
metadata at 64 records / 16 MiB independently of raw-capture budgets. Each scalar
record is at most 256 KiB; identity strings are at most 128 UTF-8 bytes.
The harness still authenticates typed attribution and writes opaque bytes under
existing private paths and diagnostic retention, without interpreting the schema.

Provider startup captures one immutable `Arc<OutboundNetworkPolicy>`. The
policy reads lowercase proxy variables before their uppercase forms, selects
`HTTP_PROXY`/`http_proxy` for HTTP and WS, selects
`HTTPS_PROXY`/`https_proxy` for HTTPS and WSS, and falls back to
`ALL_PROXY`/`all_proxy`. `NO_PROXY`/`no_proxy` is the only intentional direct
bypass. A malformed selected value fails closed; clients disable their own
environment discovery and redirects, and a selected proxy is never replaced
with a direct connection after failure. Environment changes require restart.

Only HTTP and HTTPS proxies are accepted. Credentials are percent-decoded
exactly once, rejected when ambiguous or control-bearing, removed from stored
endpoint URLs, and supplied to reqwest through its sensitive proxy-auth
configuration. The policy's `Debug`, errors, and status projections contain
only closed route/phase facts. They never contain target/proxy URLs,
credentials, bearer tokens, account ids, CA paths/material, response bodies, or
library diagnostics.

TLS uses rustls with `rustls-platform-verifier` and its additive-extra-roots
API. `TAU_PROVIDER_CA_BUNDLE` may name one startup-read PEM bundle. It is
bounded, certificate-only, strictly parsed, and deduplicated by exact DER.
Every extra certificate must be accepted by the platform verifier. There is no
verification-disable path. The same prepared TLS configuration covers target
HTTPS/WSS and HTTPS-proxy TLS.

Provider HTTP and WebSocket upgrades use async reqwest. Prompt-bound futures
remain owned until completion, cancellation, or their absolute deadline;
dropping a canceled future prevents later result/socket publication. Plain
HTTP/WS proxy routes are not confidential from the selected proxy. SOCKS,
PAC/WPAD, OS GUI discovery, integrated proxy authentication, and redirects are
unsupported.

HTTP routes negotiate and decode gzip and zstd response bodies. Existing
response and SSE-line bounds apply after decoding; there is no separate encoded
body bound or encoded-byte statistic. WebSocket upgrades advertise the same
HTTP response codings, but the policy neither requests WebSocket extensions nor
decodes WebSocket frames.

`NO_PROXY` matching is syntactic and never resolves DNS: exact IP addresses and
CIDR networks match address literals, while host entries match DNS label
boundaries with an optional exact port. This avoids resolver-dependent bypasses,
but means a hostname resolving into a bypassed network is not itself bypassed.
Entries are comma-separated. `*` matches all targets; IPv4/IPv6 literals, CIDR,
DNS names, `.example.com`/`*.example.com` suffix spellings, and optional exact
ports are accepted. Brackets are required when an IPv6 literal has a port.
Empty comma fields are ignored. DNS labels must contain only ASCII letters,
digits, hyphens, and dots, with no empty label or leading/trailing hyphen. One
malformed non-empty entry rejects the entire startup snapshot atomically.

After this syntactic route choice, the selected direct target or proxy hostname
is still resolved by the operating-system resolver inside reqwest. Tau cannot
interrupt a platform `getaddrinfo` implementation which itself blocks, although
the owning async operation remains bounded/canceled and cannot publish a late
provider result. DNS rebinding does not retroactively change `NO_PROXY` because
matching never uses resolved addresses.

Reqwest does not expose a CONNECT tunnel response status through its public
error API. A hidden CONNECT 407 is classified as redacted Proxy/Transport, not
proven proxy authentication, and can therefore retry at transport cadence.
Plain HTTP/WS proxy 407 responses are visible and specifically typed. Error-text
inspection is prohibited.

The deterministic acceptance matrix covers direct HTTP and HTTPS; HTTP through
HTTP and HTTPS proxies; HTTPS through HTTP and HTTPS proxies; WS through an HTTP
proxy; and WSS through HTTP and HTTPS proxies. Nested secure routes prove outer
proxy TLS, exact authenticated CONNECT scope, inner target TLS, and origin
request or WebSocket upgrade.
Negative coverage proves untrusted proxy and target TLS, hidden CONNECT
rejection, and upgrade rejection without direct fallback. Deterministic
resolver injection additionally proves selected-proxy DNS failure cannot
resolve or reach the direct target; an accepted early-close proxy covers socket
failure.

The same matrix covers lowercase precedence, ALL_PROXY fallback, syntactic
NO_PROXY host/port/CIDR matching, every named non-UTF-8 proxy/bypass variable,
immutable route and CA startup state, strict mixed/duplicate/malformed CA
bundles, plain-proxy 407 classification, backend redaction canaries,
unsolicited WS extension/subprotocol rejection, OpenRouter cache fallback
boundaries, and joined compact cancellation. Stalled Chat Completions header
and body reads and fresh WebSocket connection waits have separate deterministic
cancellation coverage.

Reqwest's preconfigured-rustls hook is version-coupled. Dependency upgrades,
proxy/upgrade changes, new content decoding, or changes to cancellation
ownership require deterministic direct/proxy/TLS/no-fallback/redaction tests
and focused security review.
