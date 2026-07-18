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

Within one provider process, a permanently rejected ChatGPT OAuth refresh is
suppressed for the exact unchanged credential/profile generation across startup
quota, prewarm, prompt, retry, and scheduled quota resolution. Credential or
profile change permits a new attempt; cold restart may probe once again. A
failed preemptive refresh may fall back only to the authoritative locked access
token while it remains valid, never to an expired or stale pre-lock bearer. This
does not change the logical prompt's slow authentication retry cadence.

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

ChatGPT's full account quota snapshot is acquired best-effort from `/wham/usage`
and reconciled with sparse in-band WebSocket `codex.rate_limits` observations
without delaying model work. Tau shows neutral `Q?` for a selected model when provider quota
current-state exists but weekly data is absent, unbound, stale, expired, or
timing-untrusted. `Q-`, `Q=`, `Q+`, and `Q!` require a fresh explicit in-band
pool binding for the exact model. Tau does not infer colored applicability from
a default/sole pool, treat credits as weekly usage, or fabricate a reset when a
cached boundary passes. Quota capability remains available for the running
harness after account state is cleared, keeping live and late clients on the
same neutral unknown state.


## Runtime behavior

The harness assembles prompts and routes provider-owned turns to this extension. The extension publishes `ProviderModelsUpdated`, streams response updates, and emits final response events with stop reasons and usage/cache diagnostics.

ChatGPT/Codex turns use the Responses backend. Conversation chains reuse `previous_response_id` when possible so follow-up requests can send only newly added messages while upstream carries reasoning state. If an upstream stored response id expires, Tau retries once with a full replay within that finite provider attempt; an ambiguous failed attempt then returns to the logical-prompt scheduler.

ChatGPT GPT-5.6 Sol, Terra, and Luna publish a 353,400-token effective context
window and include `max` among their published reasoning choices. They use
standard Responses and parallel direct tool calls by default. Legacy Responses
Lite is available only by setting `responses_lite_compatibility: true` on that
ChatGPT profile (or answering Yes during `tau provider add`) and restarting.
Tau never changes modes as a retry fallback. Both modes omit legacy inline
context management; manual and automatic compaction use the unary
`/codex/responses/compact` operation and install one validated
replacement-window transcript boundary. The startup mode also separates prompt
cache/thread/socket identity, causing one cold transition after upgrade, while
quota and retry identity remain account/provider based.

The ChatGPT/Codex surface also uses a persistent WebSocket connection pool keyed by account, startup-selected Responses mode, and agent so upstream connection-local caches stay warm across turns, including interleaved sub-agent delegations. Prompt-cache keys use the same mode-aware identity and do not split based on whether a turn came from the user, an extension, a manager relay, or an agent-to-agent message. Refreshed OAuth tokens invalidate stale sockets on next use. ChatGPT/Codex inference is WebSocket-only: retryable WS failures return to the in-memory logical-prompt scheduler, while proven terminal WS errors surface without an HTTP fallback.

Fresh WebSocket setup first emits a fixed secret-free connecting status, then races
DNS/TCP/TLS/upgrade against the prompt cancellation registry and a 30-second
deadline. Timeout is retryable transport work; all unsuccessful upgrades release
their same-key reservation.

Best-effort ChatGPT cache prewarm runs on a capped supervised worker, not the
provider event loop. Matching real work, cancellation, shutdown, or profile
rotation wakes it. Its connection and non-generating response phases are each
bounded to 30 seconds, and stale canceled/invalidated sockets cannot return to
the pool.

After setup, ChatGPT/Codex inference is WebSocket-only with a separate default
five-minute idle watchdog. The timer resets on each provider frame and is not an
absolute turn-duration cap. If upstream stalls, Tau aborts that finite attempt,
clears tentative output, and parks the logical prompt for another attempt.

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
### Standalone compaction tools

`compact` and `agent_compact` require a live provider route whose selected
model advertises standalone compaction. Unsupported or inline-only models are
rejected without a durable accepted request. Provider failures terminate the
background tool exactly once.
The static `/retry` command can release the selected agent's exact parked
logical prompt immediately. This one-job override preserves retry accounting,
does not initially wake peer jobs, and still waits for a bounded provider worker
slot. If that exact probe commits a successful terminal response, Tau clears the
matching current provider cooldown generation and wakes same-profile peers with
stable anti-herd jitter. Errors, cancellation, stale successes, and best-effort
quota display updates do not clear inference cooldowns.
