# Grok protocol support

This crate owns xAI-specific behavior rather than adapting private ChatGPT/Codex
authentication. It is an incremental implementation, **not yet a registered Tau
inference provider or complete subscription integration**.

## Implemented

- Native OAuth device authorization and bounded, cancellable RFC 8628 polling.
- One-shot rotating refresh exchange with no transparent HTTP retries.
- Authenticated userinfo subject lookup; no identity inferred from unsigned JWTs.
- Fixed production `auth.x.ai` origin, Tau outbound network policy, no redirects,
  credential-free errors, and bounded decoded response bodies.
- Caller-supplied public OAuth client ID; no Grok Build identity/version headers,
  cookie scraping, shared auth-file access, browser launch, or paid-key fallback.
- Closed version-zero `grok_oauth` records with access/refresh tokens, optional
  advertised expiry, and a validated account subject. Storage callbacks cover
  authoritative read, one refresh, exact-byte CAS, and authoritative reload.
  Production process-local single-flight/runtime wiring is not implemented.
- Public API model discovery with bounded responses, exact integer pricing,
  optional context limits and server-advertised reasoning selectors. Catalog
  membership is not a subscription entitlement or text-inference claim.
- Isolated full-replay Responses request lowering: `store:false`, encrypted
  reasoning inclusion, per-agent sticky routing, and no OpenAI cache TTL/options.
  Selected reasoning must match the advertised selector exactly; omission keeps
  the upstream alias default.
- Native typed image tool outputs on audited image-capable routes, kept inside
  `function_call_output` rather than synthetic user messages. The request-wide
  24 MiB raw / 32 MiB data-URL bounds and unsupported routes produce explicit
  image omissions. Admitted opaque replay retains its exact raw JSON.

The caller supplies bounded Secret read/CAS callbacks; the
crate opens no state or credential files. Production ownership must comply with
[`GATE-extension-filesystem-mediation`](../../specs/GATE-extension-filesystem-mediation.md).
Runtime/setup wiring remains pending. The intended baseline matches Codex:
same-generation refreshes coalesce inside one process, and Secret CAS arbitrates
saved generations. It does not serialize token exchanges between processes.
Workers check cancellation before
refreshing; once rotation starts they must finish publication even if the waiter
cancels. Dropping a future does not cancel remote work. This callback policy
does not solve a process crash after a refresh token was consumed; concurrent
cross-process exchanges or a crash before saving can require logging in again.
There is no production file lock, new RPC, or durable refresh-claim record.

Refresh omission preserves the old refresh token and expiry. An unknown lifetime
stays unknown; no TTL is invented. A lost refresh response may already have
consumed the token and is not retried. Callers must suppress another automatic
refresh of a failed observed generation. Account changes require explicit
readmission; stale refreshes do not create missing records. Lost/failed CAS replies
stop admission even when a replacement is visible, because visibility alone
does not establish completed durability. The device flow
requires a renewable credential before returning success.

## Remaining integration

| Capability | State |
| --- | --- |
| Provider registration, profile setup, Secret lifecycle | Storage policy library implemented; runtime/setup wiring pending |
| Model discovery | Library implemented; not integrated into profile setup |
| Images and local tools | Request lowering implemented; no runtime inference |
| Grok inference | Partial-limit policy implemented; runtime not registered |
| Native compaction, hosted tools, quota display | Not implemented |
| Default OAuth registration | Shared public CLI registration approved; runtime default wiring pending |
| Subscription entitlement, Fast model route | Not credential-verified |

There is no remote logout/revocation or billing mutation in this crate.
Subscription use must not be presented as free/unlimited or as proof that paid
extra usage cannot be consumed.

## Evidence and verification

Protocol basis: xAI's public OIDC discovery at
`https://auth.x.ai/.well-known/openid-configuration`, and the endorsed OpenCode
integration at commit `cb88db6ce31dfbf52b2462258b42a607758e200a`,
`packages/opencode/src/plugin/xai.ts`, inspected September 24, 2026.
The upstream shared public client registration's general reuse policy was not
established; public/non-secret does not itself document third-party eligibility.

Tests use synthetic loopback HTTP responses, not captured account traffic.

Request and discovery policy additionally follows the public xAI inference API
documentation (`docs.x.ai`, Responses, images, functions and model endpoints),
inspected September 24, 2026, and the public Grok Build request types at commit
`f0e3be1100ef5252488e3be8bb0e91cf68d8c305`,
`crates/codegen/xai-grok-sampling-types/src/conversation/responses.rs`.
No production inference, login, subscription entitlement, quota or Fast route was
credential-verified. The request lowerer deliberately provides no inference
entry point. Prepared requests select nonretryable Error handling for exact
`max_prompt_tokens` and `max_time_limit` incomplete terminals, preserving validated
partial assistant prose and terminal accounting while stripping all tools and
opaque output. Neither limit permits automatic resend, output-length
continuation or context-overflow recovery.
They perform no login, live inference, paid requests, or account changes.
