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

The caller must own refresh serialization, durable storage, account pinning, and
generation rejection. Refresh omission preserves the old refresh token at that
boundary. A missing lifetime remains unknown; this crate does not invent a TTL.
A lost refresh response may already have consumed the token and is not retried.
The device flow requires a renewable credential before returning success.

## Remaining integration

| Capability | State |
| --- | --- |
| Provider registration, profile setup, Secret lifecycle | Not implemented |
| Grok inference, model discovery, images and local tools | Not implemented |
| Native compaction, hosted tools, quota display | Not implemented |
| Default OAuth registration | Caller supplies client ID; product decision pending |
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
They perform no login, live inference, paid requests, or account changes.
