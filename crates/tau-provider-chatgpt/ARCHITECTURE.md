# tau-provider-chatgpt architecture

This crate contains the ChatGPT/Codex provider transport implementation shared by the built-in provider extension. It owns request construction, Responses HTTP/SSE handling, persistent Responses WebSocket pooling, provider-cache key derivation, and provider-specific retry/error mapping.

## Prompt-cache identity

First-party ChatGPT/Codex prompt-cache keys are stable per provider base URL and durable target `AgentId`. Prompt provenance (`PromptOriginator`) is intentionally not part of the key: a target agent must stay on the same provider cache bucket whether a turn came from direct user input, extension-originated work, a manager relay, or an agent-to-agent message.

The legacy `share_user_cache_key` prompt flag is retained for persisted events and older providers, but this crate treats it as a no-op for cache-bucket selection. Any future cache-sharing behavior should be explicit agent metadata (for example, a reviewed `share_cache_from` design) rather than inferring cache identity from prompt provenance.

WebSocket pool keys must follow the same identity as request `prompt_cache_key` values so upstream thread/session headers and request bodies target the same cache bucket.

## Model metadata tags

ChatGPT/Codex model publication includes provider-owned capability tags such as
`shell:chatgpt` and `tools:custom-text`. These tags describe the model/backend
surface; the harness owns all policy that maps them to tool alternatives.
