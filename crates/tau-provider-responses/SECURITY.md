# Public Responses provider security boundary

Public Responses profiles send complete typed transcripts over explicitly
selected HTTP/SSE or WebSocket transport to an operator-configured endpoint
under the shared outbound network policy
documented in [`tau-provider/SECURITY.md`](../tau-provider/SECURITY.md). Provider
payloads remain untrusted model data: response, SSE-line, and WebSocket-frame
bounds apply before the
parser admits only supported assistant text, plain reasoning, and Function
calls.

Explicit durable-session debug capture stores the finalized HTTP/SSE request at
its send boundary or the exact WebSocket `response.create` envelope at frame
send. It stores a bounded successful-response event snapshot after validation,
or bounded metadata for non-cancellation failures. WebSocket connection and
upgrade failures therefore have no request artifact; cancellation has no error
artifact; cancellation after request send can leave the already-submitted
request artifact. Disabled capture and standalone compaction construct and
submit no capture metadata.

Each private `.json.zst` artifact has a strict 1 MiB uncompressed serialization
ceiling; successful responses separately retain at most 512 KiB and 4,096 raw
events. Oversized artifacts become content-free truncation records. The adapter
recursively omits embedded image data URLs, including those in replay sidecars,
function extras, raw successful events, and opaque HTTP error bodies. It redacts
exact occurrences of the untrimmed configured API key in values and keys, and
replaces an object if redaction would collide two keys. This is not general
secret scrubbing: other provider-controlled prompt and response content can
remain sensitive. Queue pressure, compression, filesystem failure, or shutdown
can omit an artifact. Captures never enter ordinary logs, journals, status, or
UI events.

The parser accepts at most 1,024 distinct provider output indices per attempt.
It rejects an out-of-range index or terminal output array before allocating a
slot, which bounds index-driven heap growth and ordered insertion work. Revisit
this bound when changing the response-byte limit, supported output families, or
provider output cardinality contract.

Every public Responses attempt has finite network work: request, connection,
and response headers have a five-minute bound, then a successful SSE or
WebSocket body has an unextendable ten-minute total bound and a renewable
five-minute semantic-idle bound. Only accepted non-empty assistant or
displayable reasoning text, a completed material opaque reasoning item, a
non-empty Function name, or non-empty Function arguments renews idle time.
SSE comments, WebSocket control frames, transport bytes, status/usage,
allocations, empty/duplicate semantic events, and unknown events do not. This
prevents endpoint keepalives from retaining an otherwise stalled attempt while
preserving cooperative cancellation. Revisit this availability boundary when
changing transport framing, semantic parsing, timeout constants, or
cancellation waits.

WebSocket terminal code, type, or incomplete-reason detail retains its first
128 Unicode scalars. Its public failure diagnostic visibly escapes controls and
terminal-unsafe Unicode into one line while retaining ordinary printable text;
classification and retry inspect the original bounded detail, while display
escaping does not alter recovery. Tau deliberately performs no arbitrary secret
scrubbing, accepting the theoretical risk that an operator-configured provider
reflects sensitive content in that bounded diagnostic. Revisit this boundary
when changing captured fields, the scalar cap, escaping policy, public
diagnostic destinations, or classification/recovery flow.

Validated plain `reasoning_text` is sensitive transcript content. Tau retains
both its displayable full-reasoning projection and the exact provider item
sidecar in the durable response, regardless of whether the UI's
`show-thinking` setting currently displays it. The sidecar may contain
provider-controlled fields and secrets reflected by model output. Replay
validates its semantic shape but preserves its exact JSON syntax, then sends it
only as part of the next complete transcript to the selected configured public
Responses endpoint.

Malformed, encrypted, summary-only, mixed, incomplete, or contradictory
reasoning never becomes a durable replay item. The parser also continues to
reject image/file output, custom or hosted tools, and unknown output families.
These validations limit the supported transcript surface; they do not make
provider output trusted or redact reasoning from journals, session inspection,
or other authorized transcript consumers.

## Typed OpenAI prompt-cache controls

Only an operator-declared exact route may send
`compat.openai_prompt_cache` controls. Tau sends its stable
`tau:<agent-id>` key to that configured external provider, making it a
provider-visible correlation value. Legacy retention accepts the provider's
retention posture and possible volatile-suffix cache-write premium. Explicit
first-input-text caching is opt-in per-agent multi-turn cost control, not
cross-agent reuse.

The backend preserves top-level `instructions`, never rewrites it into input
content, and rejects explicit mode locally when no Tau-constructed
non-assistant input-text block exists. HTTP/SSE and WebSocket serialize the same
cache policy fields.
