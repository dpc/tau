# Public Responses provider security boundary

Public Responses profiles send complete typed transcripts over explicitly
selected HTTP/SSE or WebSocket transport to an operator-configured endpoint
under the shared outbound network policy
documented in [`tau-provider/SECURITY.md`](../tau-provider/SECURITY.md). Provider
payloads remain untrusted model data: response, SSE-line, and WebSocket-frame
bounds apply before the
parser admits only supported assistant text, plain reasoning, and Function
calls.

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

Only an operator-declared exact route may send legacy
`compat.openai_prompt_cache` controls. Tau sends its stable
`tau:<agent-id>` key to that configured external provider, making it a
provider-visible correlation value. Public Responses accepts only legacy
automatic retention, so the operator accepts the provider's retention posture
and possible volatile-suffix cache-write premium.

The backend does not accept explicit cache options and does not rewrite
top-level `instructions` into a content block. HTTP/SSE and WebSocket serialize
the same key and legacy retention fields.
