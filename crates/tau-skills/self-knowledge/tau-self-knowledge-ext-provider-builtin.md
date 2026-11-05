---
name: tau-self-knowledge-ext-provider-builtin
description: Use when configuring or troubleshooting Tau provider-builtin, including built-in model providers, ChatGPT/Codex OAuth, OpenAI-compatible Chat Completions, OpenRouter, provider setup, model publication, or tau provider commands.
advertise: false
---

# Tau provider-builtin extension self-knowledge

`provider-builtin` runs `tau-ext-provider-builtin`, is enabled by default, and
publishes configured, locally credential-usable models. Publication does not prove
remote authentication, entitlement, or available quota. See `docs/providers.md`
for profile schemas and route-specific detail.

## Profiles and setup

```sh
tau provider add
tau provider login <name>
tau provider list
tau provider show <name>
tau provider remove <name>
tau provider rename <old> <new>
```

Use `--extension <instance>` when multiple built-in instances exist.
`tau provider add [KIND]` accepts `chatgpt`, `chatgpt-plan`, `grok`,
`chat-completions`, `responses`, and `openrouter`; omission opens a picker.

| Profile kind | Route |
|---|---|
| `chatgpt` | Private ChatGPT/Codex OAuth Responses; inference is WebSocket-only |
| `chatgpt_plan` (CLI `chatgpt-plan`) | Separate public Sign in with ChatGPT plan-sharing Responses/SSE |
| `grok` | Native Grok subscription OAuth |
| `chat_completions` | OpenAI-compatible HTTP/SSE `/chat/completions`, including local servers |
| `openrouter` | Chat Completions with explicit or discovered models |
| `responses` | Generic public `/responses`, explicit model list and `sse` or `websocket` transport |

Settings contain no credentials; typed credentials live in private Secret state.
Add defaults to mutable XDG state. `--config` targets portable XDG config;
`--config --output -` emits credential-free JSON while storing required
credentials locally. Config/state names must be disjoint. Remove accepts
`--config`/`--state` or infers a unique source. The retired `provider-settings/`
directory is not discovered: move only its JSON, not Secret records.

Restart after settings changes; credential rotation takes effect at the next
prompt boundary. Login replaces credentials without modifying settings, config
symlinks, or creating a state shadow. For renamed instances:
`tau provider --extension <instance> login <name>`.

API-key setup offers masked direct entry, a named secret, or keyless operation
where supported. Existing-name selection requires an available source.
`Enter secret name for deferred binding…` instead saves settings without a
Secret record, allowing deployment before the declaration/value exists.
Restart imports the current bound source; unavailable sources disable the profile.
Declare sources under `extensions.provider-builtin.secrets`; there is no ordinary
`extensions.provider-builtin.config` credential schema.

Explicit keyless profiles use `credential: {"kind":"none"}` and never read/write
Secret records. Lost credentials never silently enable keyless requests.
Invalid filenames/profiles or legacy/inline credential fields reject the entire
startup settings snapshot without migration. Missing/malformed referenced
credentials omit the affected profile's models.

`tau dev preview-declarations` inspects config-owned metadata without credentials
or provider runtime work. State-owned profiles are omitted and availability
unverified, so provider results are partial and the command exits nonzero. It is
not effective `print-tools`/`print-prompt` output.

## ChatGPT/Codex

Private `chatgpt` never falls back to HTTP/SSE or a paid API key. Set
`responses_lite_compatibility: true` in the profile and restart to request Lite;
capability still depends on the exact model. Tau never changes modes as a retry
fallback. Both modes omit provider-inline context management.

Set top-level `application_idle_timeout_secs: 900` in the credential-free
ChatGPT provider JSON for a fifteen-minute application-message silence limit.
This is not a `harness.yaml` key. Default: 300 seconds; accepted range:
1–4294967295. Consumed Text messages renew the limit, including metadata;
Ping/Pong does not. It is not a total turn deadline and does not alter prewarm,
setup, or standalone compaction limits. Longer limits delay stall recovery.

Tau reuses account/mode/agent WebSockets and `previous_response_id` where
possible. A stale chain can trigger one full-replay repair before semantic output;
it cannot splice a replay onto partial output. Retryable failures return to the
logical-prompt scheduler.

Codex `partial_answer` is visible answer text, not a final summary. Tau preserves
observed phases across history/replay/model switches without inferring capability.
A phase alone requests no continuation. Explicit `end_turn=false` continues the
same outer turn, with at most 16 additional no-tool samplings; exhaustion retains
text/usage and errors.

Use exact published capabilities, not model-name guesses:

- `chatgpt/gpt-6.1-sol` and `chatgpt/gpt-6.1-terra` are selectable without changing
  the `gpt-6-sol` default. They use local-summary compaction. Public coding routes
  need `responses`, not Chat Completions tool traffic; tested entries live at
  `crates/tau-ext-provider-builtin/fixtures/provider-models/gpt-6.1-sol.json` and
  `crates/tau-ext-provider-builtin/fixtures/provider-models/gpt-6.1-terra.json`.
- `:model chatgpt/gpt-6.2-astra` selects a provisional private entry. Its
  272,000-token local budget (258,400 effective input) is not verified capacity.
  It uses Standard client tools and local-summary compaction; do not infer
  native efforts, hosted search, images, phase, or native compaction.
- Image tool results require exact-route support and role permission.
  `ARCH-tau-provider-codex` records pinned evidence, not live visual-fidelity tests.
  Reference-bearing image history requires the Artifact store and root registry
  alongside journals for backup/restore; missing/corrupt images fail the consuming
  request before submission.

Opaque-compacted history can attempt unchanged replay across private account
aliases only within one built-in provider instance, with the same exact upstream
model and effective Responses mode and matching current frozen profiles.
`:model` preflights compatibility. Remote acceptance is not guaranteed; rejection
preserves history rather than stripping opaque bytes or falling back accounts.

### Hosted search and image generation

Audited Standard routes advertise hosted search; Lite does not.
`agents.web_tools` selects the implementation, cached by default. Hosted search
runs inside inference, not as a Tau tool; ambiguous retries may repeat paid search.
Its transient `web_search (native)` UI rows do not survive cold restart or count
as tool facts. Hosted search may navigate accessible pages without `web_fetch`;
fetch only when content is missing, stale, unreadable, or needs another extraction.
Neither path guarantees complete or current text.

ChatGPT profiles declare `generate_image` by default; opt out with
`image_generation: false`. Roles must opt in with `enable_tools: [generate_image]`;
an instance `tool_prefix` prefixes the alias. Standard and Lite are supported.
Use `{"prompt":"..."}` (non-whitespace, at most 32 KiB) to generate.
Add `attachments: ["<tau-artifact:FULL_KEY>", ...]` to edit 1–5 originals in
the supplied order. Omitted or empty attachments still generate. Bare canonical
keys also work; paths, URLs, file IDs and previews do not. No masks, batches or
automatic recent-image selection.

Inputs must be valid static PNG, JPEG or WebP, at most 16 MiB each and 24 MiB
total. Separate limits cap data URLs at 32 MiB and JSON at 33 MiB. Tau decodes one
raster at a time, with limits of 8192 pixels per axis and 256 MiB per decode.
Inputs at the raw-byte limit can exceed encoded limits due to padding and prefixes.
Attached original bytes and embedded metadata go to the selected account's
remote image service; check disclosure
intent. Artifact reads do not pin or renew retention, so later edits can fail.
For a generation-then-edit workflow, pass the first result's `artifact` header
as the next call's single attachment and describe changes to “image 1”.

Generation uses fixed `gpt-image-2` on the selected subscription account, without
entitlement probes, account switching, or API-key fallback. Nonempty attachments
use the sibling JSON edit endpoint, without generation fallback or separate
uploads. Edits follow the upstream source and have deterministic tests, but no
live-account verification. Success returns
`artifact`, `size`, and `mime_type` headers for the original PNG, at most 16 MiB.
Pass `<tau-artifact:FULL_KEY>` directly to `import` or `read_image`; no inline image
or store path is returned. Artifacts outlive ephemeral transcripts. Ordinary
background/wait/cancel applies; uncertain/failed generation is not retried.
Cancellation during publication may leave an orphan artifact. Network deadline:
five minutes; overall call: seven minutes. Expired credentials require login
before another explicit invocation.

## Public Sign in with ChatGPT

`tau provider add chatgpt-plan` requires persistent `--state` or `--config`,
not `--output -`. It registers a separate account/workspace client using a real
loopback callback and direct plan scope; Codex credentials never migrate.
Choose a catalog model and enter missing context limits explicitly.

Use separate names for accounts/workspaces. `login NAME` reauthorizes the same
registration; `logout NAME` attempts remote session revocation and clears local
tokens while retaining registration, reporting unconfirmed revocation. Disabled
grants retain registration without models; repeat add with the same name to grant
permission and choose models.

Refresh uses a per-credential OS lock and saved-token reload. After an ambiguous
exchange failure, sign in again rather than expecting background retries.
Cross-process coordination does not guarantee crash-safe remote token exchange.

This route supports text/reasoning/local Function tools, full history,
`store:false`, `stream:true`, and developer `additional_tools`. It exposes no
hosted/custom tools, image generation, WebSocket continuation, native compaction,
or paid-key fallback. Local-summary compaction remains available.
Only `response.completed` succeeds; inference failures are terminal, without
automatic replay. Catalog visibility does not establish plan entitlement or rule
out separately enabled credits. Validation is offline/loopback, not live-account.

## Native Grok

`tau provider add grok` uses device OAuth and a setup-time catalog snapshot.
`login NAME` replaces credentials; `logout NAME` deletes only local `grok_oauth`,
without remote revocation. Missing credentials withdraw models. Update settings
and restart for model changes.

Exact audited `grok-4.7` and discovered aliases default to Function tools; other
routes require explicit capabilities. Native tool images require opt-in and lack
live verification. Native compaction replays one encrypted item; OAuth compact
entitlement and live fidelity remain unverified.

An exact 401 can force one refresh; 403 cannot. Failed rotation generations need
login. Refresh coordination is process-local: cross-process exchange/crash can
lose rotating tokens. Grok never falls back to ChatGPT auth, cookies, private
Build headers, or paid API keys. Unknown status/output failures remain terminal;
native failures do not select another paid summary implementation.

Do not initiate login, live inference, or paid traffic without consent. No
free/unlimited, entitlement, extra-usage, or quota-safety promise exists; consult
`docs/providers.md` for client-ID, pricing, and partial-limit caveats.

## Compatible and public API models

Configure actual context limits and route capabilities. Generic Chat Completions
accepts `supported_tool_types: []` or `["function"]`; parallel support requires
Function support. OpenRouter discovery independently checks `tools`,
`tool_choice`, and `parallel_tool_calls`; absent/unversioned metadata grants
no capability. Tau does not force `provider.require_parameters`.

`max_input_tokens` narrows admission/compaction below `context_window`.
`max_output_tokens` declares capability and clamps a nonzero profile output policy;
policy zero still omits the wire cap. Profile/model `tags` publish their union.

For `Qwen/Qwen3.8-27B`, use model-local explicit `low`/`medium`/`xhigh`
reasoning bands, `single_initial_system_message: true`, role effort `xhigh`,
Qwen's fixed thinking sampler, `chat_template_kwargs.preserve_thinking: true`,
and `reasoning_replay: both`. Use the server's actual context reservation.
Servers lacking top-level `reasoning_effort`, including current llama.cpp, should
publish only `xhigh` with wire policy `omit`. Non-thinking needs a separate profile
with no effort and `chat_template_kwargs.enable_thinking: false`.
Start local serving with `TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY=1`.

For a verified vision-capable llama.cpp route, configure both
`input_modalities: ["text", "image"]` and
`tool_result_modalities: ["text", "image"]` on that model entry. Omission remains
text-only; Tau never infers vision from a model name.

Public `responses` requires explicit models. Setup preselects WebSocket only for
the exact official OpenAI base URL, otherwise SSE; runtime neither probes nor
falls back between transports. Omitted transport in older profiles means SSE.
Each attempt replays the full typed transcript, without `previous_response_id`
or `store`. This is not Realtime. The route supports text, completed reasoning,
and Function tools, not hosted/custom tools or image/file inputs. Tau preserves
validated opaque reasoning for replay without displaying it.

`responses.models[].reasoning_effort.mapping` uses increasing cut points/native
levels starting at `0.0`. Omission maps `[none, minimal, low, medium, high, xhigh, max]`;
`[]` disables control. Boundaries at/below `0.5` select the higher band; those
above select the preceding lower band. Migrate retired `efforts` arrays.

Public Responses `response.incomplete_details.reason: "max_output_tokens"`
preserves validated partial output/usage but never retries unchanged requests or
executes truncated Function calls. Replay-safe plain reasoning-only output may
use one bounded continuation. Other incomplete reasons remain failures.

## Retries and compaction

ChatGPT/Codex, Chat Completions, and OpenRouter inference retries until success or
cancellation unless Tau proves the unchanged request invalid. Unknown failures
retry conservatively; billing/quota/auth/configuration failures retry slowly.
This does not apply to the terminal-failure policies above.

Retry delays release worker slots; concurrency defaults to 8, configurable with
`TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY`. Same-profile jobs share cooldowns.
Server timing hints remain lower bounds except usage-window reset estimates:
Tau keeps bounded probes because access may return early. Retry state does not
survive cold restart. Use `:retry` to release the selected parked prompt early,
or cancel to stop. A successful probe can wake same-profile peers; merely
refreshing quota display does not clear cooldowns.

An exact high-volume streaming loop stops with `stop_reason: repetition_detected`
and clears tentative output. The harness treats it as a loop guard, not a
retryable provider turn.

Rejected OAuth refresh generations remain suppressed within the process until
credentials/profile change. An exact provider 401 permits one forced refresh,
not replay with the rejected bearer if recovery fails. A failed preemptive refresh
can use only the authoritative still-valid access token.

`compact`/`agent_compact` require advertised standalone capability.
Known native absence uses local summary; a definitive native rejection before
semantic output permits one local request in the same transaction.
Cancellation/post-content failure never falls back. Configured thresholds remain
independent of native support; `provider_default` without a numeric default
has no boundary.

Standalone pre-output transport failures remain pending in five-attempt bursts,
with one-, two-, four-, then capped five-minute backoffs between bursts.
Other transient failures stop at or beyond the fifth total attempt.
Terminal failures pause inference until explicit `:compact` or authorized
`agent_compact`; ingress/timers do not rearm them. Restart pauses interrupted
compaction. Clean empty/reasoning-only local summaries may retreat to a smaller
complete prefix, at most four successors; exhaustion needs explicit recovery.

Chat Completions/OpenRouter/public Responses advertise local-summary compaction
when context is nonzero. `local_summary_compaction` accepts optional
`max_input_bytes`, `max_output_tokens`, `max_output_bytes`; `{}` keeps defaults:
no prefix-byte cap, inherited ordinary output policy, 256 KiB output-byte bound,
no proactive threshold. No separate default generation cap exists. Retired
`serialization_profile`/`context_window_tokens` keys are invalid.

Summary requests retain the ordinary prefix and append a harness instruction.
Any returned tool call fails without execution. Tau accepts nonempty bounded
final text, discards reasoning, and stores one synthetic checkpoint.
Chat Completions output-limit continuations assemble the narrative; warnings
require checking both Tau policy and server generation caps. Cancellation,
post-output failure, insufficient context, or route loss does not authorize
ambiguous resend. Native compaction counts `N/?` describe observed activity,
not percentage reduction; only canonical success seals `N/N`.

## Cache, cost, and diagnostics

Chat Completions ignores cache counters unless `compat.cache_usage` selects
`open_ai` or `deep_seek`; pair this with `compat.stream_options: true`.
OpenRouter selects streamed read/write telemetry but publishes no cache contract:
upstream residency, privacy, and lifecycle remain unknown.

Generic Chat Completions/public Responses `cache_contract` is an operator
assertion about the exact route, not discovery. Hits do not prove TTL.
Tau has no native Anthropic/Gemini cache-object create/PATCH/delete client.
Cache refresh is disabled by default; global opt-in still requires safe sliding
read-renewal contracts, explicit prices/quota policy, and measured break-even
evidence. It does not authorize unbounded idle traffic.

Route-local OpenAI cache controls:

- `compat.openai_prompt_cache.key: agent` derives a stable key.
- Both adapters accept `options: { mode: implicit, ttl: 30m }`.
- Chat Completions explicit mode:
  `options: { mode: explicit, ttl: 30m, boundary: system_prompt }`.
- Public Responses explicit mode:
  `options: { mode: explicit, ttl: 30m, boundary: first_input_text }`.
  This marks the first eligible non-assistant text block, not a system boundary
  or cross-agent cache; no eligible block fails locally.

Implicit mode may write volatile suffixes at a premium. Retired
`compat.prompt_cache_key: bool` and `retention: in_memory` / `"24h"` settings are
invalid; Tau does not translate them to the new contract.

Optional prices are nonnegative decimal USD values:
`est_uncached_input_cost_1m_usd`, `est_cached_input_cost_1m_usd`,
`est_cache_write_input_cost_1m_usd`, `est_output_cost_1m_usd`, and
`est_cache_storage_cost_1m_token_hour_usd`. Quote fractional decimals; integers
are accepted. Missing prices can use fallback estimates even for local/free
models. Displays are rough current-runtime API equivalents, not subscription
billing; see `docs/provider-price-audit.md`.

ChatGPT quota `Q?` means weekly data is unavailable, stale, or unbound.
`Q-`, `Q=`, `Q+`, `Q!` require fresh exact-model pool binding; credits do not
count as weekly usage.

`cache_diagnostics: off|metadata` freezes at startup. Metadata and separate exact
request/response captures default on for durable inference/compaction; disabling
metadata does not disable exact captures. Ephemeral activity is excluded.
Inspect native compact `received_response`/`raw_events` and terminal/truncation
flags, not the synthetic finished capture. `tau agent/session cache` recognizes
scalar records but does not analyze them; see `docs/agent-cache.md`.

## Networking

Startup snapshots `http_proxy`, `https_proxy`, `all_proxy`, `no_proxy`; lowercase
wins over uppercase. HTTP/WS use the HTTP class, HTTPS/WSS the HTTPS class, then
`ALL_PROXY`. Only `NO_PROXY` bypasses a selected proxy; failures never fall back
directly. Supported proxies use HTTP/HTTPS with optional percent-encoded Basic
credentials, not SOCKS, PAC/WPAD, desktop discovery, or integrated auth.
Redirects are unsupported. Restart after environment changes.

TLS uses platform trust. `TAU_PROVIDER_CA_BUNDLE` adds a bounded certificate-only
PEM bundle at startup; it cannot disable/replace verification. Hidden HTTPS/WSS
CONNECT 407s may appear as redacted proxy-route transport failures.
