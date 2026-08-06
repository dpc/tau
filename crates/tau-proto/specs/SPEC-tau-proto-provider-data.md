# SPEC-tau-proto-provider-data: Provider-visible data

## Record justification

Provider-visible data contracts span protocol DTOs, core transcript folding,
harness prompt assembly, tool rendering, and backend-specific lowering. No one
of those implementation areas can coherently state the shared semantic
authority, replay sidecars, escaping boundaries, and provider presentation
rules described here.

## Provider-visible tool responses

Tool result events carry raw CBOR for non-provider consumers, but provider prompt construction must render tool outputs through `ToolResponse::render()`. That render path is the central defense-in-depth normalization boundary after tool-local semantic escaping.

A successful function-tool result may additionally carry typed
`ToolResultContentPart::Image` values. Image bytes never pass through
`ToolResponse::render()`; the normalized text remains first and providers append
typed images in declared order inside the same causal tool-result envelope.
Closed media/detail enums and decoded dimensions accompany canonical binary
bytes. Old events and transcript items omit the additive content field and
remain text-only.

`ToolResponse::render()` must keep headers as safe single lines, preserve ASCII LF body separators for line-oriented records, escape other model-visible control and separator characters, and keep binary/fallback rendering bounded. This is not terminal/UI escaping; terminal renderers still need their own sanitization for display state and layout.

Bounded model-visible diagnostic text helpers that are shared by harness, core,
and extensions may also live in `tau-proto` when they have no dependency on
harness state or extension implementation details. These helpers are part of the
same prompt-surface safety boundary as rendered tool responses: keep work
bounded, outputs deterministic, and tests close to the exported helper.

## Tool-call argument dual representation

`ToolCallItem.arguments` is the parsed CBOR semantic form of assistant tool-call
arguments. Harness validation, tool routing, repair, and dispatch must use this
field as the source of truth.

`ToolCallItem.raw_arguments_json` is an optional replay/cache-identity sidecar
for provider function calls whose upstream wire format supplied arguments as a
JSON string. Providers should populate it with the exact original string while
also parsing into `arguments`; prompt replay should prefer the sidecar when
reconstructing provider history and fall back to serializing `arguments` only for
old persisted records or calls without provider-wire JSON. The sidecar is not a
semantic authority and must not bypass CBOR validation/dispatch.

`ToolCallItem.responses_envelope` is an optional Responses-only replay sidecar
for provider-owned tool-call output item envelope fields such as item `id`,
`status`, and unknown future fields. It must not contain semantic fields such as
`call_id`, `name`, `arguments`, or `input`; those are rebuilt from the validated
`ToolCallItem` fields so harness-side id normalization and dispatch remain
authoritative. Responses replay should prefer the provider item id from this
sidecar and fall back to deterministic `fc_`/`ctc_` synthesis only for old
records.

## Opaque provider item dual representation

`OpaqueProviderItem.value` is the parsed CBOR form of provider-owned output
items such as Responses reasoning, compaction, and unknown future provider items.

Standalone compaction control uses bounded transaction identifiers and
harness-owned started, failed, and inference-dispatch checkpoint facts. New
starts pre-mint and persist the compact prompt/model/standalone-operation tuple;
successful boundaries repeat that tuple so replay can validate exact provider
work ownership. Canonical submitted, injected, and steered inputs carry a
default-false `inference_activation` marker: new activating writes set it true,
while passive and legacy facts cannot independently replay-wake inference.
`agent.compacted` records carry immutable cut and suffix metadata; metadata-free
historical records retain legacy hard-boundary replay semantics.
It exists for semantic inspection and compatibility with protocol consumers that
need structured data.

`OpaqueProviderItem.raw_json` is an optional provider-visible replay sidecar for
the exact JSON item emitted by a backend. Responses request reconstruction should
prefer this sidecar so key order and numeric spelling remain stable for upstream
cache identity, and fall back to serializing `value` only for older records or
items that were not captured from provider JSON.

The public Responses backend represents validated plain `reasoning_text` output
as both `ReasoningTextKind::Full` display content and an opaque `Reasoning` item.
The display item follows the existing thinking-visibility policy and is not
replayed; the opaque item is the durable replay authority. Encrypted,
summary-only, malformed, and mixed reasoning items do not enter the transcript.

## Responses assistant message dual representation

`MessageItem.role`, `MessageItem.content`, and `MessageItem.phase` are Tau's
semantic assistant-message truth. Rendering, prompt display, and any future
message-level validation must use those typed fields.

`MessageItem.responses_raw_json` is an optional Responses-only replay sidecar for
assistant `message` output items. Responses request reconstruction may reuse it
to preserve provider item ids, statuses, annotations, content-part boundaries,
and unknown fields that are not semantically modeled by Tau. Replay must still
rebase the provider-visible text and `phase` from the typed fields, and must
drop the sidecar for non-Responses providers. The sidecar is replay-eligible
only when it decodes as a Responses assistant `message`; otherwise providers
must synthesize replay from the typed fields.

## Provider-visible message facts

The six harness-authored canonical `message.*` event types share one compact
`<message event="…">` provider projection. Canonically ordered attributes
carry the harness-stamped publisher plus applicable message, target, party, and
conversation identifiers. Text-bearing facts use direct exact-close-framed text;
delete and reaction facts are self-closing. Publisher-provided text and metadata are
untrusted data and grant no identity, routing, instruction, tool, or control
authority. Opaque `extension_data` is never projected generically.

Controls in message-fact bodies (including LF and tab), line separators,
bidi/format controls, and noncharacters become visible `\u{XXXX}` escapes before
exact `</message>` collisions are replaced. Attributes retain separate
XML-delimiter and quote escaping.
Message-fact references are descriptive opaque identifiers, not generic reply
routes or capabilities.

## Provider-visible interactive user prompts

Canonical submitted and steered prompt facts retain raw accepted text plus
harness-stamped `PromptSubmissionSource`. Derived transcript entries preserve that
typed source. During provider assembly only, `HumanUi` text projects as one
fieldless `<user>...</user>` user-role item with only exact `</user>` collisions replaced;
all other text, whitespace, and Unicode remain unchanged. Every other submission
source and injected input remains raw or uses its separate typed projection.

This late presentation boundary makes live and replay deterministic, including
historical facts already tagged `HumanUi`, without changing canonical text,
provider-specific lowering, or provider cache identity. See
[SPEC-interactive-user-prompt-envelope](../../../specs/SPEC-interactive-user-prompt-envelope.md).

## Provider model declarations and canonical state

Configured provider extensions publish complete replacement snapshots as
transient `provider.models_declared` events. The harness publishes accepted
current state as a separate transient `provider.models_updated` event, then emits
model and role availability projections. `ProviderModelsDeclared` contains the
provider's proposed model list. `ProviderModelsUpdated` contains the accepted list
plus `publisher_extension_id`, the stable configured provider identity whose
current state the snapshot replaces. Empty canonical lists therefore withdraw only
that publisher's state and remain attributable during live delivery and replay.

Declarations are mutable through same-name interception before commit. Canonical
updates are harness-authored, immutable, and must-pass. Live delivery identifies
the provider connection as the declaration source and the harness as the canonical
source; canonical payload provenance remains the stable configured publisher.
Subscribe-time current-state catch-up emits one canonical update for each active
provider, including an active provider's empty snapshot, and must not regenerate
declarations or model-application side effects. It is synthesized from running
state rather than durable replay. This is the provider-model row of
[SPEC-peer-event-publication](../../../specs/SPEC-peer-event-publication.md).

Provider model declarations are categorically transient. An extension-supplied
request for persistence is ignored; neither the declaration nor its optional
cache contract enters semantic journals. Newly attached live subscribers receive
a canonical snapshot synthesized from running-harness current state. This
subscribe-time catch-up is not durable replay or restart reconstruction.

## Provider tool-type metadata

`ProviderModelInfo.supported_tool_types` is provider-published capability
metadata. Omitted or empty metadata is legacy-compatible Function-only support;
Custom tools require explicit publication. The harness may narrow this set with
policy but must not widen it. Changes require coordinated provider, harness,
wire-compatibility, and serialization tests.

`ProviderModelInfo.input_modalities` and `tool_result_modalities` describe the
exact composite model/route capability. Omitted legacy fields mean text-only.
Image-producing tools require explicit image support in both fields; model
names and generic compatibility tags do not widen this capability.

Typed image data is CBOR byte-string content with shared immutable in-process
ownership; cloning a prompt or event must not copy the image allocation.
Diagnostic projections replace that shared buffer before generic JSON
serialization. This ownership optimization does not change the durable or wire
representation.

`ProviderResponseFinished.failure_kind` carries a closed, display-prose-independent category for
terminal provider request rejection. `context_window_exceeded` allows lifecycle consumers to
reason about the outcome without parsing the bounded human-readable `error` field.

`ProviderModelInfo.supports_parallel_tool_calls` describes whether the exact
published provider/model route can generate multiple direct tool calls in one
response. It is an effective route capability, not abstract model metadata.
Legacy publishers that omit it decode as `true`; publishers serialize their
effective value explicitly.

`ProviderModelInfo` also carries optional fixed-point estimated USD prices per
million ordinary input, provider-reported cached reads, cache writes, output
tokens, and cache token-hours. Omitted ordinary/read/output categories resolve
independently to the central GPT-5.6-equivalent fallback. An omitted cache-write
rate uses ordinary input; omitted storage has no charge. These values are basic
equivalent-API comparison metadata, not billing facts; provider declarations
must reject negative, malformed, or over-precise decimals.

`ProviderModelInfo.cache_policy` is an optional runtime-only declaration of
documented cache mechanism, TTL shape, renewal operation, output floor, quota
treatment, prefix-identity version, and privacy posture for the exact route.
Absence means no contract was declared, not that the provider performs no
caching. Minimum, fixed, sliding, and unknown TTLs remain distinct; response
observations never promote minimum or unknown residency into a hard deadline.
The declaration contains no cache key, cache/object identifier, prompt content,
timestamp, hit history, or lifecycle state.

Cache pricing continues to use the existing optional read, write, and
token-hour fields as its sole model metadata. Runtime policy must not reinterpret
the central UI fallback as a provider price. Cache declarations remain part of
transient provider model current state and live catch-up only; they add no
semantic persistence, cold replay, or restart reconstruction.
