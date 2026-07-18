# DECISION-provider-backend-split-and-codex-ws-only: Separate Chat Completions from WS-only Codex

Authority: confirmed, 2026-07-17, dpc

## Status

The implementation still uses `tau-provider-chatgpt`, permits HTTP/SSE Codex
inference for non-WebSocket configurations, and distributes provider ownership
across the existing crates. The stable user/protocol surfaces and non-goals below
already constrain the code; the crate split, WS-only inference, shared network
policy, and revised recovery boundaries do not yet apply. This record is the
approved target for `tau-agent-6fjo`; until that cutover completes, current
component architecture records continue to describe the implemented boundaries.

## Executive summary

- Keep `tau-provider-chat-completions` as the generic OpenAI-compatible
  Chat Completions backend used by llama.cpp, other compatible servers, and
  OpenRouter.
- Rename the private ChatGPT-subscription implementation from
  `tau-provider-chatgpt` to `tau-provider-codex`. Ordinary Codex Responses
  inference is WebSocket-only and never falls back to HTTP/SSE.
- Preserve the HTTPS operations the Codex product still requires: OAuth/token
  refresh, `/wham/usage`, and unary `/codex/responses/compact`.
- Make `tau-provider` the small shared home for provider-independent storage,
  retry/repetition facts, cancellation, and one outbound HTTP/WSS policy.
- Preserve user profiles, provider namespaces, models, protocol records, Lite,
  OpenRouter, and Chat Completions SSE. Accept the internal Rust package/API
  break and the product requirement that Codex environments support WS.

## Decision and rationale

Tau already has separate Chat Completions and ChatGPT/Codex implementations, but
some ownership and APIs still imply a generic OpenAI provider or a supported
Codex HTTP inference route. Neither implication matches the supported product.
The useful change is therefore a boundary correction and removal of unreachable
compatibility code, not a new provider framework.

Chat Completions remains the broad compatibility surface because local servers
such as llama.cpp implement it directly, including streamed Function tool calls,
while their Responses support need not implement response-id chaining or the
Codex WS contract. The crate is not named “local” because remote compatible
services and OpenRouter are supported consumers.

The Codex crate represents the private ChatGPT OAuth/Codex product contract. It
is not a public API-key OpenAI Responses client. Its name, configuration, and
runtime must make that distinction explicit.

## Ownership boundaries

`tau-ext-provider-builtin` owns the user and harness boundary:

- serialized `auth.d/<provider>.json` profiles and filename-derived provider
  namespaces;
- model publication and exact configured-model routing;
- mutable profile/credential reload and the OAuth refresh/storage transaction;
- OpenRouter profile, discovery/cache, and CLI behavior;
- logical required-work retry, cooldowns, cancellation ownership, public
  response sampling, and final provider event ordering.

Backend crates execute one finite provider attempt and return typed progress,
success, retry, cancellation, or terminal failure. They own request lowering,
wire transport, parsing, accumulation, provider-specific classification, and
strictly bounded transport repair. They do not write harness messages, sleep for
logical retry, or own durable event policy.

`tau-provider-chat-completions` owns only the generic `/chat/completions` wire
contract: HTTP POST, SSE, semantic replay, Function tools, raw streamed tool
arguments, usage/reasoning compatibility, optional bearer auth, and the saved
compatibility/extra-body controls needed by real compatible servers. Provider
stream errors are typed provider failures, never assistant text and never
OpenRouter-branded output.

`tau-provider-codex` owns the ChatGPT/Codex model matrix, OpenAI OAuth wire
protocol, Standard and explicit Lite Responses lowering, the Responses event
parser and replay sidecars, WS pool/prewarm/chaining/recovery, WS quota events,
`/wham` parsing, unary compact, and Codex VCR/debug behavior.

`tau-provider` owns only shared provider-independent facilities: auth/profile
storage primitives, structured retry facts, repetition guards, cooperative
cancellation, and the outbound network policy. It does not own Codex OAuth
endpoints, GitHub/Copilot OAuth, OpenAI request schemas, model matrices, or a
generic backend trait. The unused GitHub/Copilot OAuth surface is removed.

Dependency direction remains extension to both backend crates, and both backend
crates to `tau-provider`/`tau-proto`; no backend depends on the extension or on
the other backend. Chat Completions and Responses do not share request structs.

## Stable user and protocol surface

No profile or durable-record migration is required. The following remain
stable:

- profile kinds `chatgpt`, `chat_completions`, and `openrouter`;
- profile filenames/provider namespaces, the `chatgpt` CLI spelling,
  `chatgpt/*` model ids, and `shell:chatgpt` tags;
- ChatGPT OAuth fields and startup-scoped `responses_lite_compatibility`;
- current published model metadata and supported model/tool capabilities;
- Chat Completions base URL, optional API key, model list, tags, compatibility
  controls, output-token setting, and arbitrary non-conflicting `extra_body`;
- OpenRouter discovery/cache and routing through Chat Completions;
- durable `ProviderBackendKind::{ChatCompletions, Responses}` and
  `ProviderBackendTransport::{HttpSse, Websocket}`, replay sidecars, and old
  session decoding.

Capability publication and optional request-field emission become distinct,
while defaults preserve model publication and existing JSON without edits.

The crate rename and deletion of low-level Codex SSE APIs are intentional Rust
source breaks for direct package consumers. Tau does not retain a deprecated
package or an HTTP inference shim solely for unknown downstream callers.

## Codex WS-only inference

Ordinary Codex inference always uses the private Codex Responses WS endpoint.
There is no `supports_websocket` switch, Responses-surface selector, initial
HTTP/SSE transport, runtime fallback, or dormant feature flag. Codex inference
backend facts use `Responses` and `Websocket`.

The transport-independent Responses request/event logic remains: Standard and
Lite lowering, Function and Custom tools, reasoning/encrypted reasoning,
images where published, service tier, prompt cache, context management,
`store:false`, response-id chaining, usage, opaque items, and replay fidelity.
The WS pool, cancellation, keepalive, age-out, same-key serialization,
different-key concurrency, debug capture, and WS VCR also remain.

HTTPS remains part of the Codex crate only for supported non-inference work:
OAuth exchange/refresh, full quota fetch, and standalone compact. Compact resets
the usable WS response chain so the following inference sends full context.
Global `HttpSse` protocol compatibility remains because Chat Completions and old
records still use it.

A WS upgrade `426` is a terminal actionable request/protocol failure; Tau does
not leave it pending waiting for a fallback that cannot occur. Authentication,
quota, overload, and transport failures retain structured retry categories and
the existing required-work scheduler behavior.

This is an intentional product difference from the current official Codex
client, which may activate HTTP Responses fallback. Tau chooses a smaller,
honest supported surface over that fallback.

## Shared outbound network policy

Every provider HTTP, HTTPS, and WSS operation uses one immutable outbound policy
snapshot created at provider startup. It consistently covers Chat Completions,
OpenRouter discovery, Codex OAuth/quota/compact, and Codex WS setup.

The default policy reads standard `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and
lowercase equivalents, plus `NO_PROXY`/`no_proxy`. `NO_PROXY` is an intentional
direct-route decision. Once a proxy is selected for a target, connection,
authentication, CONNECT, TLS, or timeout failure never falls back to a direct
connection.

HTTPS and WSS use HTTP CONNECT through the selected proxy, followed by target
TLS and then the request or WS upgrade. Target bearer/account credentials are
not sent to the proxy or before target TLS. The same platform-root and optional
additional-CA policy applies to provider HTTPS/WSS and HTTPS proxy TLS;
certificate verification cannot be disabled.

Underlying HTTP/WS libraries do not independently rediscover environment
proxies or follow redirects. This prevents inconsistent `NO_PROXY` behavior,
double proxying, credential forwarding, and silent direct bypass. All
prompt-bound connection phases are bounded and cooperatively cancelable.

Network environment is startup configuration. Changes require restart. Invalid
proxy/NO_PROXY/CA state is exposed as bounded configuration status and remains a
retryable configuration outcome for prompt work, preserving confirmed
required-work semantics and avoiding a false durable claim that the provider
rejected the request. Non-prompt CLI/discovery/quota operations retain their
normal immediate error, cache, or best-effort behavior.

Plain HTTP Chat Completions remains supported for local/compatible servers. It
provides no TLS confidentiality from a selected HTTP proxy, so users must trust
that route or configure HTTPS. SOCKS, PAC/WPAD, OS GUI proxy discovery,
enterprise integrated proxy authentication, and persisted proxy settings are
not part of this decision.

## Recovery and partial-output safety

The extension continues to own indefinite required-work retry outside the
bounded prompt worker. Backends may perform only bounded, immediate transport
repair inside one attempt; they do not sleep or create a second retry policy.

Codex may replace a limited/dead WS or clear an unknown previous response id and
resend full context once before returning to outer retry. These are WS-to-WS
recovery, not fallback. Exact stale-chain and connection-limit codes take
precedence over generic HTTP-status classification.

No backend silently replays after it has parsed semantic model output. Once
assistant text, reasoning, tool data, or another output item exists, a retry,
cancellation, shutdown, or terminal failure clears transient response state
before the next status/final event. Partial output is never spliced with a
nondeterministic replay or committed as durable completion.

Successful `generate:false` prewarm may retain its response id only for the same
socket and an exact compatible request whose lowered input is the warmed input
plus an optional suffix. Otherwise the warm anchor is discarded and inference
sends full context. Failed or canceled prewarm installs no socket, response id,
or chain baseline.

Provider status/codes and trusted retry/reset headers remain classification
authority; arbitrary provider prose does not. Proxy credentials, OAuth/bearer
tokens, account ids, prompts, raw error bodies, and CA material never enter safe
status or ordinary logs.

## Accepted tradeoffs and consequences

- A network that blocks WS Upgrade cannot run the `chatgpt`/Codex provider; Tau
  reports the typed failure rather than degrading to SSE.
- Corporate/restricted environments gain consistent HTTP/WSS proxy and trust
  behavior, but proxy/environment changes require process restart.
- The private Codex crate remains coupled to an upstream product contract whose
  endpoint, headers, and quota events are not a public stability guarantee.
- The internal crate/API break is accepted to remove unreachable code and make
  supported behavior unrepresentable as HTTP inference.

## Non-goals

This decision does not:

- add a public API-key OpenAI Responses provider;
- add Codex HTTP/SSE inference or any WS-to-HTTP fallback;
- change the Codex model catalog, Standard-default/Lite-opt-in policy, model
  tags, or model-native tool surface;
- remove OpenRouter, Chat Completions SSE, compatible-server controls, or old
  replay sidecars;
- create a generic provider framework or shared OpenAI request model;
- add programmatic/code-mode tools, hosted tools, Realtime/WebRTC, or multiple
  in-flight responses per socket;
- change required-work scheduler policy, persistence/event schemas, repetition
  semantics, quota pacing policy, or standalone-compaction product semantics.

## Linked Specs

This record satisfies
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
It preserves the event/retry authority of
[SPEC-provider-response-streaming](SPEC-provider-response-streaming.md),
[DECISION-tau-ext-provider-builtin-required-work-retries](../crates/tau-ext-provider-builtin/specs/DECISION-tau-ext-provider-builtin-required-work-retries.md),
and
[DECISION-tau-ext-provider-builtin-structured-retry-facts](../crates/tau-ext-provider-builtin/specs/DECISION-tau-ext-provider-builtin-structured-retry-facts.md).

It amends
[DECISION-provider-quota-pacing](DECISION-provider-quota-pacing.md) only by removing
Codex HTTP-inference response headers as a sparse quota source. Codex sparse
quota observations are WS `codex.rate_limits`; `/wham/usage` remains the full
snapshot source.

Implementation updates the ownership/name statements in
[ARCH-tau-provider-chat-completions](../crates/tau-provider-chat-completions/specs/ARCH-tau-provider-chat-completions.md),
[ARCH-tau-provider-chatgpt](../crates/tau-provider-chatgpt/specs/ARCH-tau-provider-chatgpt.md),
[DECISION-tau-ext-provider-builtin-profile-ownership](../crates/tau-ext-provider-builtin/specs/DECISION-tau-ext-provider-builtin-profile-ownership.md),
and
[DECISION-tau-provider-chatgpt-responses-surface-selection](../crates/tau-provider-chatgpt/specs/DECISION-tau-provider-chatgpt-responses-surface-selection.md)
without changing their unrelated authority.

The unconfirmed
[DECISION-tau-ext-provider-builtin-standalone-compaction](../crates/tau-ext-provider-builtin/specs/DECISION-tau-ext-provider-builtin-standalone-compaction.md)
is not implicitly confirmed. This decision preserves the current unary compact
route and post-compact full-context WS behavior only.
