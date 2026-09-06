# Providers

Canonical context-window rejection is reported as a typed terminal provider
failure. The harness, not the adapter, decides whether an ordinary no-output
inference may receive one standalone-compaction recovery; provider-authored
recovery-disposition fields are ignored.

A provider is a normal Tau extension that exposes models and executes prompts.
The harness does not own provider-specific LLM execution; provider extensions are the model executors.

## Prompt-cache prefix stability

For ordinary ChatGPT/Codex, public Responses and Chat Completions (including
OpenRouter) inference and standalone compaction,
`cache_diagnostics: "metadata"` is the
default in provider profiles; `"off"` disables only scalar cache observations
after restart. Exact request/response capture remains default-on for durable
activity. Scalar capture uses existing private storage and diagnostic retention
and does not alter prompts, provider traffic, retries or accounting. Other
adapters and per-item attribution are not yet supported. See
[Private runtime metadata](agent-cache.md#private-runtime-metadata).

Tau keeps each backend's provider-visible request meaning stable whenever only
local prompt correlation changes or a newest conversation turn is appended.
System/developer authority, ordered history, full tool definitions and schemas,
and supported reasoning/thinking settings remain unchanged; new context follows
the preceding history. When an existing backend accepts `tool_choice: "none"`,
standalone compaction and general callers retain the full tool-definition list
and use that selector rather than removing definitions solely to disable calls.
Non-tool extension side queries are the deliberate exception: they use `none`
and remove ordinary and hosted logical web definitions so provider-hosted search
cannot leak side-query text or incur cost. Other definitions may remain for
cache structure, but the provider cannot invoke them.

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
declarations after Secret hydration. A snapshot includes only routes with valid
local configuration and locally usable credential material; missing or malformed
credentials omit their routes. Later observed credential changes publish a complete
replacement snapshot. Local usability does not guarantee remote authentication.
After ordinary interception
and commit, the harness publishes protected canonical `provider.models_updated`
current state and updates routing/availability projections. The declaration
payload contains the proposed model list. The canonical payload adds
`publisher_extension_id`, the stable configured provider whose complete current
state it replaces; an empty list withdraws that provider's models. Replay exposes
one canonical snapshot per active provider, including empty snapshots. Model lists
carry metadata, not just IDs:

The harness validates every proposed model independently. A malformed entry is
omitted from the accepted snapshot and produces a structured
`provider.model_declaration_diagnostic`; valid siblings remain unchanged. Tau
rejects zero context windows or model output capabilities, standalone-only
metadata without effective standalone support, zero standalone thresholds or
prefix budgets, and standalone thresholds larger than the route's effective
legal input limit. Diagnostics distinguish thresholds above the total window
from those above only a separate input maximum. Effective support is
`supports_standalone_compaction || standalone_compaction_generation_negative`;
generation-negative routes keep their standalone metadata. Reasoning mappings
must be non-empty, start at `0.0`, keep thresholds within `0.0..=1.0`, and
strictly increase both thresholds and native levels. A non-first threshold must
be below `1.0`, because inward ownership would make a higher band starting at
`1.0` unreachable. Tau does not strip or default invalid fields. At an exact cut
point at or below `0.5`, the higher band owns the value; above `0.5`, the
preceding lower band owns it.

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
    context_window: TokenCount,
    max_input_tokens: Option<TokenCount>,
    max_output_tokens: Option<TokenCount>,
    efforts: ReasoningEffortCapability,
    verbosities: Vec<Verbosity>,
    thinking_summaries: Vec<ThinkingSummary>,
    supports_compaction: bool,
    supports_standalone_compaction: bool,
    standalone_compaction_generation_negative: bool,
    standalone_compaction_threshold: Option<TokenCount>,
    standalone_compaction_prefix_budget: Option<ByteCount>,
    cache_policy: Option<ProviderCachePolicy>,
    est_uncached_input_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    est_cached_input_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    est_cache_write_input_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    est_output_cost_1m_usd: Option<EstimatedUsdPerMillion>,
    est_cache_storage_cost_1m_token_hour_usd: Option<EstimatedUsdPerMillionTokenHours>,
}
```

`context_window` is the required total model window. `max_input_tokens`
optionally narrows the exact route's legal input boundary; omission falls back
to `context_window`. `max_output_tokens` is the model's separate output
capability; omission leaves it unknown rather than inventing a value from the
window. The provider profile's `max_output_tokens` remains Tau's request-policy
cap: a nonzero request uses the smaller policy and model-capability value, while
zero continues to omit the request cap.
`standalone_compaction_prefix_budget`, when present, is a nonzero `ByteCount`
of the canonical JSON-serialized historical `PromptContext`. The native trigger
is excluded. The harness fits a prefix in this byte domain, and the adapter
exactly remeasures the fully materialized historical prefix before dispatch.
Provider-specific whole-wire bytes are not compared with token-window metadata;
an adapter may enforce a separate whole-wire limit only when its transport
publishes a genuine byte-domain bound.
When the budget is absent, the harness dispatches the exact normalized
provider-closed target without a local byte admission check; canonical typed
context rejection may then authorize strict-predecessor retreat.
`input_modalities` declares what the exact provider/model route accepts as
prompt input, while `tool_result_modalities` declares what it accepts inside
native tool-result output. A tool that returns images is exposed only when both
lists contain `image`; omitted lists preserve legacy text-only behavior.

For a Chat Completions profile, declare these fields on only the exact
multimodal `models[]` entry:

```json
{
  "id": "Qwen/Qwen3.8-27B",
  "input_modalities": ["text", "image"],
  "tool_result_modalities": ["text", "image"]
}
```

The two image declarations are atomic and accept only canonical `[text,
image]` ordering. Tau rejects one-sided, image-only, repeated, or reordered
declarations before publishing models. Omission remains text-only. On an
opted-in route the Chat Completions adapter replays an image-bearing Function
result as a `tool` message with text followed by high-detail `image_url` data
URL parts, as accepted by llama.cpp. Do not set these fields merely because a
model name suggests vision support; the exact endpoint and loaded projector
must accept that wire shape.

`supported_tool_types` also describes the exact route. An omitted or empty list
means no native tool support; providers must publish `function` or `custom`
explicitly. The harness owns tool definitions and execution and only exposes
definitions whose types appear in this list.

The built-in Chat Completions adapter is Function-only: its configured model
value must be `[]` or `["function"]`. A model without Function support must also
set `supports_parallel_tool_calls: false`.

`supports_parallel_tool_calls` is the effective route capability used to make
system-prompt guidance truthful; it is not merely abstract model metadata.
Omission defaults to false, and the harness forces it false when no tool type or
no effective tool definition is available.
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
membership; completed descendants remain included until final session shutdown. If a
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

### Runtime cache contracts

Provider models may publish an optional runtime-only cache contract. It
classifies the exact route as automatic-prefix, explicit-breakpoint,
explicit-object, or response-chain caching and separately declares sliding,
minimum, fixed, or unknown residency. Minimum residency is not a hard expiry,
and Tau never turns recent hits into a TTL or renewal guarantee.

The contract also declares read, expiry-patch, recreate, or unsupported renewal;
zero, one, unbounded-reasoning, or unknown output floor; request/read/write/output
quota treatment; an adapter-owned prefix-identity version; and privacy facts.
Privacy distinguishes volatile memory, extended provider retention, named
provider objects, proxy-specific state, and unknown storage. ZDR compatibility,
data-residency effect, and manual deletion availability remain explicit.
Unknown or provider-specific values must not be presented as compliant.
Automatic caches generally cannot be manually cleared.

The three existing raw model price fields remain the sole cache read, write, and
token-hour storage price authority. The broad equivalent-API fallback used for
display is not a provider fact and cannot drive cache policy. Contracts contain
no prompt, cache key, object name, timestamp, or residency history. They travel
only in transient model current state; Tau adds no refresh, PATCH, delete,
journaling, restart recovery, or cache lifecycle operation.

Generic Chat Completions and public Responses models may declare
`cache_contract` metadata. The adapter supplies prefix identity version `1`.
For example:

```yaml
cache_contract:
  kind: automatic_prefix
  ttl:
    kind: sliding_known
    seconds: 300
  renewal: read
  output_floor: zero
  quota:
    requests: counts_fully
    read_tokens: exempt
    write_tokens: counts_fully
    output_tokens: exempt
  privacy:
    storage: volatile_memory
    zero_data_retention: compatible
    data_residency: preserves_route_policy
    manual_deletion: unavailable
```

This is an operator assertion for one exact generic route. Tau does not infer it
from endpoint, provider/model name, OpenRouter routing, typed request controls,
cache usage, or recent responses. Current production backends have no typed
cache-object deletion, so generic profiles cannot declare manual deletion
support. The private ChatGPT/Codex route publishes response-chain recreation
with zero output but unknown TTL, cache billing, quota, and privacy. It omits
raw read/write/storage cache prices; the UI may still use its
non-authoritative central fallback estimate. Those unknowns deliberately
prevent safe scheduled renewal.

Anthropic's
[prompt-caching documentation](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
defines explicit cache breakpoints with sliding five-minute and one-hour TTLs.
A cache hit refreshes the selected TTL, and a request with `max_tokens: 0` can
write or refresh a breakpoint without generating output.
The corresponding generic policy facts are `kind: explicit_breakpoint`,
`ttl: sliding_known` with `seconds: 300` or `3600`, `renewal: read`, and
`output_floor: zero`. Each declaration describes one exact route and one
selected TTL; it does not mean that Tau can select between TTLs or lower
Anthropic's nested `cache_control`.

Anthropic's
[published cache prices](https://platform.claude.com/docs/en/about-claude/pricing#prompt-caching)
are `1.25U` for a five-minute write, `2U` for a one-hour write, and `0.1U` for
a read, where `U` is the route's ordinary input price. For the same
cached-prefix token count, the discrete break-even read count is the least
integer `n` satisfying
`W + nR <= (n + 1)U`: one read for the five-minute mode and two reads for the
one-hour mode. This calculation uses only an exact route's explicit ordinary,
cache-read, and cache-write price fields; never use Tau's fallback estimate.
The result excludes uncached suffixes, output, mixed TTL breakpoints, Batch,
regional, residency, and negotiated price modifiers.

An operator may publish these facts for a generic route only when the configured
proxy itself guarantees one exact Anthropic cache mode. For example, a Claude
Sonnet 4.6 route with a proxy-enforced five-minute breakpoint can pair
`est_uncached_input_cost_1m_usd: "3"`,
`est_cached_input_cost_1m_usd: "0.30"`, and
`est_cache_write_input_cost_1m_usd: "3.75"` with the 300-second policy above;
a separately named proxy route that guarantees one-hour breakpoints uses
`"6"` as its write price and 3,600 seconds. These are declaration examples,
not built-in dispatch support. Tau has no native Anthropic backend and sends no
Anthropic pre-warm, refresh, or nested generic request controls. In particular,
Tau never turns the five-minute TTL into a roughly four-minute cadence and
never schedules traffic during unknown or unbounded idle.

Gemini's [explicit context caching](https://ai.google.dev/gemini-api/docs/caching)
stores a named provider object. Creation establishes an absolute expiry; the
provider can PATCH its `ttl` or expiry, and deletion is a separate provider
operation. A generic route that consumes an already-created object can describe
that provider contract without claiming Tau implements it:

```yaml
cache_contract:
  kind: explicit_object
  ttl:
    kind: fixed
    seconds: 3600
  renewal: patch_expiry
  output_floor: zero
  quota:
    requests: unknown
    read_tokens: unknown
    write_tokens: unknown
    output_tokens: unknown
  privacy:
    storage: named_provider_object
    zero_data_retention: incompatible
    data_residency: provider_specific
    manual_deletion: unavailable
```

`patch_expiry` records Gemini's documented object mechanism, not a Tau
operation. `manual_deletion: unavailable` likewise reports Tau's typed
capability: Gemini can delete the object, but no current Tau backend can create,
PATCH, or delete it. The object extends provider retention, is incompatible
with zero-data-retention operation, and has service/surface-specific residency.
The fixed TTL describes the configured object expiry, not a read-refreshed
deadline.

Generic Chat Completions profiles retain `extra_body` for non-conflicting
provider-specific request members, including an opaque reference to an object
an operator manages outside Tau. Tau preserves that escape hatch but does not
model a separate cache-object identity, lifecycle, or accounting state in
runtime metadata or journals. It preserves the opaque configured request member
in the profile and clones it into each attempt. Operators must account for the
object's lifetime and lifecycle externally. The normal raw cache-price fields
can state the exact route's token-hour storage rate; for example, Gemini 2.5
Flash's listed rate is `$1` per million token-hours. Tau adds a storage estimate
only when a backend reports `storage_token_micros`; generic Gemini-compatible
routes do not parse or infer that usage from an object reference.

Gemini implicit caching is a separate automatic-prefix optimization. A generic
route may declare it only as `kind: automatic_prefix`, unknown residency,
unsupported renewal, and unknown output floor unless its exact compatibility
surface documents more. Tau's support is limited to keeping compatible request
prefixes stable through its normal deterministic request lowering. It sends no
keepalive, prewarm, cache-object, or lifecycle traffic, and a hit never proves
a TTL or permits a renewal schedule.

OpenAI's
[prompt-caching documentation](https://developers.openai.com/api/docs/guides/prompt-caching)
defines different contracts for GPT-5.6-and-later and older models. An exact
GPT-5.6 generic route that opts into Tau's typed OpenAI cache controls may
declare `kind: explicit_breakpoint`, `ttl: { kind: sliding_known, seconds: 1800 }`,
`renewal: read`, and `output_floor: unknown`. Cache creation and each successful
cache reuse start a fresh 30-minute window. This is a provider-owned sliding
lifetime, not Tau keepalive traffic: Tau sends no standalone cache refresh
unless the separately disabled-by-default refresh scheduler is explicitly
configured and admits one under its own policy. Ordinary read/write observations
still do not establish this contract for another route.

The same exact GPT-5.6 route may publish OpenAI's explicit ordinary-input,
cached-read, and cache-write prices. For example, the short-context
`gpt-5.6-sol` comparison rates are `$5`, `$0.50`, and `$6.25` per million
tokens. The basic model fields do not represent OpenAI's long-context tier, so
cost comparisons using them must exclude that tier; the central fallback is
not a provider fact. Select `compat.cache_usage: open_ai` on Chat Completions
only when the endpoint uses OpenAI's documented usage shape. Public Responses
already parses `cached_tokens` and
`cache_write_tokens`. Those counters measure an ordinary request; they do not
prove expiry or renewal.

Models before GPT-5.6 require a separate declaration. Their documented typical
in-memory eviction after 5–10 minutes and possible retention for up to one hour
are not a guaranteed TTL, so a conservative automatic-prefix declaration uses
`ttl: { kind: unknown }`, `renewal: unsupported`, and `output_floor: unknown`.
The retired `prompt_cache_retention` request control selected a legacy OpenAI
retention policy; it never turned typical behavior or a maximum into a minimum,
fixed, or sliding policy fact. Its former `24h` value is not equivalent to the
new public cache `ttl: 30m` contract.

Tau's cache-refresh scheduler is disabled by default. Opt in globally:

```yaml
provider_cache_refresh:
  enabled: true
  max_idle_seconds: 300
```

`max_idle_seconds` accepts `1..=86400`; the default is 300. The scheduler uses
only exact routes with sliding read renewal, zero output, volatile ZDR-compatible
storage that preserves route residency, concrete quota classes, and explicit
ordinary/read/write prices. It requires both a reported write and the
price-derived number of later reads before one refresh is economical. It resends
the exact full successful prompt prefix—including prior user/tool context and
tool schemas—through the existing non-generating prewarm operation. This
sensitive resend is why opt-in is explicit.

Refreshes currently run only during a finite foreground tool-batch window,
below real prompts, with two global and one-per-Provider slots. There is no
deadline-bearing approval operation today, so approval waits do not admit work.
A real prompt,
cooldown, exact idle/residency deadline, route or prefix change, shutdown, or
rotation suppresses or cancels work. The Provider reports a correlated terminal;
cancel delivery alone does not release scheduler capacity. Each qualifying read
creates a new observation generation, and each generation authorizes at most one
attempt; failure never creates a prompt retry. Keys, evidence, jitter, and
lifecycle state are process-only and never journaled or restored.

Hardcoded ChatGPT/Codex comparison prices come from OpenAI's provider-owned
[API pricing table](https://developers.openai.com/api/docs/pricing). Astra
publishes its standard short-context ordinary-input, cached-read, cache-write,
and output rates. These are API-equivalent estimates, not private subscription
billing; they exclude Astra's long-context and service-tier variants. Other
private ChatGPT models continue to omit cache rates. Configured compatible
providers own their explicit values; refresh those profile fields from that
provider's basic public pricing table. The built-in Chat Completions
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

### Chat Completions compatibility

`compat` may appear at provider or model level. When a model has `compat`, that
object fully replaces the provider object; fields do not merge. A configured
`reasoning_effort.mapping` requires a non-empty sequence of strictly increasing
portable cut points and native effort values. The first cut point must be `0.0`.
Cut points at or below `0.5` belong to the higher band that starts there; cut
points above `0.5` belong to the preceding lower band. Thus a standard medium
range is `[0.35, 0.65]`, while providers remain free to configure asymmetric
cuts. Its wire policy is:

- `open_ai`: publish and send the distinct `none` through `high` spellings;
- `literal`: preserve extended `xhigh`/`max` instead of folding them to `high`
  while retaining the shared provider spellings;
- `omit`: publish one fixed configured effort but send no top-level field.

Omitting `reasoning_effort` publishes unsupported control and sends no field. Use
`omit` only for one fixed server-side effort, such as
`mapping: [{"from": "0.0", "level": "xhigh"}]`; publishing several values would
imply a choice the wire cannot convey.

Provider profiles using the former `efforts: ["low", ...]` shorthand must migrate
to explicit bands. For example:

```json
"reasoning_effort": {
  "mapping": [
    {"from": "0.0", "level": "low"},
    {"from": "0.35", "level": "medium"},
    {"from": "0.65", "level": "high"}
  ],
  "wire": "literal"
}
```

Put this object in provider-level `compat` to define the provider default, or in
one model's `compat` to replace that complete provider compatibility object for
the exact model.
`reasoning_replay` selects `reasoning_content`, `reasoning`, or `both` for
assistant history. The default is `reasoning_content`.
`single_initial_system_message: true` retains Tau's leading system prompt but
rejects any later System or Developer transcript message before dispatch.
`tool_choice: false` omits that optional selector. Auto still sends configured
Function definitions and relies on the endpoint default; None omits the
definitions too, so disabling tool calls remains effective. It defaults to
`true` for ordinary configured Chat Completions routes. Models default
`supported_tool_types` to `["function"]`; set it to `[]` only for a route that
has no native Function interface, and set `supports_parallel_tool_calls: false`
with it.

### Qwen3.8 text-only local profile

Use a dedicated Chat Completions profile for `Qwen/Qwen3.8-27B`. Set
`context_window` to the server's actual `--max-model-len`, not the model's
262,144-token architectural maximum unless the server really reserves it. This
text-first profile intentionally does not advertise image input:

```json
{
  "kind": "chat_completions",
  "base_url": "http://127.0.0.1:8000/v1",
  "credential": {"kind": "none"},
  "extra_body": {
    "chat_template_kwargs": {
      "enable_thinking": true,
      "preserve_thinking": true
    },
    "temperature": 1.0,
    "top_p": 0.95,
    "top_k": 20,
    "min_p": 0.0,
    "presence_penalty": 0.0,
    "repetition_penalty": 1.0
  },
  "models": [{
    "id": "Qwen/Qwen3.8-27B",
    "context_window": 65536,
    "compat": {
      "stream_options": true,
      "reasoning_effort": {
        "mapping": [
          {"from": "0.0", "level": "low"},
          {"from": "0.35", "level": "medium"},
          {"from": "0.8", "level": "xhigh"}
        ],
        "wire": "literal"
      },
      "reasoning_replay": "both",
      "single_initial_system_message": true
    }
  }]
}
```

Select a numeric role intensity whose published mapping resolves to `xhigh` for
Qwen's recommended/default thinking mode. The adapter sends literal `low`,
`medium`, or `xhigh` only when the profile publishes them. Some local servers,
including current llama.cpp releases, do not accept the top-level
`reasoning_effort` extension. For those servers, configure
`"reasoning_effort": {"mapping": [{"from": "0.0", "level": "xhigh"}],
"wire": "omit"}` so Tau publishes fixed `xhigh` behavior while relying on Qwen's
default template behavior.
Keep `preserve_thinking: true` and `reasoning_replay: "both"` for tool
continuations.

For non-thinking, use a separate profile with no `reasoning_effort` block and
set `chat_template_kwargs.enable_thinking` to `false`. Use Qwen's fixed
non-thinking sampling values (`temperature: 0.7`, `top_p: 0.8`, `top_k: 20`,
`min_p: 0.0`, `presence_penalty: 1.5`, `repetition_penalty: 1.0`) rather than
changing them dynamically between turns.

Start conservatively with
`TAU_BUILTIN_PROVIDER_PROMPT_CONCURRENCY=1`. Increase concurrency only after the
server's batching, KV-cache allocation, and memory headroom are measured under
the configured context limit. Tau supports Qwen's streamed reasoning, visible
text, single and parallel function calls, raw JSON argument replay, terminal
usage-only chunks, and continuation. Qwen's template does not support later
system or Developer messages, so this profile rejects those shapes before
dispatch while retaining the one initial system prompt.

Chat Completions profiles parse provider-specific cache counters only when
`compat.cache_usage` explicitly selects `open_ai` or `deep_seek`; omission
ignores cache-only fields while retaining ordinary input/output usage. Public
Responses and private ChatGPT routes use their native OpenAI response shape.
Anthropic/Gemini compatibility routes remain best-effort and expose no native
cache parsing or object lifecycle.

Private ChatGPT cached-input counts remain provider-reported usage. Tau no
longer synthesizes an exact cache-read ceiling for Sol, Terra, or Luna because
the backend's observed cache boundaries no longer match the former fixed
geometry. When no exact ceiling is available, the CLI marks cache efficiency
with `?`. On consecutive supported private Responses WebSocket turns, the CLI
can passively learn one of the observed 128-token or shifted 1,024-token
reported-read regimes after two consecutive matching predecessor/read
observations, then apply that provisional geometry to the following turn. It
requires unchanged typed model controls and route/model continuity, stays
within the prefix sizes covered by the observations, and falls back to the
generic estimate when evidence is missing, ambiguous, stale, or changes regime.
The current read never becomes its own calibrated denominator, and the estimate
does not alter provider counters or publish an exact ceiling.

DeepSeek Chat Completions routes must explicitly select both
`compat.stream_options: true` and `compat.cache_usage: deep_seek`. The first
uses the existing `stream_options.include_usage` request member; the second is
the sole authority for parsing `prompt_cache_hit_tokens` and
`prompt_cache_miss_tokens`. Those counters are response-local telemetry with
probabilistic residency, not a cache-policy declaration or TTL evidence.

OpenRouter profiles and discovered models select streamed OpenAI-compatible
cache read/write telemetry by default. Tau publishes no OpenRouter cache policy,
does not send cache controls, and drops configured generic cache contracts on
that route: OpenRouter can choose a different upstream provider, so a response
cannot establish cache mechanism, privacy, residency, renewal, or lifecycle.
Its observations consequently retain unknown expiry confidence. Neither
DeepSeek nor OpenRouter observations schedule a keepalive. Any future
latency-oriented keepalive needs a separate explicit operator budget with
privacy and quota visibility; it cannot infer a cadence from eviction or a
recent cache hit.

OpenRouter discovery also treats `supported_parameters` as exact, independent
route metadata. `tools` publishes Function support, `tool_choice` controls
whether Tau sends that selector, and `parallel_tool_calls` enables parallel
requests only when `tools` is also present. With tools but no `tool_choice`, Auto
sends definitions and relies on OpenRouter's documented default; None omits both
definitions and selector. Missing, null, and empty metadata grant no capability.
Tau leaves OpenRouter's `provider.require_parameters` at its default so unrelated
optional request fields do not unnecessarily remove endpoints.

Generic OpenAI-compatible routes may opt into typed cache request controls only
when the exact configured route supports them. Tau never infers these controls
from a provider name, model name, base URL, or OpenRouter route. `extra_body`
cannot supply `prompt_cache_key`, retired `prompt_cache_retention`, or
`prompt_cache_options`; typed compatibility owns those top-level fields.

Chat Completions provider or model compatibility selects cache mode and lifetime
independently:

```yaml
compat:
  openai_prompt_cache:
    key: agent
    options:
      mode: implicit
      ttl: 30m
```

```yaml
compat:
  openai_prompt_cache:
    key: agent
    options:
      mode: explicit
      ttl: 30m
      boundary: system_prompt
```

`key: agent` derives Tau's stable `tau:<agent-id>` key; profiles cannot select
an arbitrary shared key. Implicit mode sends
`prompt_cache_options: { mode: implicit, ttl: 30m }` and no content marker, so
the provider selects any breakpoint. It deliberately accepts that the provider
can choose a volatile suffix and, where the route prices cache writes
separately, can charge a write premium. Explicit mode instead marks the end of
the non-empty system prompt and sends `mode: explicit`, so Tau does not create
an implicit suffix breakpoint. `boundary` is forbidden in implicit mode and
required in explicit mode. The former `retention: in_memory` / `"24h"` profile
control is rejected with migration guidance: legacy
`prompt_cache_retention: "24h"` and the new `ttl: 30m` are different provider
contracts and Tau deliberately does not translate one into the other. The
retired `compat.prompt_cache_key: bool` is invalid.

Public Responses profiles accept the same independent controls at provider or
model `compat`:

```yaml
compat:
  openai_prompt_cache:
    key: agent
    options:
      mode: explicit
      ttl: 30m
      boundary: first_input_text
```

Public Responses also accepts implicit
`options: { mode: implicit, ttl: 30m }` without a boundary or marker. Explicit
mode leaves top-level `instructions` unchanged and marks the earliest
Tau-constructed non-assistant `input_text` block. It is per-agent, multi-turn
cost control, not a system-prompt boundary or cross-agent reuse. A request
without that block fails locally rather than sending explicit options without a
marker. HTTP/SSE and WebSocket serialize the same cache fields; neither public
route emits legacy `prompt_cache_retention`.

### Scoped provider credentials

`tau provider add` defaults to mutable state under the selected enabled built-in
provider extension instance. `--config` writes the credential-free JSON to XDG
config instead, and `--config --output -` prints canonical JSON for redirecting
into dotfiles while publishing any required host-local Secret record:

```text
$XDG_CONFIG_HOME/tau/providers/<extension>/<provider>.json
$XDG_STATE_HOME/tau/providers/<extension>/<provider>.json
$XDG_STATE_HOME/tau/secrets/ext/<extension>/providers/<credential-id>/{oauth,api-key}.json  # authenticated only
```

Config and state names form a disjoint union. A duplicate fails startup even when
both files are identical; neither source overrides the other. Config symlinks and
read-only Nix store files are supported. `tau provider list` reports `config` or
`state`, `show` prints the credential-free JSON and source path, and `remove`
infers a unique source or accepts `--config`/`--state`. ChatGPT rows whose OAuth
credential is absent or expired include the exact `tau provider login` command
needed to repair that profile.

Harness `aliases.providers` entries only rewrite the provider component of
static configured role models. For example, `subscription: codex-work` makes
`subscription/gpt-5.5` route as `codex-work/gpt-5.5`; it does not rename, copy,
load, or redirect provider profile files, credentials, extension instances, or
`tau provider` command arguments. The canonical target profile must still exist,
authenticate, and publish the selected model.

`tau provider login <profile>` hydrates or refreshes an existing profile without
changing its config- or state-owned settings. It publishes only the host-local
typed Secret record, revalidates the exact profile source and bytes before the
write, never replaces a config symlink, and never creates a shadow state profile.
Use `tau provider --extension <instance> login <profile>` for a renamed built-in
provider instance. Bare `tau provider add chatgpt` offers this login path when the
default config-owned `chatgpt` profile needs authentication; noninteractive use
prints the exact command instead of starting OAuth.

`tau provider rename <old> <new>` renames only the unique config- or
state-owned profile filename. It does not parse or rewrite profile bytes, move
credentials, or modify `harness.yaml`; the profile's stable opaque credential
identity keeps the existing host-local credential usable after the rename.
The command rejects a missing, duplicated, or colliding profile name before
renaming anything.

Settings contain backend and model metadata plus either a stable opaque
credential identity and a closed credential-slot kind or, for supported local-compatible profiles, the exact
explicit marker `"credential": {"kind": "none"}`. They never contain OAuth
tokens or API keys. The keyless marker performs no Secret lookup and makes the
profile fully portable. Omitting `credential`, adding fields to the keyless
object, or using keyless mode for a provider kind that requires authentication
remains invalid; losing a referenced Secret therefore never turns an
authenticated profile into unauthenticated network requests. The runtime loads
referenced version-zero credentials before model publication and at prompt
boundaries. Prompt and due-retry reads run asynchronously with a 30-second
Secret-response deadline. Ready prompts retain accepted order while reads or
OAuth refreshes finish out of order; cancellation removes only the affected
prompt, and late results have no runtime authority. Credential rotation
therefore takes effect without restart;
settings changes require a full harness restart. Provider-process restart does
not reload either directory, and Tau creates no imported copy or watcher.
Missing or malformed referenced credentials exclude that provider. If prompt-time
hydration positively finds a ChatGPT OAuth Secret absent, the initiating retry
status names the provider and prints `tau provider login <profile>`; it does not
print the Secret path or storage error. Other hydration failures keep the generic
authentication/configuration retry status.
Initial Configure validates the complete bounded settings snapshot before
retaining it or publishing any model. One invalid filename or profile—including
legacy `api_key_secret`, inline API keys, or mixed credential fields—rejects the
whole snapshot, publishes no models or Ready, and produces one mandatory,
replayable, redacted configuration warning. Re-register the invalid profile;
startup does not rewrite or migrate persisted settings.

The old `$XDG_STATE_HOME/tau/provider-settings/` location is not inspected at all.
Move only its credential-free JSON manually into one new `providers/` location;
leave the existing Secret records in place.

`tau provider add [KIND]` accepts exactly `chatgpt`, `chat-completions`,
`responses`, or `openrouter`; without `KIND` it presents those same choices in
a picker. API-key profiles explicitly select direct masked entry, a named
secret, or (only for keyless/local-compatible backends) no key. Existing
configured names resolve eagerly. The named-secret picker separately offers
`Enter secret name for deferred binding…`; this intentional deferred path accepts
any valid source name, including an existing declaration, and writes the
credential-free profile without a Secret record. This supports deploying the
declaration and value later through Nix. The profile stays disabled until a
persistent restart sees the exact authorized declaration and value and
materializes the canonical typed record. An unavailable ordinary existing
selection still fails setup rather than silently becoming deferred. A later
unavailable restart invalidates
its old materialization, omits the profile, and publishes a source-name-only
warning. A bound declaration is
consumed for materialization and is not copied into `Configure.secrets`.
Provider setup/login/removal and startup serialize per configured instance; the exact
startup settings snapshot that selects the source also becomes the immutable
Configure snapshot.
Keyless setup writes only the explicit portable profile. It does not create an
empty or dummy API-key record.

Tau enables its summary compaction fallback for Chat Completions, OpenRouter,
and public Responses models whose configured context window is nonzero. Omit
`local_summary_compaction` to use the generic fallback: no historical-prefix
byte cap, an output-token cap of `clamp(context_window / 8, 1, 4096)`, and a
256 KiB output-byte bound. The fallback publishes no proactive threshold.

An explicit object supplies independent optional overrides. An empty object is
equivalent to omission:

```json
{
  "id": "local-model",
  "context_window": 32768,
  "local_summary_compaction": {
    "max_input_bytes": 16384,
    "max_output_tokens": 1024
  }
}
```

`max_input_bytes` bounds the canonical JSON-serialized historical prompt prefix.
`max_output_tokens` controls the summary request. `max_output_bytes` independently
bounds accepted narrative and reasoning output and defaults to 256 KiB. Explicit
values must be positive; output tokens cannot exceed the model `context_window`,
and output bytes cannot exceed 256 KiB.

The old duplicate context and serialization-selector fields are rejected with
migration guidance:

```text
remove obsolete local_summary_compaction.context_window_tokens; model context_window is used
remove obsolete local_summary_compaction.serialization_profile
```

Remove those keys and keep any of the three limit keys that differ from the
generic fallback. Provider settings changes still require a Tau restart.

Tau lowers the selected immutable cut exactly like ordinary inference: the
same system prompt, tools, ordered history, images, raw tool-call arguments,
route/model fields, and cache controls. It appends one harness-authored
`<tau_internal>` user message last. This preserves eligibility for provider
prefix-cache reuse; actual cache hits remain provider-controlled. ChatGPT/Codex
models continue to prefer their unchanged provider-native compaction.

Any returned tool call rejects compaction and executes nothing. Tau accepts
exactly one nonempty bounded assistant final text, discards separately bounded
reasoning and opaque replay items, and rejects every other semantic item. The
harness stores the exact final text once as one synthetic user-role checkpoint,
without a wrapper or deterministic supplement. Events after the immutable cut
remain suffix history, and live/cold replay reuse the committed checkpoint
without another model call. Ordinary default-on durable provider debug capture applies.
Tau does not infer token fit from byte limits. The object controls only its
declared resource/output bounds; without an exact token threshold it does not
proactively schedule. Compaction does not rewrite the ordinary input prefix.
Empty, unsupported,
truncated, or over-limit summaries fail the durable transaction without fallback
or resend.

`tau-provider-responses` implements the public `/responses` protocol over
API-key HTTP/SSE or WebSocket for the `responses` profile. The
`tau provider add` picker labels these kinds `OpenAI-compatible Chat
Completions` and `OpenAI Responses API`.
Responses profiles require a base URL, explicit models, and a `transport` value
spelled `sse` or `websocket`; omitted values from older profiles mean `sse`.
For API-key profiles, the wizard first selects `Enter API key now`, `Use named
secret`, or `No API key` where keyless operation is supported. Existing
declarations appear before `Enter secret name for deferred binding…`; selecting
that separate choice accepts any valid name, including one already declared.
With none configured, Tau opens that explicit deferred-name prompt directly.
Only direct entry opens the masked value prompt. It asks for transport after the
endpoint, API-key authority, and models. It
preselects WebSocket only for the exact official
`https://api.openai.com/v1` base URL and otherwise preselects SSE. Tau does not
infer endpoint support at runtime or discover models. Every turn sends the complete typed
Responses transcript. It supports assistant text, completed reasoning items,
and Function tools. Plain `reasoning_text` uses the existing `show-thinking` UI
behavior. Opaque, summary-only, and encrypted reasoning is retained without a
display projection. The complete validated reasoning item is replayed verbatim;
malformed reasoning remains unsupported.
The backend preserves assistant-message, reasoning-item, and Function-call
replay sidecars. It deliberately omits `previous_response_id`, `store`,
hosted/custom tools, image/file inputs, and public compaction. Existing
`openrouter` profiles remain Chat Completions profiles.

WebSocket mode opens a fresh connection for each finite attempt and sends one
`response.create` envelope without SSE-only fields. A retry reconnects and
replays the complete local transcript. It never continues from a response ID
whose connection-local cache may have disappeared, never silently switches to
SSE, and never targets the distinct OpenAI Realtime protocol.

Both transports classify only canonical nested
`response.incomplete_details.reason: "max_output_tokens"` as an output-length
terminal. Tau preserves validated partial prose, reasoning, usage, and the
nested response id without retrying the unchanged request. It never executes a
Length-truncated Function call. Only replay-safe plain reasoning without prose
or calls can use the existing one bounded continuation. During standalone
summary compaction, the partial response remains durable non-context accounting
data; Tau never installs it as the replacement window or retries it
automatically. Other incomplete reasons remain provider failures.

Each `responses.models[]` entry may set `reasoning_effort.mapping` to describe the
exact reasoning-effort levels and portable cut points its upstream model accepts:

```json
{
  "id": "quirky-model",
  "reasoning_effort": {
    "mapping": [
      {"from": "0.0", "level": "none"},
      {"from": "0.2", "level": "low"},
      {"from": "0.35", "level": "medium"},
      {"from": "0.65", "level": "high"}
    ]
  }
}
```

Omitting `reasoning_effort` publishes the standard full mapping for
`[none, minimal, low, medium, high, xhigh, max]`. An explicit
`reasoning_effort: {"mapping": []}` publishes no reasoning-effort capability.
Existing profiles using `efforts: [...]` must replace that field with the
explicit mapping object shown above. Non-empty mappings must start at `0.0` and
strictly increase both cut points and native levels; every later cut point must
remain below `1.0`.
The harness clamps numeric portable intent only while mapping each prompt to the
published native levels. Disabled selects `none` when available, or the minimum
native level otherwise. Provider-default intent omits the effort selector.
`tau provider add` intentionally omits this field, so new
profiles receive the full default; edit the profile only when a model needs an
override.

`tau-provider-codex` implements the private ChatGPT OAuth/Codex Responses
contract. Ordinary inference is WebSocket-only: it has no HTTP/SSE selector or
fallback. HTTPS remains in that backend for OAuth and `/wham/usage`. It supports
Standard Responses by default and
explicit Lite compatibility, Function and Custom tools, response-id chaining,
prompt caching, pool/prewarm reuse, opaque replay items, and provider-owned
reasoning state. It is not a public API-key OpenAI Responses client.

`agents.web_tools` is inherited through agent defaults, role groups, roles, and
selected profiles. Named candidates merge by name and select once, in
`(priority, name)` order, when a prompt is materialized. Capable ChatGPT/Codex
Standard Responses routes default to cached hosted `web_search`; Lite and exact
routes without that capability select `websearch_hybrid_search`.
`websearch_hybrid_fetch` remains external.

`access: cached` means provider index/cache access, not local, offline, private,
or free search. `access: live` permits current external pages.
`context_size: low|medium|high` is qualitative; `null` uses the provider
default. Provider transport retries retain the selected implementation and may
repeat a paid hosted search after ambiguous failure.

The complete role-policy shape is:

```yaml
agents:
  web_tools:
    # absent inherits; null removes an inherited restriction; [] denies all web
    allowed_domains: null
    search:
      unavailable: omit # or error before provider delivery
      candidates:
        native:
          enable: true
          priority: 10
          kind: model_provider
          access: cached # or live
          context_size: null # provider default, low, medium, or high
        external:
          enable: true
          priority: 20
          kind: tool
          tool: websearch_hybrid_search
    fetch:
      unavailable: omit
      candidates:
        external:
          enable: true
          priority: 20
          kind: tool
          tool: websearch_hybrid_fetch
```

Candidate maps merge by name and candidate fields merge individually. `enable:
false` disables an inherited candidate. Tool references must be syntactically
valid, but registration, route support, role authorization, expected aliases,
and enforcement metadata are checked when each prompt is materialized.
`unavailable: omit` exposes nothing when every candidate is ineligible;
`unavailable: error` rejects that prompt before provider delivery. Empty
candidate maps and malformed or contradictory candidate fields reject
configuration.

`allowed_domains` uses lowercase DNS names and includes exact hosts plus their
subdomains. It is not query steering or result post-filtering. Hosted search
uses provider-side filters only on routes advertising that control. Ordinary
search requires an adapter that advertises provider-side per-call enforcement.
The default Exa/Parallel/You pool does not, so restricted external search is
unavailable unless configured Tavily or Firecrawl is present. External fetch
gates only its requested target before extractor contact; redirects and
subresources are outside this control. See
[Security](../SECURITY.md) and the
[websearch extension](../crates/tau-ext-websearch/README.md).

```yaml
profiles:
  live-research:
    agents:
      web_tools:
        search:
          candidates:
            native: { access: live, context_size: high }
  external-only:
    agents:
      web_tools:
        search:
          candidates:
            native: { enable: false }
```

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
and merge sparse in-band WebSocket `codex.rate_limits` observations. Quota
telemetry is best-effort, starts after provider `Ready` in incremental
background rounds, and never delays inference or consumes prompt retry budget.
The compact status
chip is shown for a selected model when its provider publishes quota current
state. Tau uses neutral `Q?` when weekly state is absent, unbound, stale, expired,
or timing-untrusted. It never guesses a colored claim from a default or sole
account pool: colored state requires a fresh in-band binding of the exact
`ModelId` and trustworthy weekly timing, and expires rather than locally resetting
when the server reset boundary passes. After quota state is cleared, an empty
capability snapshot remains replayable for the running harness so both live and
late clients show neutral unknown.

Required inference work has no attempt-count or elapsed-time retry limit during
the running session. Standalone compaction is bounded to five total attempts:
transient failures use the same scheduler and shared cooldown, then the fifth
failure terminalizes without a sixth provider attempt. Transport/server
failures, throttling, usage windows, billing/quota/credits, reloadable
auth/configuration, and unknown remote inference failures remain pending until
success or cancellation. A narrowly proven deterministic unchanged-request
failure closes immediately for either operation.

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
A canonical provider HTTP 401 overrides local expiry once for the exact
credential generation. Tau reloads and, if needed, forces one refresh; it does
not automatically replay the prompt with the same rejected access token after
that recovery fails. Refresh accepts omitted token replacements, derives any
replacement access-token expiry from its JWT `exp`, and publishes or adopts a
CAS winner only when its ChatGPT account identity matches the pinned account.
Missing or inconsistent identity fails closed.
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
context management. Manual and threshold-driven compaction use a fresh ordinary
Responses WebSocket request with the full window and a final
`compaction_trigger`. A successful response installs only the one validated
opaque provider compaction item; Tau does not copy items from the compacted
prefix back into the replacement. The harness preserves the exact ordered
post-cut suffix, including facts accepted while compaction runs. A
route/account rejection removes capability for that credential generation;
rotation permits one fresh serialized probe.
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
