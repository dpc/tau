# SPEC-typed-image-tool-results: Typed local image tool results

## Record justification

Extension preparation, protocol/harness persistence and byte-free projections,
the [security disclosure boundary](../SECURITY.md), and audited provider
lowering in
[Codex Responses](../crates/tau-provider-codex/specs/ARCH-tau-provider-codex.md)
and
[Chat Completions](../crates/tau-provider-chat-completions/specs/ARCH-tau-provider-chat-completions.md)
jointly implement this contract, so no one local artifact can coherently own
its end-to-end typed-content, retention, and disclosure rules.

Local image inspection is a native tool-result capability, not text containing
base64 and not a synthesized user message. A successful image-producing
function call retains its call id and normalized text result while carrying
ordered typed image content as inline encoded bytes or a reference to their
retained prepared bytes, closed MIME type, dimensions, and high provider detail.
Provider adapters lower that semantic
content only on explicitly audited routes.

The distinct `ImageReference` form carries the BLAKE3 key and exact encoded size
of the prepared canonical bytes with the same closed media/detail and dimensions.
It is not an original-image key or transformation recipe, and cannot also carry
inline bytes. The DTO policy defaults to Inline. Before dispatch, the harness
captures Artifact for authorized image-producing calls in durable sessions with
persistent agents and current scoped persistence authority; session-ephemeral,
memory-only, ephemeral-agent and unavailable-authority cases retain Inline.
A newly accepted persistent agent may still be awaiting physical creation; it
does not receive Artifact authority until its persistence lease is Prepared.
Producers must not guess durable authority from artifact availability or a session
name. Interception preserves the harness-selected storage field while retaining
existing argument and web-policy edit behavior. A changed call or agent identity
forces that field to Inline; subsequent interception cannot restore a discarded
grant by reverting identity. The original call's captured authority is unchanged.
Artifact mode requires bounded runtime size/digest and actual-media
verification plus durable transcript protection before canonical admission.
Core remains filesystem-free: reference shape and each logical occurrence's size
are validated on replay, not eagerly resolved from disk. Inline history continues
to receive its existing full media validation without rewriting old records.
The shared lifetime and backup boundary is
[SPEC-shared-artifacts](SPEC-shared-artifacts.md).

The prepared uploader shares the original reader's bounded call/correlation loop.
It releases the original read before beginning a prepared upload, verifies the
Finalize descriptor against the prepared bytes, and retains tool causality and
per-use metadata in the reference result. Pending allocation cleanup remains
bound to its original session, including cancellation before Begin acknowledges.
A late descriptor after cancellation cannot produce success; an unknown upload
outcome may leave a shared object. Failure does not retry a producer effect or
substitute inline bytes for requested Artifact storage.

Tool-reference admission uses the existing eight-operation artifact worker
budget, including queued work and unconsumed completions. Pure call, permission,
policy and logical-quota checks precede I/O. Scoped exact Agent lock custody
precedes CAS coordination; actual media validation precedes protected backing and
root publication. The main loop rechecks call, configured source, session,
runtime incarnation and exact persistence lease before ordinary immutable
canonical publication, then rechecks current core constraints. Head navigation
alone does not revoke a still-owned tool round: existing core call-id ownership
attaches its result to the original round without selecting that old branch.

Verification is cancelable runtime work, not a semantic terminal reservation.
A committed cancel request settles this harness-owned phase even though the
producer has already finished. Late work cannot revive the terminal; a completed
root may remain conservative orphan protection. Storage overload, missing/corrupt
bytes and invalid media fail the affected call without inline fallback. Once
verified output enters ordinary publication, existing interception, backpressure,
retained-terminal retry and post-commit settlement rules apply without repeating
storage verification.

Generic standalone replacement windows may contain structurally closed tool
rounds with new references; prior-source image equality is not required. Their
actual bytes and roots are verified before the canonical boundary under the
captured transaction, attempt, cut, source and persistence authority. Ordinary
provider terminals still cannot inject ToolResult. Opaque provider JSON is not
interpreted as typed references.

The built-in provider resolves references through bounded Artifact RPC before
image-consuming ordinary, native compact, local-summary and prewarm submission.
It caches digest-verified encoded bytes, validates every occurrence's media
metadata, and keeps retry snapshots reference-backed. Unsupported routes preserve
their explicit omission without fetching images. Missing or corrupt content
fails the consuming request before model submission; a valid cache hit is not a
durable-store health check. Cache eviction never releases transcript roots and
does not imply model-token or embedding caching.

The provider-independent surface is `tau-ext-utils`'s one-image
`read_image(key)` tool. Prefer a bare 24-digit lowercase hex artifact key for the
argument; it also accepts `tau-artifact:KEY` or `<tau-artifact:KEY>`.
The input boundary normalizes these spellings once to `ArtifactKey`. Typed artifact
identity fields serialize only bare keys; original tool-call arguments retain
the supplied text for replay. UI locators use the plain URI, while
model-context references use Markdown
autolinks as defined by [SPEC-shared-artifacts](SPEC-shared-artifacts.md).
It verifies immutable original bytes through the
existing directed Artifact RPC without granting shell, workdir, import, or
provider-account authority. It accepts PNG, JPEG, and WebP and enforces
source, decoded-allocation, dimension, pixel, output, record, and provider
request bounds, rejects animation, applies orientation, strips metadata by
re-encoding, and prepares pixels with a named local profile. Bare calls use the
compatible `high` profile bounded to a 2048-pixel side and 2,500 32-by-32
patches. Explicit experimental `overview` calls use 1024-side/600-patch bounds
for coarse inspection. An optional half-open region selects pixels from the
EXIF-oriented source before profile resizing. Both profiles remain provider
high detail: overview is a local raster transform, not provider low detail.
Original detail, equality suppression, GIF, decoded-raster caches, thumbnails, attachments,
multiple images, and provider uploads are outside this version. Artifact reads
do not renew original retention age.

The canonical text and typed-image ordering follow
[SPEC-context-output-formatting](../crates/tau-harness/specs/SPEC-context-output-formatting.md#canonical-tool-text).
Image results retain native tool-result headers and typed image payloads rather
than using an injected user-role envelope. See the native/source lane distinction
in [SPEC-streamed-tool-output](SPEC-streamed-tool-output.md#source-and-completion).

Model-facing success text reports the delivered raster dimensions as `size: WxH`.
If those dimensions differ from the full EXIF-oriented source before cropping or
resizing, it also reports the source dimensions as `source_size: WxH`. The source
is the coordinate space for subsequent region requests. Detailed preparation
diagnostics remain in UI metadata rather than model-facing text. Canonical
transformed bytes and dimensions remain the sole authority for provider request and context
accounting. Overview remains experimental and must not become the bare default
or normative general guidance until the opt-in live visual-fidelity oracle
meets its [documented gates](../docs/read-image-fidelity-oracle.md).

The harness exposes an image-producing tool only when the exact provider model
route publishes both image-input and image-tool-result modalities. Audited
routes include the published multimodal ChatGPT/Codex models listed in
[ARCH-tau-provider-codex](../crates/tau-provider-codex/specs/ARCH-tau-provider-codex.md) and
operator-declared multimodal llama.cpp Chat Completions models.
Responses lowering uses `function_call_output.output[]` with text followed by
`input_image`; standard ChatGPT Responses preserves `detail: high`, while
Responses Lite receives prepared high-detail pixels but omits
the `detail` field. Audited Chat Completions lowering uses a `tool` message whose
`content` array contains the normalized `text` part followed by `image_url`
parts with high-detail data URLs, the multimodal tool-result shape accepted by
llama.cpp. Unsupported projections contain a bounded omission marker and never
send image bytes.

Canonical image content is durable transcript authority. Legacy inline records
retain their bytes; references require the associated rooted shared artifacts.
Prepared copies use shared Artifact lifetime and may outlive transcript deletion.
Generic UI completion events, wait results,
debug logs, and provider diagnostics retain safe metadata but omit image bytes
and data URLs. They also omit fetchable prepared-image keys, including keys with
invalid or zero claimed sizes. Generic projections use the existing empty inline
image shape for safe metadata, never a fake content address.
The durable agent transcript and the selected provider's directed
prompt retain typed bytes or protected references as replay and inference authority; generic live
broadcasts and subscriber replay receive byte-free projections regardless of
client kind. Image buffers use shared immutable
ownership within a process so diagnostic and prompt projections do not deep-copy
the payload. Live provider-result broadcasts exclude UI clients, and historical
UI replay converts the durable provider event to a payload-free
`tool.result_display` projection.
Debug and TRACE projections that serialize a prompt as JSON recursively clear
image buffers, including prompt contexts and compaction replacement windows.
The built-in provider's ordinary TRACE record instead emits only fixed
content-free structural metadata and does not serialize prompt content.
The Tau-owned local summary fallback for Chat Completions, OpenRouter, and
public Responses uses the same ordinary provider lowering and therefore retains
canonical image bytes through the warmed request prefix.

An agent may retain at most 128 MiB of logical canonical image bytes across its
complete append-only history, including branches and compaction replacement
windows. The quota is enforced before durable append for both durable and
ephemeral agents. Encoded agent records are also rejected on write above the
same 64 MiB bound enforced by the loader. These retained-data bounds complement,
rather than replace, the per-image and per-provider-request bounds above.
Repeated references to one digest count repeatedly against this logical quota;
physical CAS deduplication does not expand transcript or upstream byte budgets.

The utility tool is foreground-only: it declares `BackgroundSupport::Never`
because background completion events cannot carry typed provider content. Both
the extension and core independently check decoder-reported output bytes before
allocation; WebP receives a stricter 4,194,304-pixel/32-MiB decoded-output bound
because its decoder has additional workspace allocations, and only one image
decode may run concurrently in the utility extension.

The native typed-result choice is recorded by
[GATE-typed-image-tool-results](GATE-typed-image-tool-results.md).
This behavior refines
[SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md)
and is implemented at the component boundaries described by
[SPEC-tau-proto-provider-data](../crates/tau-proto/specs/SPEC-tau-proto-provider-data.md),
[SPEC-tau-harness-prompt-dispatch](../crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md),
[ARCH-tau-provider-codex](../crates/tau-provider-codex/specs/ARCH-tau-provider-codex.md),
[ARCH-tau-provider-chat-completions](../crates/tau-provider-chat-completions/specs/ARCH-tau-provider-chat-completions.md),
and [ARCH-tau-ext-utils](../crates/tau-ext-utils/specs/ARCH-tau-ext-utils.md).
