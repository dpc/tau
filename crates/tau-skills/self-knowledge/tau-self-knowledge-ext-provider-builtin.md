---
name: tau-self-knowledge-ext-provider-builtin
description: Use this extension skill when the user asks about Tau's provider-builtin extension, built-in model providers, ChatGPT/Codex OAuth, OpenAI-compatible Chat Completions, OpenRouter, provider profiles, model publication, or tau provider commands.
advertise: false
---

# Tau provider-builtin extension self-knowledge

`provider-builtin` is Tau's built-in provider extension. It runs `tau-ext-provider-builtin`, is enabled by default, publishes available models from configured provider profiles, and executes agent turns for built-in provider backends.


## Provider profiles and CLI

Provider profiles live as JSON files under Tau state `auth.d/` (`~/.local/state/tau/auth.d/<name>.json` on typical Linux systems). Manage them with:

```sh
tau provider add
tau provider list
tau provider remove <name>
```

Supported profile kinds:

- `chatgpt` — ChatGPT/Codex OAuth credentials for the Responses backend.
- `chat_completions` — OpenAI-compatible Chat Completions endpoint with base URL, optional API key, model list, max output tokens, extra body, and compatibility options. `tau provider add` accepts `chat-completions` at the interactive provider-kind prompt.
- `openrouter` — OpenRouter profile with API key and either explicit models or models fetched from OpenRouter.

The extension has no ordinary `extensions.provider-builtin.config` schema for provider credentials; credentials belong in provider auth/profile storage, not harness config.
ChatGPT profiles publish model tags such as `shell:chatgpt` and `tools:custom-text` so the harness can choose compatible tool surfaces. Chat Completions profiles and individual models can also carry optional `tags`; published model metadata contains the provider/model tag union.


## Runtime behavior

The harness assembles prompts and routes provider-owned turns to this extension. The extension publishes `ProviderModelsUpdated`, streams response updates, and emits final response events with stop reasons and usage/cache diagnostics.

ChatGPT/Codex turns use the Responses backend. Conversation chains reuse `previous_response_id` when possible so follow-up requests can send only newly added messages while upstream carries reasoning state. If an upstream stored response id expires, Tau retries once with a full replay before surfacing the error.

The ChatGPT/Codex surface also uses a persistent WebSocket connection pool keyed by account and agent so upstream connection-local caches stay warm across turns, including interleaved sub-agent delegations. Prompt-cache keys are stable per target agent and do not split based on whether a turn came from the user, an extension, a manager relay, or an agent-to-agent message. Refreshed OAuth tokens invalidate stale sockets on next use.

Prompt execution concurrency defaults to 4 and can be overridden with `TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY`. Main-agent transient provider errors retry with a Fibonacci-like backoff up to roughly nine minutes; extension-originated side turns use a smaller retry cap.


Provider response streaming note: built-in providers publish transient `provider.response_updated` append deltas for visible assistant/reasoning text. Retry diagnostics are provider status updates, and complete durable assistant output is committed through `provider.response_finished`.
