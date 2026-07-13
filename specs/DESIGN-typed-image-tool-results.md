# DESIGN-typed-image-tool-results: Typed local image tool results

Status: confirmed, 2026-07-13, user

Local image inspection is a native tool-result capability, not text containing
base64 and not a synthesized user message. A successful image-producing
function call retains its call id and normalized text result while carrying
ordered typed image content as canonical encoded bytes, closed MIME type,
dimensions, and high detail. Provider adapters lower that semantic content only
on explicitly audited routes.

The first surface is `tau-ext-shell`'s one-image `read_image(path)` tool. It
inherits the extension's ordinary filesystem authority and session lifecycle.
It accepts PNG, JPEG, and WebP, reads an opened regular file once, enforces
source, decoded-allocation, dimension, pixel, output, record, and provider
request bounds, rejects animation, applies orientation, strips metadata by
re-encoding, and prepares bounded high-detail pixels. Crop, original detail,
GIF, caches, thumbnails, attachments, multiple images, and other provider
adapters are outside this version.

The harness exposes an image-producing tool only when the exact provider model
route publishes both image-input and image-tool-result modalities. GPT-5.6
Sol/Terra/Luna on the ChatGPT Responses surface are the audited initial routes.
Responses lowering uses `function_call_output.output[]` with text followed by
`input_image`; Responses Lite receives prepared high-detail pixels but omits
the `detail` field. Unsupported projections contain a bounded omission marker
and never send image bytes.

Canonical image bytes are durable transcript data under the same retention and
access rules as other session data. Generic UI completion events, wait results,
debug logs, and provider diagnostics retain safe metadata but omit image bytes
and data URLs. The durable agent transcript and the selected provider's directed
prompt retain typed bytes as the replay and inference authority; generic live
broadcasts and subscriber replay receive byte-free projections regardless of
client kind. Image buffers use shared immutable
ownership within a process so diagnostic and prompt projections do not deep-copy
the payload. Live provider-result broadcasts exclude UI clients, and historical
UI replay converts the durable provider event to a byte-free generic tool result.
Recursive debug and TRACE projections clear image buffers before JSON
serialization, including prompt contexts and compaction replacement windows.

An agent may retain at most 128 MiB of logical canonical image bytes across its
complete append-only history, including branches and compaction replacement
windows. The quota is enforced before durable append for both durable and
ephemeral agents. Encoded agent records are also rejected on write above the
same 64 MiB bound enforced by the loader. These retained-data bounds complement,
rather than replace, the per-image and per-provider-request bounds above.

The shell tool is foreground-only: it declares `BackgroundSupport::Never`
because background completion events cannot carry typed provider content. Both
the extension and core independently check decoder-reported output bytes before
allocation; WebP receives a stricter 4,194,304-pixel/32-MiB decoded-output bound
because its decoder has additional workspace allocations, and only one image
decode may run concurrently in the shell extension.

This decision refines [SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md)
and is implemented at the component boundaries described by
[SPEC-tau-proto-provider-data](../crates/tau-proto/specs/SPEC-tau-proto-provider-data.md),
[SPEC-tau-harness-prompt-dispatch](../crates/tau-harness/specs/SPEC-tau-harness-prompt-dispatch.md),
[ARCH-tau-provider-chatgpt](../crates/tau-provider-chatgpt/specs/ARCH-tau-provider-chatgpt.md),
and [ARCH-tau-ext-shell](../crates/tau-ext-shell/specs/ARCH-tau-ext-shell.md).
