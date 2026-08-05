# Providers

Canonical context-window rejection is reported as a typed terminal provider
failure. The harness, not the adapter, decides whether an ordinary no-output
inference may receive one standalone-compaction recovery; provider-authored
recovery-disposition fields are ignored.

A provider is a normal Tau extension that exposes models and executes prompts.
The harness does not own provider-specific LLM execution; provider extensions are the model executors.

## Prompt-cache prefix stability

Tau keeps each backend's provider-visible request meaning stable whenever only
local prompt correlation changes or a newest conversation turn is appended.
System/developer authority, ordered history, full tool definitions and schemas,
and supported reasoning/thinking settings remain unchanged; new context follows
the preceding history. When an existing backend accepts `tool_choice: "none"`,
Tau retains the full tool-definition list and uses that selector rather than
removing definitions solely to disable calls for one turn. This preserves tool
visibility and authorization while presenting stable request structure to
automatic provider caches.

This improves cache eligibility only; each provider controls matching,
breakpoints, residency, and hits. Model identity, system/developer
instructions, tool order or schema, reasoning/thinking configuration, image
details, and provider-specific extra request fields remain provider-visible and
may affect matching when they change. Generic backends replay their complete
semantic transcript. Only private Responses stale-anchor repair can replace
suffix chaining with full replay, and it does so only after its exact upstream
anchor proof fails. Tau does not add cache keepalive requests, scheduling, or
cache persistence to obtain a hit.

## Proxy and certificate policy

Built-in provider networking snapshots `http_proxy`/`HTTP_PROXY`,
`https_proxy`/`HTTPS_PROXY`, `all_proxy`/`ALL_PROXY`, and
`no_proxy`/`NO_PROXY` when the provider process starts. Lowercase variables
take precedence. HTTP and WebSocket targets use the HTTP proxy class; HTTPS
and secure WebSocket targets use the HTTPS class, with `ALL_PROXY` as fallback.
Restart Tau after changing these values.

`NO_PROXY` is the only direct-route bypass. Once a proxy is selected, proxy
DNS, connection, TLS, authentication, tunneling, timeout, or upgrade failure
does not fall back to a direct connection. HTTP and HTTPS proxy URLs with
percent-encoded Basic credentials are supported. SOCKS, PAC/WPAD, desktop proxy
discovery, integrated authentication, and redirects are unsupported.

The release acceptance suite proves HTTP through HTTP and HTTPS proxies, HTTPS
through HTTP and HTTPS proxies, WS through an HTTP proxy, and WSS through HTTP
and HTTPS proxies. Secure targets through HTTPS proxies cover proxy TLS,
authenticated `CONNECT`, target TLS, and the target request or WebSocket upgrade
as distinct wire layers.

For HTTPS/WSS, reqwest owns the proxy `CONNECT` exchange. Its public API does not
expose a tunnel rejection's status. Tau therefore reports a hidden CONNECT 407
as a redacted proxy-route transport failure, not as proven proxy authentication;
this can use the shorter transport retry cadence. Plain HTTP/WS proxy 407
responses remain specifically classified. Tau never guesses from dependency
error text, and the no-direct-fallback guarantee is unchanged.

TLS always uses the operating system verifier. `TAU_PROVIDER_CA_BUNDLE` may
name a bounded certificate-only PEM bundle that adds corporate trust without
replacing platform trust. The bundle is captured at startup. Plain HTTP or WS
is visible to the selected proxy; use HTTPS/WSS when the proxy must not observe
provider credentials or content.

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

The provider extension publishes transient `provider.models_declared` replacement
declarations with the models it can currently serve. After ordinary interception
and commit, the harness publishes protected canonical `provider.models_updated`
current state and updates routing/availability projections. The declaration
payload contains the proposed model list. The canonical payload adds
`publisher_extension_id`, the stable configured provider whose complete current
state it replaces; an empty list withdraws that provider's models. Replay exposes
one canonical snapshot per active provider, including empty snapshots. Model lists
carry metadata, not just IDs:

```rust
struct ProviderModelInfo {
    id: ModelId,
    display_name: Option<String>,
    tags: Vec<ModelTag>,
    supported_tool_types: Vec<ToolType>,
    input_modalities: Vec<InputModality>,
    tool_result_modalities: Vec<InputModality>,
    supports_parallel_tool_calls: bool,
    default_affinity: i32,
    context_window: u64,
    efforts: Vec<Effort>,
    verbosities: Vec<Verbosity>,
    thinking_summaries: Vec<ThinkingSummary>,
    est_uncached_input_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    est_cached_input_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    est_cache_write_input_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    est_output_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    est_cache_storage_cost_1m_token_hour_usd: Option<EstimatedUsdPerMillionTokenHours>,
}
```

`context_window` is required for every published model.
`input_modalities` declares what the exact provider/model route accepts as
prompt input, while `tool_result_modalities` declares what it accepts inside
native tool-result output. A tool that returns images is exposed only when both
lists contain `image`; omitted lists preserve legacy text-only behavior.
`supports_parallel_tool_calls` is the effective route capability used to make
system-prompt guidance truthful; it is not merely abstract model metadata.
Publishing a model means it is available; no separate `enabled` flag is needed initially.

The optional `est_*_cost_1m_usd` fields publish USD prices for ordinary input,
cached reads, cache writes, output, and cache storage per million token-hours.
Decimal strings preserve fixed-point values on the provider wire;
the built-in Chat Completions profile parser also accepts non-negative integer
JSON numbers. Fractional configured prices must use decimal strings with at most
six fractional digits so validation never rounds through binary floating point.
Missing fields resolve built-in default prices for known compatible model ids
(documented below for the built-in Chat Completions provider) and otherwise use
the central GPT-5.6-equivalent fallback: `$5`
uncached input, `$.50` cached input, and `$30` output per million tokens. This
fallback intentionally applies to local and free models too.

The harness applies the serving model's prices to each accepted usage record and
accumulates a runtime-only self estimate per loaded agent and an inclusive
creator-subtree estimate through authenticated same-session
`AgentStarted.creator` agent edges. Metadata `parent_agent` never creates cost
membership; completed descendants remain included until session rollover. If a
provider reports total input without cached-token detail, Tau treats all input
as ordinary input. Explicit cache observations clamp to total input in read,
write, then miss order. A missing cache-write price uses the ordinary-input
price; storage contributes only when both token-time usage and a storage price
are present. The status chip renders the independently compact-formatted pair
`$self/$subtree`; it is an **estimated equivalent API cost**, not a bill. It
accounts for reported cache writes and token-time storage but still ignores
long-context and other tiers, batch or service discounts, regional and negotiated
pricing, subscriptions, and private-route accounting. It resets with the active
session/runtime and is not reconstructed from durable history.
The display rounds aggressively to fit `$` plus three characters (`$.03`, `$2.1`,
`$23`, `$12k`).

Hardcoded ChatGPT/Codex model prices come from OpenAI's provider-owned
[API pricing table](https://developers.openai.com/api/docs/pricing). Configured
compatible providers own their explicit values; refresh those profile fields from
that provider's basic public pricing table. The built-in Chat Completions
provider ships default prices for known compatible model ids without explicit
profile fields: `deepseek-v4-flash` uses DeepSeek's
[standard API prices](https://api-docs.deepseek.com/quick_start/pricing)
(`$0.14` uncached input, `$0.0028` cached input, `$0.28` output per million
tokens).

The harness records which extension sent the snapshot and uses that as routing state.
If multiple snapshots advertise the same provider-qualified `ModelId`, the
harness rebuilds the registry in lexicographically sorted extension-source
order; the last advertisement wins both metadata and routing. A bounded ordinary
warning reports the collision without changing that deterministic behavior.
It also re-emits current provider snapshots to provider-event subscribers and translates the metadata into harness model/role/selection state for the UI: context window, effort choices, verbosity choices, thinking-summary choices, and role descriptions.

Prompt execution for provider-published models is directed to the extension that owns the selected `ModelId`; it is not broadcast to every provider or agent.
This mirrors Tau's tool routing model.

## Execution events

Provider execution should use provider-named events, not `agent.*` events:

- `provider.prompt_submitted_reported`
- `provider.response_updated_reported`
- `provider.response_finished_reported`

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

Providers must not write one `provider.response_updated_reported` event for every
upstream stream chunk. They may emit the first non-empty streamed response/progress
sample promptly so UIs learn that output has started. Later non-terminal reports
are batched and emitted at most once per second per prompt; later byte changes are
accumulated, not a reason to emit early. A terminal flush is allowed immediately
before `provider.response_finished_reported`; only a correlated report accepted
by the harness terminal pipeline closes the prompt.

Providers attach public content-free `response_stats` previous/current samples to these rate-limited updates. Providers own prompt-local response byte counting because they read the upstream stream, and first-party providers advance that counter from lower-layer received backend response bytes before semantic parsing so progress does not wait for a complete response item. `previous` is the last provider response sample that was actually emitted for that prompt, while `current` is the new cumulative sample measured since backend request dispatch.

Providers may set `first_semantic_output_elapsed_micros` to the finite-attempt
duration from first backend send/enqueue until their first synchronously accepted
semantic output. Follow the exact qualifying and excluded categories in
[SPEC-provider-response-streaming](../specs/SPEC-provider-response-streaming.md);
in particular, function arguments and custom-tool input must be non-empty, and
opaque reasoning qualifies only when a material completed item is accepted.
Capture the value before update batching and repeat it unchanged on later
samples. Omit it when unsupported or not observed. The field is live-only and
must not be copied to finished output or replay state.

The harness first commits the report, then validates provider prompt ownership, fixes
routing identity, and publishes canonical `provider.response_updated`. It must not
strip `response_stats`, derive its own response byte counters, or publish a separate
response-throughput projection. UI clients render live response throughput directly
from canonical provider updates. Stats-only updates are valid when no displayable text,
status, or compaction changed. The complete authority and terminal contract is
[SPEC-provider-execution-reports-and-canonical-facts](../specs/SPEC-provider-execution-reports-and-canonical-facts.md).

First-party providers abort high-confidence tight stream loops with
`stop_reason: repetition_detected`: assistant/reasoning/tool-argument deltas are
checked per output item with bounded exact-match suffix detectors. On abort the
provider sends a `provider.response_updated_reported` status with
`clear_response: true`, then a `provider.response_finished_reported` response with
empty `output_items` and a bounded display `error`.

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

Transcript replay proves persisted reconstruction, not backend transport
execution. ChatGPT/Codex request lowering, WebSocket frame parsing, pooling,
reconnect, cancellation, deadlines, and fallback policy are covered separately
by focused provider-local transport tests; curated VCR replay remains bounded
request/parser compatibility evidence.

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

The built-in provider extension currently covers four profile kinds:

- `chatgpt` for the ChatGPT / Codex Responses backend
- `chat_completions` for user-named OpenAI-compatible profiles with explicit
  model lists
- `openrouter` for user-named profiles with explicit or fetched model lists
- `responses` for user-named public Responses profiles with explicit model lists

These are deliberately three backend contracts, not one generic OpenAI client.
`tau-provider-chat-completions` implements the OpenAI-compatible
`POST /chat/completions` HTTP/SSE surface used by local servers such as llama.cpp,
remote compatible endpoints, and OpenRouter. It supports optional bearer auth,
Function tools, streamed tool-call arguments, semantic transcript replay,
reasoning/usage compatibility controls, and non-conflicting `extra_body` fields.
The extension owns its serialized profiles, model publication, OpenRouter
discovery, public stream sampling, retry scheduling, and provider events; the
backend performs one finite typed attempt.

Chat Completions profiles parse provider-specific cache counters only when
`compat.cache_usage` explicitly selects `open_ai` or `deep_seek`; omission
ignores cache-only fields while retaining ordinary input/output usage. Public
Responses and private ChatGPT routes use their native OpenAI response shape.
Anthropic/Gemini compatibility routes remain best-effort and expose no native
cache parsing or object lifecycle.

### Scoped provider credentials

`tau provider add` writes one registration under the selected enabled built-in
provider extension instance:

```text
provider-settings/<extension>/<provider>.json
secrets/ext/<extension>/providers/<provider>/{oauth,api-key}.json
```

Settings contain backend and model metadata plus a deterministic credential
reference, never OAuth tokens or API keys. The runtime loads the typed
version-zero credential before model publication and at prompt boundaries.
Credential rotation therefore takes effect without restart; settings changes
require restart. Missing or malformed credentials exclude that provider.
Initial Configure validates the complete bounded settings snapshot before
retaining it or publishing any model. One invalid filename or profile—including
legacy `api_key_secret`, inline API keys, or mixed credential fields—rejects the
whole snapshot, publishes no models or Ready, and produces one mandatory,
replayable, redacted configuration warning. Re-register the invalid profile;
startup does not rewrite or migrate persisted settings.

`tau provider add [KIND]` accepts exactly `chatgpt`, `chat-completions`,
`responses`, or `openrouter`; without `KIND` it presents those same choices in
a picker. API-key profiles explicitly select direct masked entry, a configured
named secret, or (only for keyless/local-compatible backends) no key. A named
source is recorded without its value, materialized into the canonical Secret
record by setup and again on harness restart, and never read by provider runtime
except through Secret RPC. If the named source is unavailable, setup fails before
activating settings; a later restart invalidates its old materialization, omits
the profile, and publishes a source-name-only warning. A bound declaration is
consumed for materialization and is not copied into `Configure.secrets`.
Provider setup/removal and startup serialize per configured instance; the exact
startup settings snapshot that selects the source also becomes the immutable
Configure snapshot.

Generic Chat Completions models do not support standalone compaction. A local
model can opt in to Tau summary compaction by declaring its context window plus
an explicit `local_summary_compaction` object:

```json
{
  "id": "local-model",
  "context_window": 32768,
  "local_summary_compaction": {
    "serialization_profile": "local_transcript_v1",
    "context_window_tokens": 32768,
    "max_input_bytes": 16384,
    "max_output_tokens": 1024,
    "max_output_bytes": 8192
  }
}
```

Tau then sends one dedicated no-tools Chat Completions request to that exact
model. It commits only a validated summary as untrusted synthetic historical
context; it never adds model text to the system prompt. Public Responses and
models without this declaration remain unsupported.
The context window must match the model field. Input and output limits must be
positive and fit conservatively within that window; units are bytes, tokens,
and bytes respectively, with an additional 1,024-token worst-case request and
chat-template reserve. Known-remote OpenRouter profiles discard this local-only
declaration. Transcript-v1 deliberately removes image bytes while
retaining image metadata and a loss marker. Empty, malformed, truncated, or
over-limit summaries fail the durable transaction without fallback or resend.

`tau-provider-responses` implements the public `/responses` protocol over
API-key HTTP/SSE or WebSocket for the `responses` profile. The
`tau provider add` picker labels these kinds `OpenAI-compatible Chat
Completions` and `OpenAI Responses API`.
Responses profiles require a base URL, explicit models, and a `transport` value
spelled `sse` or `websocket`; omitted values from older profiles mean `sse`.
For API-key profiles, the wizard first selects `Enter API key now`, `Use
configured named secret` when declarations exist, or `No API key` where
keyless operation is supported. Only direct entry opens the masked value
prompt. It asks for transport after the endpoint, API-key authority, and models. It
preselects WebSocket only for the exact official
`https://api.openai.com/v1` base URL and otherwise preselects SSE. Tau does not
infer endpoint support at runtime or discover models. Every turn sends the complete typed
Responses transcript. It supports assistant text, plain `reasoning_text`
reasoning, and Function tools. Plain reasoning uses the existing
`show-thinking` UI behavior and is retained for full-transcript replay.
Encrypted, summary-only, malformed, and mixed reasoning remains unsupported.
The backend preserves assistant-message, reasoning-item, and Function-call
replay sidecars. It deliberately omits `previous_response_id`, `store`,
hosted/custom tools, image/file inputs, and public compaction. Existing
`openrouter` profiles remain Chat Completions profiles.

WebSocket mode opens a fresh connection for each finite attempt and sends one
`response.create` envelope without SSE-only fields. A retry reconnects and
replays the complete local transcript. It never continues from a response ID
whose connection-local cache may have disappeared, never silently switches to
SSE, and never targets the distinct OpenAI Realtime protocol.

Each `responses.models[]` entry may set `efforts` to describe the exact
reasoning-effort levels its upstream model accepts:

```json
{
  "id": "quirky-model",
  "efforts": ["off", "low", "medium", "high"]
}
```

Omitting `efforts` publishes the full canonical set
`[off, minimal, low, medium, high, xhigh, max]`, including for existing
profiles. An explicit `efforts: []` publishes no reasoning-effort capability.
Non-empty overrides are sets: Tau rejects duplicates and publishes the selected
levels in that canonical order, regardless of their order in JSON or YAML.
The harness clamps each request to the published set; the public Responses
backend always sends the resulting `reasoning: { effort: ... }`, spelling Tau's
`off` as API `none`. `tau provider add` intentionally omits this field, so new
profiles receive the full default; edit the profile only when a model needs an
override.

`tau-provider-codex` implements the private ChatGPT OAuth/Codex Responses
contract. Ordinary inference is WebSocket-only: it has no HTTP/SSE selector or
fallback. HTTPS remains in that backend for OAuth, `/wham/usage`, and unary
`/codex/responses/compact`. It supports Standard Responses by default and
explicit Lite compatibility, Function and Custom tools, response-id chaining,
prompt caching, pool/prewarm reuse, opaque replay items, and provider-owned
reasoning state. It is not a public API-key OpenAI Responses client.

GPT-5.6 ChatGPT profiles use standard Responses by default. The legacy Lite
contract is available only as an explicit profile compatibility setting:

```json
{
  "kind": "chatgpt",
  "auth": {
    "access_token": "<existing access token>",
    "refresh_token": "<existing refresh token>"
  },
  "responses_lite_compatibility": true
}
```

Add the top-level flag to an existing ChatGPT profile without changing its
current `auth` fields. `tau provider add` also asks for this setting and defaults
to No. The selected mode
is captured at startup, so edits require a Tau restart. OAuth refresh preserves
the setting. Tau never changes modes during retry, reconnect, replay, chaining,
or compaction and never falls back to Lite after a standard-route rejection.
Existing profiles without the field use standard mode. Upgrading intentionally
causes one prompt-cache/WebSocket cold start for both modes so prior Lite and
current standard threads cannot collide; quota and retry identity remain shared
by account/provider.

It lives in `crates/tau-ext-provider-builtin` and is spawned as the built-in `provider-builtin` extension.
It publishes hardcoded ChatGPT/Codex metadata and configured Chat Completions/OpenRouter model metadata before `Ready` during extension startup.
It owns execution for those namespaces and preserves the existing provider execution event semantics for streaming, tool calls, usage, and retries.

ChatGPT profiles fetch the bounded full account quota snapshot from `/wham/usage`
and merge sparse in-band WebSocket `codex.rate_limits` observations. Quota telemetry is best-effort
and never delays inference or consumes prompt retry budget. The compact status
chip is shown for a selected model when its provider publishes quota current
state. Tau uses neutral `Q?` when weekly state is absent, unbound, stale, expired,
or timing-untrusted. It never guesses a colored claim from a default or sole
account pool: colored state requires a fresh in-band binding of the exact
`ModelId` and trustworthy weekly timing, and expires rather than locally resetting
when the server reset boundary passes. After quota state is cleared, an empty
capability snapshot remains replayable for the running harness so both live and
late clients show neutral unknown.

Required LLM work has no attempt-count or elapsed-time retry limit during the
running session. Transport/server failures, throttling, usage windows,
billing/quota/credits, reloadable auth/configuration, and unknown remote
failures remain pending until success or cancellation. Only a narrowly proven
deterministic unchanged-request failure closes immediately.

Retry delays do not occupy one of the bounded provider workers. One in-memory
scheduler parks logical prompts, applies jittered class-specific Fibonacci
cadence (up to about thirty minutes for persistent failures), honors later
trusted reset/`Retry-After` hints for other failure classes, and shares cooldowns
by configured provider profile. Usage-window reset estimates remain informational
because the user or provider may restore access early; Tau continues probing on
the bounded persistent-failure cadence. Retry status is visible and says how to
cancel. Profiles and
credentials are resolved again when delayed work becomes due. This state lasts
only for the process/session lifetime; Tau deliberately does not replay
ambiguous in-flight requests after a cold restart. Permanent OAuth rejection
suppression is likewise process-local. Within one provider process, an
unchanged credential generation is sent once after a permanent rejection;
credential/profile change permits a new attempt, and restart may probe once
again. A failed preemptive refresh can use only an access token that remains
valid; an expired bearer is never used. The logical prompt keeps its existing
slow authentication retry cadence.
`:retry` initially bypasses a shared cooldown for only the selected prompt. A
successful terminal response from that probe invalidates the exact cooldown it
tested and wakes same-profile peers with stable anti-herd jitter. Error,
cancellation, stale probes, and best-effort quota display updates do not clear
inference cooldowns.
It publishes `chatgpt/*` only from auth named `chatgpt`; there is no `openai-codex` compatibility alias.
WebSocket-capable ChatGPT/Codex Responses models remain on WebSocket: retryable
WS failures return to the shared logical-prompt scheduler, and terminal WS errors are
surfaced instead of silently falling back to HTTP/SSE.
Fresh setup emits a fixed content-free connecting status, and the
DNS/TCP/TLS/WebSocket upgrade is cancellation-aware and bounded to 30 seconds.
Timeout is classified as retryable transport work; failure or cancellation
releases the same-key pool reservation.
Best-effort prefix prewarm is capped and supervised outside the provider event
loop. Matching prompt work, cancellation, shutdown, and mutable-profile rotation
wake it. The upgrade and prewarm response each have a 30-second bound, and a
canceled or invalidated worker cannot reinstall its socket.
Only the same socket and exact profile/mode/cache identity may use a successful
prewarm response id. The real request must retain the warmed lowered input as an
exact prefix; a changed fingerprint, divergent prefix, stale generation, or
invalidation discards the anchor and sends full context.
The ChatGPT GPT-5.6 Sol, Terra, and Luna models publish a 353,400-token
effective context window and include `max` among their reasoning choices.
Standard mode publishes and requests parallel direct tool calls; Lite
compatibility publishes its one-call limit. Neither mode emits legacy inline
context management. Manual and threshold-driven compaction use the separate
unary `/codex/responses/compact` operation with the selected mode's request
shape and a provider default threshold of 334,800 tokens; accepted output
becomes one standalone transcript boundary.
After setup, ChatGPT/Codex inference uses WebSocket exclusively with a separate
five-minute idle watchdog. The watchdog resets on each provider frame and is not
an absolute turn-duration cap. If upstream goes quiet, Tau
aborts the attempt and schedules the still-required logical prompt with transport, prompt id,
elapsed/idle timing, configured idle timeout, whether partial output had already
arrived, and read-source details where available.

One finite Codex attempt may spend one immediate WS repair for an exact
stale-chain/connection-limit failure or dead socket, but only before semantic
model output. The first request-send time is reported once and received bytes
remain cumulative across that repair. After assistant, reasoning, tool, or opaque
output begins, Tau never silently replays and splices the turn: it clears
tentative output and returns the typed result to the extension-owned logical
retry policy. Canonical provider status/codes, not provider prose, authorize
repair and retry classification.

## Summary

- providers are normal Tau extensions
- provider extensions publish models and execute prompts
- the harness routes prompts directly to the selected role's resolved model owner
- execution events should be `provider.*`, not `agent.*`
- the harness owns roles, selection, sessions, and tool routing
- provider state belongs to providers
- the UI should not resolve providers itself

### Watcher-visible provider work

Provider retries carry closed structured categories, saturating attempt counts, and approximate bounded delays independently of human UI prose. After validating prompt ownership, the harness owns the current per-agent/turn/prompt snapshot and session-local watcher fanout. Live delivery is limited to first category, category/phase changes, and terminal failure; same-category storms only refresh the late-watch snapshot. Enabling or re-enabling returns current sanitized state and emits an initial client snapshot without prompting the model. Durable live facts replay as transcript context without re-fanout; disable, prune, and session change stop delivery. Raw provider bodies, status text, errors, headers, account data, secrets, and prompt content never cross this boundary.
### Model-callable standalone compaction

The enabled-by-default `compact` tool and disabled-by-default `agent_compact`
tool require the exact selected model and live route to advertise standalone
compaction. They never fall back to legacy inline compaction. Self `compact`
internally uses an asynchronous transaction, but suspends ordinary inference
until it directly delivers and consumes one correlated terminal; that call is
not subsequently waitable. Cross-agent `agent_compact` remains asynchronous and
its original call receives a `tool.background_result` or
`tool.background_error` consumable through `wait`.
## Manual delayed retries

Use `:retry` to run the selected agent's currently delayed provider retry now.
The command applies only while that exact logical prompt is parked in the
provider retry scheduler; it does not resend completed work or start a second
prompt. It overrides the selected job's remaining delay once, including a
server-requested delay, while retaining normal worker concurrency limits and
initially leaving other delayed jobs untouched. A validated successful terminal
from that exact attempt clears its matching current shared cooldown and wakes
only peers constrained by that cooldown generation with anti-herd jitter.
