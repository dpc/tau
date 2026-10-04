# Artifact transfers

Tau's Artifact RPC stores **immutable original bytes under their content hash**.
Model-facing text refers to an artifact with the canonical Markdown autolink
`<tau-artifact:FULL_KEY>`, for example
`<tau-artifact:blake3:0123…cdef>`. `FULL_KEY` is the complete internal
content-address key; the abbreviated example is not usable. Consumers share one
formatter/parser for this spelling, and `import` and `read_image` also accept a
bare key for simple existing integrations.

The shell extension registers `export(path)` and `import(key)` under ordinary
tool-role policy. Export reads one local regular file under the shell instance's
remembered workdir authority, uploads at most 16 MiB of original bytes, and
returns `artifact` and `size` output headers plus a bounded `filename` hint.
Import validates the reference, downloads and verifies the complete original,
and returns `path` and `size` output headers after writing it to a private
unpredictable mode-0600 temporary file on the shell execution host; that local
path can be passed to filesystem tools. The provider-independent
`read_image(key)` tool instead consumes a verified original directly from
Artifact storage without granting shell or workdir authority. None of these
tools exposes store paths, inline original bytes, execution, or archive
extraction.

## Consumer API

`tau-proto` defines `ArtifactRequest`, `ArtifactResult`, `ArtifactOp`,
`ArtifactValue`, `ArtifactDescriptor`, and `ArtifactError` in `artifact`.
`ArtifactKey`, `ArtifactRequestId`, `ArtifactUploadId`, and `ArtifactReadId`
are distinct validated types: parse strings at the boundary, then keep the
types through correlation and transfer state. Descriptors also validate their
size during construction and decoding; use `descriptor.size.get()` for bytes.
`tau-client` exports:

- `ArtifactClient::new(runtime.handle())` and
  `start_request(exact_session_id, op) -> request_id`: bounded detached writer
  admission, not a filesystem acknowledgement. Receive the matching
  `HarnessOutputMessage::ArtifactResult` in the ordinary main loop. This handle
  is cloneable and does not borrow runtime input.
- `ArtifactUpload::new(original_bytes)`, `next_op()`, `accept(value)`,
  `descriptor()`, and `abort_op()`: Begin, bounded writes, then Finalize.
- `ArtifactDownload::new(key)`, `next_op()`, `accept(value)`, `descriptor()`,
  `close_op()`, and `into_bytes()`: Open, bounded ranges, verified exact
  size/digest, then Close.
  Construction is infallible after parsing an `ArtifactKey`. Upload `Begin`
  takes an `ArtifactSize`, validated before sending rather than by storage.

For every response, check its correlation and `artifact_frame_fits(&message)`
before accepting its value. One outstanding operation per transfer keeps
response ownership explicit. The helpers leave request generation separate
from response acceptance so the loop can process cancellation and unrelated
input while storage is outstanding.

Before a paid or otherwise expensive producer effect, request `Available`.
Memory-only harnesses return `Permission`; do not start generation then discover
that originals cannot be saved. Availability is not a disk-space reservation.
Persistent ephemeral sessions can store artifacts; tool guidance must say that
original bytes persist independently of their ephemeral transcript.

`next_op()` does not advance transfer state. On a lost Write or Finalize
response, resend the same operation/identity; do not begin a new upload and do
not regenerate content. A response-lost Begin can leave only an expiring
unfinished upload, not a published original. A recognized finalized upload
receipt remains retryable across reconnection/restart by the same configured
instance and session for one day; expired identities fail unavailable. Durable
receipts return the original descriptor even if independent retention has since
removed those original bytes. A new explicit upload is a new age renewal.

On cancellation, stop submitting chunks and send `abort_op()` or `close_op()`
best-effort. Do not delete a shared object or assume cancellation reversed a
publication. If tool cancellation wins while Finalize succeeds, its original
may remain without a recorded tool result. Never retry a paid producer effect
as artifact-write recovery.

`ArtifactDescriptor` contains only the internal canonical `key` and `size`.
Format that key as `<tau-artifact:FULL_KEY>` whenever it enters model-facing
text. Attach bounded
filename/media hints to the consumer's own per-use arguments/results, not the
shared object. Validate media independently. A digest detects corruption but
does not authenticate a producer or make content safe to execute.

## Bounds and storage

- Original: 16 MiB; raw range: 1 MiB; complete encoded frame: 8 MiB.
- One harness: eight open uploads/reads combined; eight queued/in-flight or
  unconsumed worker completions, plus eight retained Artifact egress frames
  until writer acknowledgement or retirement (a two-stage bound of sixteen,
  not eight end-to-end). Overflow disconnects only that recipient without
  queueing another response. Upload bytes remain bounded in worker memory.
- Transfers expire after 120 seconds from admission, never extended by traffic.
  Reads require explicit Close so a lost final-range response can be retried.
  Disconnect invalidates unfinished connection-owned state.
- One stable shared store lock coordinates cooperative processes. Active reads
  currently make finalization return `Busy` and cause cleanup to skip its pass,
  even for unrelated digests. No blocking lock wait stalls the worker. A caller
  can close its own reads and retry the same finalization operation.
- Finalization staging and receipts use a separate one-day recovery window.
  Startup processes at most 1,024 entries per domain per pass. These are finite
  maintenance bounds, not an aggregate persistent disk quota.

The harness owns `<state>/artifacts/blake3/<hex>/{data,meta.json}`, private
finalization receipts in `operations`, and private detach staging in `.cleanup`.
Extensions never receive these raw paths and must not inspect them directly.
Completed originals are independent of session directories. Different state
roots do not fetch from one another automatically.

`artifact_retention: null` is the default. A value such as `30d` enables
opportunistic startup cleanup by explicit last-new-put metadata; reads and
sharing never refresh it. No exact deadline, reference pins, or periodic sweep
is promised. Shared-root operators should select consistent policies.

## Compatibility

This interface uses protocol **6.0**. The versioning specification requires a
major bump unless best-effort skew continuation is deliberately supported.
Artifact helpers assume an actual correlated RPC rather than silently ignoring
new messages, so configured extensions with an older major are rejected before
Configure/Ready. New extensions likewise cannot run against a 5.x harness.
Rebuild the harness and configured extension/provider peers against matching
`tau-proto`/`tau-client` revisions before activation. This change does not
activate, publish, deploy, or migrate any external extension automatically.

The shell consumer keeps
their existing provider/model modality and role gates, and must not advertise a
storage-dependent operation on an incompatible or unavailable route.

The governing non-local contract is
[SPEC-shared-artifacts](../specs/SPEC-shared-artifacts.md).
## Large terminal text pastes

A single paste of at least 8 KiB of normalized UTF-8 becomes an artifact.
CRLF and bare CR become LF before measuring and uploading; no other trimming
occurs. Smaller pastes, including pasted file paths, remain ordinary text.
The maximum artifact is 16 MiB. Tau does not read paths or fetch pasted URLs.

While uploading, the original draft and cursor stay unchanged and editing,
submission, history navigation, and draft-switch bindings are paused. Ctrl-C
discards the paste without canceling an agent prompt. A second paste is rejected
with a busy notice. A successful upload inserts only an editable
`<tau-artifact:FULL_KEY>` reference. It does not submit the prompt.

On failure the source remains in memory outside the draft: Enter explicitly
retries, Ctrl-C discards. Tau never falls back to submitting the wall of text.
Retries preserve acknowledged upload identity/offset where available; an expired
transfer may require discarding and pasting again. A paste above 16 MiB cannot
upload, but remains retained until discarded. Closing the UI discards local
pending source. Cancellation or a lost result may leave a shared original.
Artifacts persist independently of session transcripts, including ephemeral
ones, and references do not pin retention.

Uploads reuse the existing interactive UI transport; there is no extra UI
connection or lifecycle participant. They require harness protocol 8.1 or newer
and a persistent artifact store. A memory-only/older harness reports failure
without inserting the original text.

## Native terminal clipboard pastes

Tau probes DEC private mode 5522 without blocking startup. A terminal advertising
the Kitty OSC5522 clipboard extension can send a MIME offer after a user paste
gesture. Tau enables that mode only after a supported report; unsupported or
unanswered probes retain ordinary bracketed text paste. Mode5522 supersedes
bracketed paste on capable terminals. The opt-in `terminal-responses` feature
of `dpc-tau-crossterm` supplies responses through the existing sole Unix terminal
reader; non-Unix input currently retains the text fallback.

Tau prefers `image/png`, then UTF-8 `text/plain;charset=utf-8`, then `text/plain`
(strictly decoded as UTF-8). PNG bytes always become an artifact, regardless of
size; text still uses the normalization and 8 KiB threshold above. MIME claims
are hints, not image validation. No filesystem paths or `text/uri-list` offers
are dereferenced, and copied-file byte transfer is not implemented.

Each read preserves the offer's primary/default location and optional paste
grant, supplies the name `Paste event`, and uses a fresh request ID. Only matching
opening OK, independently decoded DATA chunks, and final DONE admit content.
Inventory is limited to 64 KiB; read chunks to 4096 decoded bytes; complete
content to 16 MiB. These are Tau ingestion bounds, not terminal clipboard
capacity or an immutable clipboard snapshot.

Acquisition freezes the original draft and cursor. Ctrl-C discards acquisition;
it does not cancel an agent prompt. Concurrent pastes report busy; overlapping
unidentified inventories are discarded explicitly rather than mixed. Permission,
unsupported MIME, malformed transfer, and timeout failures discard partial bytes
and leave the draft untouched. Paste again to retry acquisition. Only a completed
source enters the upload retry flow above; Enter never rereads the clipboard.

The probe, MIME offer, and requested read have fixed total deadlines of 1, 10,
and 15 seconds respectively. Traffic does not extend them; a very slow SSH
transfer can therefore fail explicitly even while receiving data. Tau disables
clipboard mode on focus loss. Focus gain and successful terminal resume reprobe.
Cancellation and focus changes do not erase the obligation to drain already
admitted native replies.

Before an editor, picker, or normal interactive quit releases terminal input,
Tau retires its sole input helper and, if native mode was admitted, requires a
fresh, complete OSC5522 dot-metadata reply after mode-off. The two-second decision
budget cancels the operation on missing or invalid proof; it does not hard-cancel
kernel I/O. Tau stays interactive, retains the draft and ordinary queued keys,
and keeps native clipboard mode disabled until a successful explicit retry.
Old queued foreground requests cannot launch later automatically. Unsupported
terminals that never admitted native mode need no metadata fence.
Failed quit sends no quit/detach/shutdown request; retry the original command or
Ctrl-D. Explicit `:quit-force` warns that queued protocol bytes or keys may reach
the shell and cannot guarantee clean shell input. Fatal failures and forced
termination have only best-effort cleanup, not this safe-boundary guarantee.

Discarded or stale IDs never publish partial artifacts.
Crossterm's isolated-ESC disambiguation limit is 25 ms: once recognized,
response frames can span reads, but a longer split between initial ESC and its
introducer is not guaranteed to frame as a response.
