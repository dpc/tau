# Shared provider outbound security boundary

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

The current deterministic acceptance matrix covers these exact topologies:
direct HTTP; HTTP through an HTTP proxy (absolute form and Basic auth); HTTP
through an HTTPS proxy using an additive custom CA; direct HTTPS using an
additive custom CA; WS through an HTTP proxy; and WSS through an HTTP proxy with
CONNECT followed by custom-CA target TLS. It also covers lowercase precedence,
ALL_PROXY fallback, syntactic NO_PROXY host/port/CIDR matching, plain-proxy 407
classification, unsolicited WS extension/subprotocol rejection, compact
cancellation cleanup, malformed configuration, and no direct fallback after a
selected cleartext HTTP proxy connection fails.

This phase does not claim deterministic acceptance coverage for nested
HTTPS/WSS-through-HTTPS-proxy TLS, every DNS/refusal/stall/cancellation failure
phase, or every negative platform-TLS and mixed-bundle case. Do not broaden the
supported topology or failure-mode claims until those matrix cases exist.

Reqwest's preconfigured-rustls hook is version-coupled. Dependency upgrades,
proxy/upgrade changes, new content decoding, or changes to cancellation
ownership require deterministic direct/proxy/TLS/no-fallback/redaction tests
and focused security review.
