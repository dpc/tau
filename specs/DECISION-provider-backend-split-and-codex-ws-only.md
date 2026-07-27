# DECISION-provider-backend-split-and-codex-ws-only: Separate Chat Completions from WS-only Codex

Authority: confirmed, 2026-07-17, dpc

## Decision

Tau keeps generic OpenAI-compatible Chat Completions in
`tau-provider-chat-completions` and the private ChatGPT OAuth/Codex product
contract in `tau-provider-codex`. Ordinary Codex Responses inference is
WebSocket-only and never falls back to HTTP/SSE.

`tau-ext-provider-builtin` owns profiles, model publication and routing,
logical retries, and final provider-event policy. Each backend performs one
finite attempt. All provider transports share one immutable,
startup-captured outbound network policy.

## Rationale

The split isolates a private unstable product contract from the generic Chat
Completions protocol. WS-only Codex inference avoids divergent fallback
behavior, and one outer retry authority prevents replay or combination of
partial semantic output.

This record satisfies
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md)
and preserves the retry contract in
[DECISION-tau-ext-provider-builtin-required-work-retries](../crates/tau-ext-provider-builtin/specs/DECISION-tau-ext-provider-builtin-required-work-retries.md).
Component boundaries are described by
[ARCH-tau-provider-chat-completions](../crates/tau-provider-chat-completions/specs/ARCH-tau-provider-chat-completions.md),
[ARCH-tau-provider-codex](../crates/tau-provider-codex/specs/ARCH-tau-provider-codex.md),
and
[ARCH-tau-ext-provider-builtin](../crates/tau-ext-provider-builtin/specs/ARCH-tau-ext-provider-builtin.md).
