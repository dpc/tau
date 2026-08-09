---
name: tau-self-knowledge-ext-provider-builtin
description: Use this extension skill when the user asks about Tau's provider-builtin extension, built-in model providers, ChatGPT/Codex OAuth, OpenAI-compatible Chat Completions, OpenRouter, providers, model publication, or tau provider commands.
advertise: false
---

# Tau provider-builtin extension self-knowledge

`provider-builtin` is Tau's built-in provider extension. It runs `tau-ext-provider-builtin`, is enabled by default, publishes available models from configured providers, and executes agent turns for built-in provider backends.

## Provider retries

Required ChatGPT/Codex Responses, Chat Completions, and OpenRouter work keeps
retrying during the running session until success or explicit cancellation,
unless Tau can positively prove the unchanged request is deterministic and
invalid. Unknown provider failures retry conservatively; billing, quota,
credits, usage windows, and reloadable auth/configuration retry slowly.

Delayed work is parked in one in-memory scheduler and does not consume a
provider worker. Tau shows a retry status with the reason, next delay, and
cancellation instruction. Same-profile limits share cooldown state, trusted
server timing hints remain lower bounds except for usage-window reset estimates,
and mutable profiles are reloaded before later attempts. Usage-window estimates
remain informational because the user or provider may restore access early; Tau
continues bounded periodic probes. Retry state does not survive a cold process
restart.

Within one provider process, a permanently rejected ChatGPT OAuth refresh is
suppressed for the exact unchanged credential/profile generation across startup
quota, prewarm, prompt, retry, and scheduled quota resolution. Credential or
profile change permits a new attempt; cold restart may probe once again. A
failed preemptive refresh may fall back only to the authoritative locked access
token while it remains valid, never to an expired or stale pre-lock bearer. This
does not change the logical prompt's slow authentication retry cadence.

## Provider profiles and CLI

Provider registrations pair credential-free JSON settings with a typed credential
record in the configured extension instance's private state. Manage them with:

```sh
tau provider add
tau provider list
tau provider show <name>
tau provider remove <name>
```

Use `--extension <instance>` when more than one enabled built-in provider
instance exists. Setup writes the credential first and settings last; removal
deletes settings first. Restart Tau after settings changes. Credential rotation
is observed at the next prompt boundary without restart.

Add defaults to mutable XDG state. `--config` targets portable XDG config, and
`--config --output -` emits canonical credential-free JSON for dotfiles while
publishing credentials only into host-local Secret state. Config and state names
must be disjoint; list/show report source identity and remove can infer a unique
source or accept `--config`/`--state`. The retired state `provider-settings/`
directory is never discovered; manually move only its JSON and leave Secret
records untouched.

`tau provider add [KIND]` accepts `chatgpt`, `chat-completions`, `responses`,
or `openrouter`; no kind opens a picker. API-key setup explicitly selects masked
direct entry, a configured named secret, or keyless mode where supported.
Named-secret values are materialized only into the instance's canonical Secret
record. Setup refuses an unavailable selected source before writing settings;
restart re-imports the current source and disables the profile with a redacted,
source-name-only notice if it has become unavailable. Bound declarations do not
enter `Configure.secrets`; one per-instance lifecycle lock binds startup source
selection, typed-secret publication, and the retained Configure settings
snapshot.

Supported profile kinds:

- `chatgpt` — ChatGPT/Codex OAuth credentials for the Responses backend.
- `chat_completions` — OpenAI-compatible Chat Completions endpoint with base URL, typed API-key credential, model list, max output tokens, extra body, and compatibility options. The provider-kind picker labels it `OpenAI-compatible Chat Completions`.
- `openrouter` — OpenRouter profile with a typed API-key credential and either explicit or fetched models.
- `responses` — generic public Responses endpoint with base URL, optional API
  typed API-key credential, explicit `sse` or `websocket` transport, and an explicit model list.
  Omitted transport in an older profile means SSE. The provider-kind picker
  labels it `OpenAI Responses API`.

For API-key profiles, setup asks for the authority first: `Enter API key now`,
`Use configured named secret` when declarations exist, or `No API key` where
the profile kind supports keyless operation. Only direct entry opens the masked
value prompt.

Settings contain only a deterministic Secret-scope credential reference.
Provider startup loads and validates that typed record before publishing models.
Missing or malformed credentials exclude the profile. Runtime reloads credentials
at prompt boundaries and refreshes ChatGPT OAuth records with compare-and-swap,
so a losing refresher never retries a rotated token.

Initial Configure validates the complete bounded provider settings snapshot
before retaining parsed profiles or publishing models. An invalid filename or
profile, including legacy or inline credential fields, rejects the whole snapshot:
the extension publishes neither models nor Ready and the harness emits one
mandatory replayable warning. Invalid-filename warnings expose no raw filename;
profile-validation warnings may identify only the already-validated provider
name and a closed reason. Neither form exposes paths or settings values. Startup
does not mutate or migrate the rejected settings.

The profile kinds route to three deliberately separate wire backends.
`chat_completions` and `openrouter` use the OpenAI-compatible HTTP/SSE
`/chat/completions` adapter, including Function tools and semantic transcript
replay; this is the supported route for local servers such as llama.cpp.
`chatgpt` uses the private ChatGPT OAuth/Codex Responses adapter. Its ordinary
inference is WebSocket-only with no HTTP/SSE fallback. HTTPS is retained only for
OAuth, quota acquisition, and unary standalone compaction. It is not a public
API-key OpenAI Responses provider.

`responses` uses a generic API-key `/responses` adapter with explicit SSE or
WebSocket transport. The setup wizard preselects WebSocket only for the exact
official OpenAI base URL and SSE for compatible endpoints. Runtime never probes,
infers, or falls back between transports. It
requires user-configured models and does not discover models or choose provider
presets. Each turn sends the complete typed Responses transcript. It supports
assistant text, plain `reasoning_text` reasoning, and Function tools. Plain
reasoning follows the existing `show-thinking` UI behavior and is retained for
full-transcript replay; encrypted, summary-only, malformed, and mixed reasoning
is rejected. The backend preserves Responses replay sidecars and does not send
`previous_response_id` or `store`; it does not expose hosted/custom tools,
image/file inputs, or compaction. Existing `openrouter` profiles remain on Chat
Completions.

WebSocket attempts use one fresh connection and one `response.create` envelope.
Retries reconnect and replay the full local transcript; they do not reuse
connection-local `previous_response_id` state. This public Responses mode is not
OpenAI Realtime.

Each `responses.models[]` entry can set `efforts` to an exact set of supported
reasoning levels. Omission uses `[off, minimal, low, medium, high, xhigh, max]`;
an explicit empty list disables the control. Non-empty overrides reject
duplicates and publish in that canonical order. `tau provider add` omits the
field, so generated profiles receive the full set. Each request explicitly
sends the harness-effective level as `reasoning.effort`, mapping Tau `off` to
API `none`.

The extension has no ordinary `extensions.provider-builtin.config` credential
schema. Inline credentials remain in provider auth/profile storage; referenced
credentials use the separately scoped `extensions.provider-builtin.secrets`
declarations described above.
ChatGPT profiles publish model tags such as `shell:chatgpt` and `tools:custom-text` so the harness can choose compatible tool surfaces. Chat Completions profiles and individual models can also carry optional `tags`; published model metadata contains the provider/model tag union.

Compatible model entries may also set
`est_uncached_input_cost_1m_usd`, `est_cached_input_cost_1m_usd`,
`est_cache_write_input_cost_1m_usd`, and `est_output_cost_1m_usd` to
non-negative decimal USD prices per million tokens. They may set
`est_cache_storage_cost_1m_token_hour_usd` per million token-hours. Use quoted
decimal strings for fractional prices; integer JSON numbers
are also accepted. Missing values resolve built-in default prices for known
compatible model ids (currently `deepseek-v4-flash` from DeepSeek's standard API
pricing) and otherwise use the central GPT-5.6-equivalent `$5`/`$.50`/`$30`
fallback, including local and free models; explicit profile prices always take
precedence. Hardcoded ChatGPT ordinary-input/output values follow OpenAI's basic
public API pricing table, while private-route cache prices remain absent and use
only the non-authoritative central display fallback. A missing write price uses
ordinary input; missing storage usage
or price contributes no storage charge. The harness accumulates ordinary input,
cache reads, cache writes, output, and reported token-time storage into this
deliberately rough equivalent-API estimate per agent for the current runtime
only.

Chat Completions cache counters are ignored by default. Set
`compat.cache_usage` to `open_ai` or `deep_seek` only when the exact route uses
that response schema, and pair a selected schema with
`compat.stream_options: true` so the supported `include_usage` stream member
requests terminal usage. DeepSeek hit/miss counters are parsed only through
that explicit capability and remain response-local telemetry with probabilistic
residency. OpenRouter profiles and discovered models select streamed
OpenAI-compatible read/write telemetry by default, but never publish a cache
policy or cache controls: router-selected upstreams leave cache mechanism,
privacy, residency, renewal, and lifecycle unknown. Tau drops generic cache
contracts on OpenRouter routes. Unsupported Anthropic/Gemini-compatible cache
details remain absent; the extension has no native cache-object lifecycle
client.

Generic Chat Completions and public Responses model entries may add an optional
`cache_contract` describing documented cache kind, TTL shape, renewal, output
floor, quota treatment, and privacy posture for that exact route. This is an
operator assertion, not discovery: Tau never derives a hard TTL from route names
or recent hits. The adapter supplies prefix identity version `1`. Contracts are
transient model current state and contain no cache keys, object names, prompt
content, regions, timestamps, or hit history. They add no PATCH, delete, journal,
or restart behavior. Current generic backends reject
manual-deletion support because none owns a typed delete operation.

Anthropic documents explicit breakpoints with sliding 300-second or 3,600-second
TTLs, read renewal, and zero-output `max_tokens: 0` writes. A generic declaration
may describe one of those modes only when the exact configured proxy guarantees
it; Tau does not lower Anthropic `cache_control` or dispatch refresh requests.
With Anthropic's `1.25x`/`2x` write and `0.1x` read multipliers, equal-prefix
cost reaches the discrete break-even point after one five-minute read or two
one-hour reads. Tau never converts that fact into a roughly four-minute cadence
or traffic during unknown or unbounded idle.

Gemini explicit context caching is a named externally managed provider object:
its creation sets a fixed expiry, Gemini can PATCH the TTL/expiry, and deletion
is provider-side. A generic exact-route contract may state
`explicit_object`, `fixed` TTL, and `patch_expiry` with named-object storage,
ZDR incompatibility, provider-specific residency, and
`manual_deletion: unavailable`. That records provider semantics while accurately
stating that Tau has no typed create/PATCH/delete operation. A Chat Completions
profile can retain an opaque externally managed object reference in
non-conflicting `extra_body`; Tau persists that configured request member and
clones it into each attempt, but does not model separate object identity,
lifecycle, or accounting state in runtime metadata or journals. The raw
storage-price field can state a token-hour rate, but generic
Gemini-compatible routes do not report object storage usage.

Gemini implicit caching is only an automatic-prefix optimization. Use unknown
residency, unsupported renewal, and unknown output floor unless the exact
compatibility route documents stronger facts. Tau keeps compatible request
prefixes stable but sends no keepalive, prewarm, or lifecycle traffic.

For an exact generic GPT-5.6 route using typed explicit OpenAI breakpoints,
publish the 1,800-second lifetime as `minimum`, with unsupported renewal and an
unknown output floor. OpenAI documents eligibility for at least 30 minutes and
possible longer retention, not sliding renewal. Older models need a separate
conservative policy: typical eviction is not a hard TTL, so use unknown
residency and unsupported renewal unless the exact route has a stronger documented
contract. OpenAI read/write counters remain observations, never TTL evidence.
The harness cache-refresh scheduler is disabled by default and requires explicit
global opt-in. It accepts only safe sliding read-renewal contracts, explicit
prices, concrete quota policy, and measured writes plus break-even reads. One
observation can authorize only one non-generating full-prefix resend during a
bounded wait. Real prompts and Provider cooldowns take priority; failure never
enters inference retry, and no lifecycle state survives restart. Observed
eviction or a recent read never infers a TTL.

Generic OpenAI-compatible cache request controls are opt-in and route-local:
`compat.openai_prompt_cache.key: agent` derives a stable Tau key. Chat
Completions accepts either legacy `retention: in_memory`/`"24h"` or explicit
`options: { mode: explicit, ttl: 30m, boundary: system_prompt }`. Explicit mode
marks the end of a non-empty system prompt, preventing an implicit breakpoint
from writing a volatile transcript suffix. The legacy retention policy instead
accepts the provider's automatic behavior, including a possible volatile-suffix
write premium. Public Responses also accepts
`options: { mode: explicit, ttl: 30m, boundary: first_input_text }`. It preserves
top-level `instructions`, marks the earliest Tau-constructed non-assistant
input-text block, and rejects locally when no eligible block exists. This is
per-agent multi-turn cost control, not a system-prompt boundary or cross-agent
reuse.
The retired `compat.prompt_cache_key: bool` is invalid. Tau neither guesses
cache support from route names nor adds native Anthropic/Gemini cache clients or
cache lifecycle traffic.

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

Built-in provider HTTP and WebSocket networking snapshots `http_proxy`,
`https_proxy`, `all_proxy`, and `no_proxy` at provider startup, with each
lowercase variable taking precedence over its uppercase equivalent. HTTP/WS use
the HTTP proxy class and HTTPS/WSS use the HTTPS class, then fall back to
ALL_PROXY. `NO_PROXY` is the only direct bypass; a selected proxy never falls
back directly after failure. Supported proxy URLs use HTTP or HTTPS with
optional percent-encoded Basic credentials. SOCKS, PAC/WPAD, desktop discovery,
integrated authentication, and redirects are unsupported. Restart after changing
network environment.

Release acceptance covers HTTP and HTTPS through HTTP and HTTPS proxies, WS
through an HTTP proxy, and WSS through HTTP and HTTPS proxies. Secure targets
through HTTPS proxies cover proxy TLS, CONNECT, target TLS, and the target
request or WebSocket upgrade as distinct wire layers.

TLS always verifies with platform trust. `TAU_PROVIDER_CA_BUNDLE` can add one
bounded certificate-only PEM bundle captured at startup; it cannot disable or
replace platform verification. Reqwest does not expose the status of a rejected
HTTPS/WSS CONNECT tunnel, so Tau safely reports a hidden proxy 407 as a redacted
proxy-route transport failure rather than guessing authentication from error
text. Plain HTTP/WS proxy 407 responses remain specifically classified.

The harness assembles prompts and routes provider-owned turns to this extension.
The extension publishes a transient `ProviderModelsDeclared`; after generic
commit and activation, the harness derives canonical `ProviderModelsUpdated`
current state. The extension streams response updates and emits final response
events with stop reasons and usage/cache diagnostics.

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

A prewarm response id is eligible only on its exact socket and
profile/mode/cache identity when the real lowered input retains the warmed input
as an exact prefix. Fingerprint or prefix divergence, cancellation, invalidation,
or a stale ownership generation discards the anchor and sends full context.

One finite Codex inference attempt has one immediate WS repair budget. An exact
stale-chain/connection-limit code or dead socket may consume it only before
semantic output. Dispatch is reported at the first request send and transport
bytes remain cumulative across the repair. Once assistant, reasoning, tool, or
opaque output begins, Tau does not replay and splice the turn; transient output
is cleared before extension-owned logical retry. Canonical provider codes, never
arbitrary prose, authorize these classifications.

After setup, ChatGPT/Codex inference is WebSocket-only with a separate default
five-minute idle watchdog. The timer resets on each provider frame and is not an
absolute turn-duration cap. If upstream stalls, Tau aborts that finite attempt,
clears tentative output, and parks the logical prompt for another attempt.

Prompt execution concurrency defaults to 4 and can be overridden with `TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY`. Retry delays release those worker slots for every prompt origin. Policy-generated jittered delays reach about one minute for transient failures and at most about thirty minutes for persistent failures. Trusted later `Retry-After` and reset hints remain lower bounds except for usage-window reset estimates: users or providers may restore access early, so Tau keeps probing at the bounded persistent-failure cadence instead of sleeping until the reported reset. Retry state exists only for the running process/session and is not replayed after cold restart.


Provider response streaming note: built-in providers submit explicit transient
`provider.response_updated_reported` append deltas for visible assistant/reasoning text
and `provider.response_finished_reported` terminal payloads. The harness publishes the
correlated canonical update and durable finished facts. Retry diagnostics are provider
status updates.

Live byte stats are public provider-owned `provider.response_updated.response_stats`
samples. Built-in providers count backend response bytes at the transport receive
boundary before semantic parsing, include non-visible streamed tool-call
arguments, and do not count request bytes, HTTP headers, or tool execution
outputs/results. They are not transcript or final-response content.

Built-in providers optionally include provider-owned
`first_semantic_output_elapsed_micros` in those live stats. It measures from a
finite attempt's first backend send/enqueue to the first synchronously accepted
non-empty assistant/reasoning/tool semantic unit; material opaque reasoning is
observed at completion. Scheduled retries start a fresh measurement, while
Codex's transparent pre-semantic repair retains the original dispatch boundary.
The value is captured before batching, repeated on later samples, and remains
live-only: it is absent from finished output, journals, replay, snapshots, and
final turn stats.

Built-in providers also run a conservative exact streaming repetition guard over assistant text, reasoning text, and tool-call argument deltas. When a high-volume tight exact loop is detected, the provider clears transient streamed output and finishes with `stop_reason: repetition_detected`, empty final output items, and bounded display-only error text; the harness treats this as a loop-guard trigger rather than retrying the provider turn.

### Watcher-visible provider work

Provider retries carry closed structured categories, saturating attempt counts, and approximate bounded delays independently of human UI prose. After validating prompt ownership, the harness owns the current per-agent/turn/prompt snapshot and session-local watcher fanout. Live delivery is limited to first category, category/phase changes, and terminal failure; same-category storms only refresh the late-watch snapshot. Enabling or re-enabling returns current sanitized state and emits an initial client snapshot without prompting the model. Durable live facts replay as transcript context without re-fanout; disable, prune, and session change stop delivery. Raw provider bodies, status text, errors, headers, account data, secrets, and prompt content never cross this boundary.
### Standalone compaction tools

`compact` and `agent_compact` require a live provider route whose selected
model advertises standalone compaction. Unsupported or inline-only models are
rejected without a durable accepted request. Provider failures terminate the
background tool exactly once.
The static `:retry` command can release the selected agent's exact parked
logical prompt immediately. This one-job override preserves retry accounting,
does not initially wake peer jobs, and still waits for a bounded provider worker
slot. If that exact probe commits a successful terminal response, Tau clears the
matching current provider cooldown generation and wakes same-profile peers with
stable anti-herd jitter. Errors, cancellation, stale successes, and best-effort
quota display updates do not clear inference cooldowns.

## Local summary compaction

Generic Chat Completions and public Responses models do not advertise standalone
compaction. An explicitly local Chat Completions model can opt in with
`local_summary_compaction`, the `local_transcript_v1` profile, a context window
matching the model, and conservative input-byte/output-token/output-byte limits.
Tau uses the exact model for one no-tools request, intentionally omits image
bytes with a loss marker, persists no full request, and accepts only a bounded
six-section summary as untrusted synthetic history. Invalid output, insufficient
context, cancellation, route loss, stale state, and post-output failures end the
durable transaction without inference fallback or ambiguous resend.
