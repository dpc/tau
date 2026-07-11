# Providers

A provider is a normal Tau extension that exposes models and executes prompts.
The harness does not own provider-specific LLM execution; provider extensions are the model executors.

## Core meaning

- **provider**: a configured runtime instance that can expose and execute one or more models
- **model**: a selectable model exposed by a provider
- **role**: a harness-owned named default that points at a model plus optional model parameters

## Core responsibilities

Provider extensions own provider-specific work:

- auth and runtime state
- model availability snapshots
- request execution
- response streaming
- provider protocol details

The harness owns orchestration:

- sessions and prompt assembly
- role selection and resolving the selected role to a provider model
- mapping `ModelId` to the provider extension that published it
- direct prompt routing
- Tau tool routing and the tool-call follow-up loop
- harness/UI state such as selected role, resolved model, and available roles

The UI should stay dumb: it consumes harness/provider events and asks the harness to change role state.

## Model publication and routing

One extension may publish multiple models.
One model carries provider identity in its `ModelId`.

```rust
extension -> models
```

Example:

```rust
ModelId::new("chatgpt", "gpt-5.6-sol")
ModelId::new("chatgpt", "gpt-5.3-codex")
```

The provider extension publishes `provider.models_updated` with the models it can currently serve.
This snapshot carries model metadata, not just IDs:

```rust
struct ProviderModelInfo {
    id: ModelId,
    display_name: Option<String>,
    context_window: u64,
    efforts: Vec<Effort>,
    verbosities: Vec<Verbosity>,
    thinking_summaries: Vec<ThinkingSummary>,
}
```

`context_window` is required for every published model.
Publishing a model means it is available; no separate `enabled` flag is needed initially.

The harness records which extension sent the snapshot and uses that as routing state.
It also re-emits current provider snapshots to provider-event subscribers and translates the metadata into harness model/role/selection state for the UI: context window, effort choices, verbosity choices, thinking-summary choices, and role descriptions.

Prompt execution for provider-published models is directed to the extension that owns the selected `ModelId`; it is not broadcast to every provider or agent.
This mirrors Tau's tool routing model.

## Execution events

Provider execution should use provider-named events, not `agent.*` events:

- `provider.prompt_submitted`
- `provider.response_updated`
- `provider.response_finished`

These should keep the semantics of the current agent execution events as much as possible:

- submitted = the provider accepted the prompt and started work
- updated = transient append deltas for newly generated displayable assistant
  text and reasoning text, plus small compaction/status metadata when relevant
- finished = final response, tool calls, usage, stop reason, backend metadata

Providers must not repeat the full accumulated assistant/reasoning text in
intermediate updates. `provider.response_finished.output_items` remains the
complete durable response and is where ordered final provider items, including
tool calls and opaque provider items, are committed. Provider-authored retry or
diagnostic text must be sent as update `status`, not as assistant message
deltas.

Providers must not write `provider.response_updated` directly from every upstream stream chunk. Providers may emit the first non-empty streamed response/progress sample promptly so UIs learn that output has started. Later non-terminal response/progress updates are batched and emitted at most once per second per prompt; later byte changes are accumulated, not a reason to emit early. A terminal flush is allowed immediately before `provider.response_finished` closes the prompt.

Providers attach public content-free `response_stats` previous/current samples to these rate-limited updates. Providers own prompt-local response byte counting because they read the upstream stream, and first-party providers advance that counter from lower-layer received backend response bytes before semantic parsing so progress does not wait for a complete response item. `previous` is the last provider response sample that was actually emitted for that prompt, while `current` is the new cumulative sample measured since backend request dispatch.

The harness validates provider prompt ownership and fixes routing identity, then broadcasts `provider.response_updated` unchanged. It must not strip `response_stats`, derive its own response byte counters, or publish a harness-owned response-throughput projection. UI clients render live response throughput directly from provider updates. Stats-only provider updates are valid when no displayable text, status, or compaction changed.

First-party providers abort high-confidence tight stream loops with
`stop_reason: repetition_detected`: assistant/reasoning/tool-argument deltas are
checked per output item with bounded exact-match suffix detectors. On abort the
provider sends a `provider.response_updated` status with `clear_response: true`,
then a final response with empty `output_items` and a bounded display `error`.

Provider final responses may contain tool calls, but providers do not execute Tau tools.
The harness routes tools and sends follow-up prompts back to the selected provider when needed.
Providers that receive function-call arguments from upstream as JSON text must
store both forms in finished output items: parsed CBOR in
`ToolCallItem.arguments` for validation/tool dispatch, and the original JSON
string in `ToolCallItem.raw_arguments_json` for provider replay/cache identity.
Replay should prefer the raw sidecar when present and serialize parsed CBOR only
for old persisted records or calls that never had provider-wire JSON.

Chat Completions transcript replay is semantic rather than a byte-for-byte
provider-message round trip. It preserves the `messages[]` content Tau needs to
continue the conversation — roles, visible text, reasoning text when exposed,
tool calls, tool results, and raw function-call argument strings — but it does
not preserve arbitrary provider-specific assistant-message fields. Add opaque
Chat Completions sidecars only for concrete provider-required replay/cache or
correctness needs.

Responses providers should likewise preserve raw assistant `message` output
items in `MessageItem.responses_raw_json` when available. The typed role,
content text, and phase remain semantic truth; the raw sidecar is a replay/cache
fidelity aid for provider-owned ids, statuses, annotations, content-part
boundaries, and unknown fields.

## Roles

Roles are harness-owned.
A role points at a model and may include model parameters.

```rust
Role {
    name: "smart".into(),
    model: ModelId::new("chatgpt", "gpt-5.3-codex"),
}
```

The harness owns role resolution and first-model selection.
The UI displays and edits resolved harness state; it should not do provider resolution itself.

## State

Provider-specific config and runtime state should live with the provider extension / provider storage.
There should be no global model-registry config file that describes every provider runtime.

A provider owns its own:

- auth state
- cached tokens
- endpoint/runtime settings, if any are needed later
- transport caches or pools
- internal metadata

For the built-in ChatGPT/Codex Responses provider, auth presence is enough to enable the provider namespace:

- `chatgpt/*` is available when ChatGPT OAuth state exists

No separate enable flag is needed for registered profiles.

## Built-in first-party provider

The built-in provider extension currently covers three profile kinds:

- `chatgpt/*` for the ChatGPT / Codex Responses backend
- user-named OpenAI-compatible Chat Completions profiles with explicit model lists
- user-named OpenRouter profiles with explicit or fetched model lists

It lives in `crates/tau-ext-provider-builtin` and is spawned as the built-in `provider-builtin` extension.
It publishes hardcoded ChatGPT/Codex metadata and configured Chat Completions/OpenRouter model metadata before `Ready` during extension startup.
It owns execution for those namespaces and preserves the existing provider execution event semantics for streaming, tool calls, usage, and retries.

Required LLM work has no attempt-count or elapsed-time retry limit during the
running session. Transport/server failures, throttling, usage windows,
billing/quota/credits, reloadable auth/configuration, and unknown remote
failures remain pending until success or cancellation. Only a narrowly proven
deterministic unchanged-request failure closes immediately.

Retry delays do not occupy one of the bounded provider workers. One in-memory
scheduler parks logical prompts, applies jittered class-specific Fibonacci
cadence (up to about thirty minutes for persistent failures), honors later
trusted reset/`Retry-After` hints, and shares cooldowns by configured provider
profile. Retry status is visible and says how to cancel. Profiles and
credentials are resolved again when delayed work becomes due. This state lasts
only for the process/session lifetime; Tau deliberately does not replay
ambiguous in-flight requests after a cold restart.
It publishes `chatgpt/*` only from auth named `chatgpt`; there is no `openai-codex` compatibility alias.
WebSocket-capable ChatGPT/Codex Responses models remain on WebSocket: retryable
WS failures return to the shared logical-prompt scheduler, and terminal WS errors are
surfaced instead of silently falling back to HTTP/SSE.
The ChatGPT GPT-5.6 Sol, Terra, and Luna models publish a 353,400-token
effective context window and include `max` among their reasoning choices. Normal
inference stays on Responses Lite and never emits legacy inline context
management. Manual and threshold-driven compaction use the separate unary
`/codex/responses/compact` operation, with a provider default threshold of
334,800 tokens; accepted output becomes one standalone transcript boundary.
ChatGPT/Codex live streams use a five-minute idle watchdog on both HTTP/SSE and
WebSocket transports. The watchdog resets on each SSE `data:` event or
WebSocket provider frame, not on SSE comments/heartbeats or partial-line byte
trickles, and is not an absolute turn-duration cap. If upstream goes quiet, Tau
aborts the attempt and schedules the still-required logical prompt with transport, prompt id,
elapsed/idle timing, configured idle timeout, whether partial output had already
arrived, and read-source details where available.

## Summary

- providers are normal Tau extensions
- provider extensions publish models and execute prompts
- the harness routes prompts directly to the selected role's resolved model owner
- execution events should be `provider.*`, not `agent.*`
- the harness owns roles, selection, sessions, and tool routing
- provider state belongs to providers
- the UI should not resolve providers itself
