# Chat Completions transport boundary

Chat Completions sends prompts, tool definitions, credentials, and conversation
context to configured external endpoints. Every attempt receives the built-in
provider process's immutable `OutboundNetworkPolicy`; reqwest environment proxy
discovery and redirects remain disabled. A selected proxy is the only route
for that attempt and failure never falls back direct.

Target and HTTPS-proxy TLS use platform verification plus the optional
startup-read custom CA. Plain HTTP endpoints expose headers and content to a
selected proxy. Cancellation drops the active async response future/socket;
header, body, and idle waits retain finite bounds. Error bodies remain bounded
and provider content must never enter retry status, ordinary logs, or watcher
events.

Explicit durable-session debug captures can contain complete prompts, tool
results, model output, and bounded HTTP error bodies. They remain private
sensitive `.json.zst` diagnostics; compression is not redaction. The shared
bounded writer can omit them on overload, failure, or process exit and never
blocks provider work to guarantee persistence.
Tau does not intentionally include auth headers or API-key configuration, but
provider-controlled responses/errors and configured request fields can reflect
credentials, so every capture must be treated as potentially credential-bearing.

## Typed OpenAI prompt-cache controls

Only an operator-declared exact route may send `compat.openai_prompt_cache`.
Tau sends its stable `tau:<agent-id>` key to that configured external provider;
the key is a provider-visible correlation value and profiles cannot select an
arbitrary shared key. Every selected route sends independent mode and 30-minute
TTL options. Implicit mode supplies no content marker and accepts
provider-selected automatic caching, including any retention posture and
volatile-suffix cache-write premium.

Explicit mode requires a non-empty system prompt and marks the end of that
system text, leaving conversation and tool suffixes unmarked. The retired legacy
`prompt_cache_retention` control is rejected rather than translating former
`24h` retention into the distinct `30m` TTL. `extra_body` cannot override the
key, retired retention, or options fields. Standalone summary compaction
preserves the same cache controls as ordinary inference so its unchanged prefix
remains cache-eligible.

## Local summary compactor boundary

The built-in provider supplies a generic local-summary fallback with no prefix
byte cap or proactive threshold and independent token-output/narrative-byte
limits; an explicit `local_summary_compaction` profile may override those
native-domain limits. The adapter lowers the immutable cut exactly like ordinary
inference, retaining the ordinary system prompt, tools, typed history, images,
raw tool arguments, route/model fields, and cache controls, then appends the
fixed harness-authored `<tau_internal>` user instruction last. This is deliberate
cache alignment, not an isolated authority or privacy boundary.

Any returned tool call rejects the attempt and executes nothing. The extension
accepts exactly one nonempty bounded assistant final text, discards independently
bounded reasoning, and rejects every other semantic item. The adapter rejects a
delta before appending it when that semantic channel would cross its selected
limit, then rechecks the completed projection. The harness stores the exact text
once as one synthetic user-role checkpoint without a wrapper or supplement.
Ordinary opted-in provider debug capture applies and remains non-semantic,
sensitive observability data. Revisit prefix identity, output validation,
non-execution, bounds, capture, retries, suffix preservation, and replay identity
when changing this boundary.
