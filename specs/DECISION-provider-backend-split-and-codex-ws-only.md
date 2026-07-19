# DECISION-provider-backend-split-and-codex-ws-only: Separate Chat Completions from WS-only Codex

Authority: confirmed, 2026-07-17, dpc

Tau keeps generic OpenAI-compatible Chat Completions in
`tau-provider-chat-completions` and the private ChatGPT OAuth/Codex product
contract in `tau-provider-codex`. Ordinary Codex Responses inference is
WebSocket-only and never falls back to HTTP/SSE. Codex retains HTTPS only for
OAuth/token refresh, quota usage, and unary standalone compaction.

`tau-ext-provider-builtin` owns profiles, model publication and routing,
credential reload, logical required-work retry, cooldowns, cancellation, and
final provider-event policy. Each backend performs one finite attempt and owns
its wire lowering, transport, parsing, accumulation, and provider-specific
classification. `tau-provider` contains only provider-independent storage,
retry/repetition facts, cancellation, and outbound network policy; it is not a
generic backend framework.

Chat Completions and Codex do not share request structures or depend on each
other. The split preserves profile kinds and namespaces, CLI/model names,
OpenRouter, Chat Completions SSE, explicit Responses Lite compatibility, and
durable protocol records. The internal crate rename and removal of low-level
Codex HTTP-inference APIs are accepted source breaks; there is no compatibility
package or shim.

All provider HTTP, HTTPS, and WSS operations share one immutable startup-captured
reqwest/rustls outbound policy. It uses platform roots plus optional
`TAU_PROVIDER_CA_BUNDLE`, lowercase-first proxy variable precedence, and
`no_proxy`/`NO_PROXY`. Malformed selected values fail closed. A selected proxy's
failure never falls back direct, redirects are disabled, certificate verification
cannot be disabled, and prompt-bound connection phases remain bounded and
cooperatively cancellable. Proxy and trust configuration changes require restart.

Codex may perform only bounded WS-to-WS transport repair before semantic model
output exists. It never silently replays or combines partial semantic output after
retry, cancellation, shutdown, or terminal failure. This preserves one outer
retry authority and prevents nondeterministic output splicing.

The main tradeoffs are that networks blocking WebSocket Upgrade cannot use the
Codex provider, the private backend remains coupled to an unstable upstream
product contract, and the source API break removes unreachable compatibility
surfaces. In return, supported transport and ownership boundaries are explicit
and inconsistent proxy, fallback, and credential behavior are eliminated.

This record satisfies
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md)
and preserves the retry contract in
[DECISION-tau-ext-provider-builtin-required-work-retries](../crates/tau-ext-provider-builtin/specs/DECISION-tau-ext-provider-builtin-required-work-retries.md).
Component boundaries are described by
[ARCH-tau-provider-chat-completions](../crates/tau-provider-chat-completions/specs/ARCH-tau-provider-chat-completions.md),
[ARCH-tau-provider-codex](../crates/tau-provider-codex/specs/ARCH-tau-provider-codex.md),
and
[ARCH-tau-ext-provider-builtin](../crates/tau-ext-provider-builtin/specs/ARCH-tau-ext-provider-builtin.md).
