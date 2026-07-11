---
name: tau-self-knowledge-ext-provider-builtin
description: Use this extension skill when the user asks about Tau's provider-builtin extension, built-in model providers, ChatGPT/Codex OAuth, OpenAI-compatible Chat Completions, OpenRouter, provider profiles, model publication, or tau provider commands.
advertise: false
---

# Tau provider-builtin extension self-knowledge

`provider-builtin` is Tau's built-in provider extension. It runs `tau-ext-provider-builtin`, is enabled by default, publishes available models from configured provider profiles, and executes agent turns for built-in provider backends.

## Provider retries

Required ChatGPT/Codex Responses, Chat Completions, and OpenRouter work keeps
retrying during the running session until success or explicit cancellation,
unless Tau can positively prove the unchanged request is deterministic and
invalid. Unknown provider failures retry conservatively; billing, quota,
credits, usage windows, and reloadable auth/configuration retry slowly.

Delayed work is parked in one in-memory scheduler and does not consume a
provider worker. Tau shows a retry status with the reason, next delay, and
cancellation instruction. Same-profile limits share cooldown state, trusted
server reset hints are not shortened, and mutable profiles are reloaded before
later attempts. Retry state does not survive a cold process restart.

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

ChatGPT/Codex turns use the Responses backend. Conversation chains reuse `previous_response_id` when possible so follow-up requests can send only newly added messages while upstream carries reasoning state. If an upstream stored response id expires, Tau retries once with a full replay within that finite provider attempt; an ambiguous failed attempt then returns to the logical-prompt scheduler.

ChatGPT GPT-5.6 Sol, Terra, and Luna publish a 353,400-token effective context
window and include `max` among their published reasoning choices. Normal inference
uses Responses Lite without legacy inline context management. Manual and automatic
compaction use the separate unary `/codex/responses/compact` operation and install
one validated replacement-window transcript boundary.

The ChatGPT/Codex surface also uses a persistent WebSocket connection pool keyed by account and agent so upstream connection-local caches stay warm across turns, including interleaved sub-agent delegations. Prompt-cache keys are stable per target agent and do not split based on whether a turn came from the user, an extension, a manager relay, or an agent-to-agent message. Refreshed OAuth tokens invalidate stale sockets on next use. WebSocket-capable ChatGPT/Codex turns remain on WebSocket: retryable WS failures return to the in-memory logical-prompt scheduler, while proven terminal WS errors surface instead of silently falling back to HTTP/SSE.

ChatGPT/Codex live streams have a default five-minute idle watchdog on both HTTP/SSE and WebSocket. The timer resets on each SSE `data:` event or WebSocket provider frame, not on SSE comments/heartbeats or partial-line byte trickles, and is not an absolute turn-duration cap. If upstream stalls, Tau aborts that finite attempt, clears tentative output, and parks the logical prompt for another attempt.

Prompt execution concurrency defaults to 4 and can be overridden with `TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY`. Retry delays release those worker slots for every prompt origin. Policy-generated jittered delays reach about one minute for transient failures and at most about thirty minutes for persistent failures; trusted later `Retry-After` and reset hints remain lower bounds. Retry state exists only for the running process/session and is not replayed after cold restart.


Provider response streaming note: built-in providers publish transient `provider.response_updated` append deltas for visible assistant/reasoning text. Retry diagnostics are provider status updates, and complete durable assistant output is committed through `provider.response_finished`.

Live byte stats are public provider-owned `provider.response_updated.response_stats`
samples. Built-in providers count backend response bytes at the transport receive
boundary before semantic parsing, include non-visible streamed tool-call
arguments, and do not count request bytes, HTTP headers, or tool execution
outputs/results. They are not transcript or final-response content.

Built-in providers also run a conservative exact streaming repetition guard over assistant text, reasoning text, and tool-call argument deltas. When a high-volume tight exact loop is detected, the provider clears transient streamed output and finishes with `stop_reason: repetition_detected`, empty final output items, and bounded display-only error text; the harness treats this as a loop-guard trigger rather than retrying the provider turn.

### Watcher-visible provider work

Provider retries carry closed structured categories, saturating attempt counts, and approximate bounded delays independently of human UI prose. After validating prompt ownership, the harness owns the current per-agent/turn/prompt snapshot and session-local watcher fanout. Live delivery is limited to first category, category/phase changes, and terminal failure; same-category storms only refresh the late-watch snapshot. Enabling or re-enabling returns current sanitized state and emits an initial client snapshot without prompting the model. Durable live facts replay as transcript context without re-fanout; disable, prune, and session change stop delivery. Raw provider bodies, status text, errors, headers, account data, secrets, and prompt content never cross this boundary.
