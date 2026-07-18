# ARCH-tau-provider-chatgpt: tau-provider-chatgpt architecture

Provider output is constrained by
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).
Curated provider wire evidence follows
[SPEC-tau-provider-chatgpt-curated-vcr](SPEC-tau-provider-chatgpt-curated-vcr.md);
its rationale and evidence boundary are recorded in
[DECISION-tau-provider-chatgpt-curated-vcr](DECISION-tau-provider-chatgpt-curated-vcr.md).

## Account quota telemetry

This adapter owns the isolated ChatGPT `/wham/usage`, HTTP quota-header, and
WebSocket `codex.rate_limits` contracts. It normalizes only bounded pool/window
facts and preserves independent usage and timing observations; credentials,
account ids, credits, and provider prose never leave the in-process provider
boundary. Applicability and UI pacing are owned above this crate as specified
by [SPEC-provider-quota-pacing](../../../specs/SPEC-provider-quota-pacing.md).
The official WebSocket contract assigns a valid nameless `codex.rate_limits`
turn event to the canonical default `codex` pool. Explicit valid
`metered_limit_name` or legacy `limit_name` values take precedence, while a
JSON null is treated as absence. Every present non-null pool field must
normalize successfully, including a lower-precedence legacy field, or the
observation is rejected rather than falling back.

## Typed image tool output

GPT-5.6 Sol, Terra, and Luna on the ChatGPT Responses surface explicitly
publish image input and image tool-result support. Successful typed function
results lower to one `function_call_output` whose `output` array contains the
normalized `input_text` followed by `input_image` data URLs. Canonical binary
bytes remain in Tau; base64 exists only in the outgoing request. Responses Lite
omits `detail` after local high-detail preparation. Other model/routes project a
bounded omission marker and never receive bytes.

Normal inference, WebSocket, replay, and standalone compaction share this item
converter. Each request admits at most 24 MiB of canonical image bytes and 32
MiB of image-attributable data URLs. Debug request files and VCR matching
fixtures replace data URLs with metadata before persistence.

## Transport selection

`ResponsesConfig::supports_websocket` is the source of truth for ChatGPT/Codex Responses
transport routing. When it is true, Tau treats WebSocket as the required transport for
that model/configuration, not as a speculative optimization before HTTP/SSE. Capability
or limit failures from the WebSocket path and retryable WebSocket failures must surface
to the outer logical-prompt scheduler rather than silently replaying the same prompt
over HTTP/SSE.

HTTP/SSE remains the Responses transport for configs that do not advertise WebSocket
support and for the HTTP/SSE-specific request/debug/replay paths.

A fresh WebSocket path emits one fixed, content-free connecting status and owns
its same-key pool reservation through connection setup. Cancellation and
deadline behavior is specified by
[SPEC-tau-provider-chatgpt-cancellation](SPEC-tau-provider-chatgpt-cancellation.md).

Best-effort WebSocket prewarm uses the same shared pool and cooperative abort
seam from a provider-supervised worker, never the provider event loop. It skips
an already-reserved same-key socket. Profile and session invalidation is shared
with normal transport pool ownership.

## GPT-5.6 Responses modes

The surface choice follows
[DECISION-tau-provider-chatgpt-responses-surface-selection](DECISION-tau-provider-chatgpt-responses-surface-selection.md).
Each profile captures an explicit mode at startup. Standard mode uses top-level
instructions/tools, requests parallel tool calls, omits forced all-turn reasoning
context, preserves image detail, and carries no Lite marker. Lite compatibility
moves declarations and instructions into developer input items, requests serial
tool calls, forces all-turn reasoning context, omits image detail, and carries the
HTTP header or per-request WebSocket metadata marker. Retries, reconnect, replay,
and previous-response chaining retain the selected mode; there is no mode fallback.

Both modes suppress legacy inline `context_management` for GPT-5.6 and advertise
standalone compaction. The provider sends unary HTTP
`POST /codex/responses/compact` using the selected mode's lowering and marker, and
the harness installs its output as one replacement-window boundary. Older models
retain inline context management and ignore the profile's Lite compatibility flag.
Hosted Responses tools are not part of either contract; Tau's tools remain
client-executed definitions.

ChatGPT model metadata distinguishes the raw provider context window from the
effective window published to the harness. Standalone compaction thresholds
derive from the raw window, while UI usage and local context limits use the
provider's 95-percent effective ceiling.

## Prompt-cache identity

First-party ChatGPT/Codex prompt-cache keys are stable per provider base URL, startup-selected Responses mode, and durable
target `AgentId`. Prompt provenance (`PromptOriginator`) is intentionally not part of
the key: a target agent must stay on the same provider cache bucket whether a turn came
from direct user input, extension-originated work, a manager relay, or an agent-to-agent
message.

The deprecated `share_user_cache_key` prompt flag is a no-op for first-party
cache-bucket selection. Any future cache-sharing behavior should be explicit
agent metadata (for example, a reviewed `share_cache_from` design) rather than
inferring cache identity from prompt provenance.

WebSocket pool keys follow the same identity as request `prompt_cache_key` values
so upstream thread/session headers and request bodies target the same cache bucket.
Both modes are labeled, intentionally causing one cold cache/socket transition
when upgrading from the former model-name-derived identity. Quota and retry
cooldown identity remain account/provider based and do not include the mode.

Provider-visible replay fidelity, sidecar validation, and typed semantic authority are
specified by
[SPEC-tau-provider-chatgpt-streaming-replay](SPEC-tau-provider-chatgpt-streaming-replay.md).

## Model metadata tags

ChatGPT/Codex model publication includes provider-owned capability tags such as
`shell:chatgpt` and `tools:custom-text`. These tags describe the model/backend surface;
the harness owns all policy that maps them to tool alternatives.

## WebSocket turn cancellation

The synchronous WebSocket turn loop receives transport events and cooperative
abort wakes through one inbound queue. Pool checkout and connection setup share
the same typed abort source. Exact observable behavior is specified by
[SPEC-tau-provider-chatgpt-cancellation](SPEC-tau-provider-chatgpt-cancellation.md).

## Tool definitions

The Responses adapter publishes and serializes both Function and Custom tool types.
Model metadata must continue to advertise both so harness prompt capability truth
matches the upstream request.

## Terminal request rejection

Responses transports classify canonical provider error codes before transport recovery.
In particular, `context_length_exceeded` is a typed terminal provider failure across
WebSocket, SSE, and non-2xx responses: it cannot reopen a cached socket, replay a full
request, fall back between transports, or enter the logical-prompt retry scheduler.
Unknown stream failures and explicit transient status/code classes retain their existing
retry ownership. Terminal context classification trusts only canonical Responses
envelope `code`/`type` fields; echoed nested fields and provider prose are not
authoritative. Known canonical transient identifiers retain precedence over
deterministic HTTP status classification.

Retry classification remains provider-owned and is exported only as a closed structured
category, saturating attempt, and bounded approximate delay. The harness validates
prompt ownership before projecting those facts to watchers; provider display text and
response bodies are never watcher data.

## External provider trust boundary

This crate sends prompt/tool context to ChatGPT/Codex endpoints and parses provider
responses back into Tau stream state. Treat upstream responses and diagnostics as
crossing an external-provider trust boundary.

## Streaming status boundary

ChatGPT/Codex output implements
[SPEC-tau-provider-chatgpt-streaming-replay](SPEC-tau-provider-chatgpt-streaming-replay.md)
under the workspace
[SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md).
Provider-authored text and tool payloads never become status or response-stat metadata.

## Transient reply hints

Message-envelope `reply` attributes are capability hints, not durable authority.
Responses chaining must not preserve an older server-side rendering after route or
effective-tool liveness changes.
