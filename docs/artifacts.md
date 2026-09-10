# Artifact transfers

Tau's Artifact RPC stores **immutable original bytes under their content hash**.
The shell extension registers `export(path)` and `import(key)` under ordinary
tool-role policy. Export reads one local regular file under the shell instance's
remembered workdir authority, uploads at most 16 MiB of original bytes, and
returns the descriptor plus a bounded filename hint. Import validates the key,
downloads and verifies the complete original, and writes it to a private
unpredictable mode-0600 temporary file on the shell execution host; that local
path can be passed to `read_image`. Neither tool exposes store paths, inline
original bytes, execution, or archive extraction.

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
- `ArtifactDownload::new(key)`, `next_op()`, `accept(value)`, `close_op()`, and
  `into_bytes()`: Open, bounded ranges, verified exact size/digest, then Close.
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

`ArtifactDescriptor` contains only canonical `key` and `size`. Attach bounded
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
