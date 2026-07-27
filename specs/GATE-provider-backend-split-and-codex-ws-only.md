# GATE-provider-backend-split-and-codex-ws-only: Isolate Codex and keep it WebSocket-only

## Gate

Generic OpenAI-compatible Chat Completions and the private ChatGPT OAuth/Codex
product contract must remain separate backends. Ordinary Codex Responses
inference must remain WebSocket-only without HTTP/SSE fallback.

## Justification

The user wants the private, unstable product contract isolated from the generic
protocol and wants one Codex transport behavior rather than divergent fallback
semantics.
