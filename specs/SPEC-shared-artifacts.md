# SPEC-shared-artifacts: Shared original-byte artifacts

## Record justification

Protocol DTOs, client transfer state, harness admission and filesystem workers,
startup configuration/retention, and extension consumers jointly implement
original-byte identity, authority, durability, and lifetime, so none can own the
complete contract locally.

Artifacts are immutable original bytes addressed directly by their BLAKE3
digest. The selected persistent state root defines a shared cross-session
namespace. Identical bytes share one original and age; filenames and media
claims are per-use hints, not canonical identity or first-writer metadata.
There is no list, overwrite, delete, reference-pin, or implicit remote-fetch API.
Knowing a digest permits lookup through configured Artifact RPC peers; it does
not grant model-role, tool, or remote-recipient authority. Configured local
extensions and cooperative peers remain the existing trust boundary, not
untrusted Internet ingress.

Persistent ephemeral sessions may explicitly create and read shared artifacts.
Consumers must disclose that this persists original bytes independently of
ephemeral transcripts. Memory-only harnesses reject artifact operations before
persistent root access and perform no artifact cleanup. Availability is an
explicit preflight before an expensive producer effect, not a reservation or
proof that a later filesystem write will succeed.

Transfers use separate directed, non-event Artifact request/result messages.
Neither original bytes nor transfer bookkeeping enters semantic journals,
interception, generic broadcasts, or debug JSONL. Existing canonical typed
image previews remain inline and independently replayable under
[SPEC-typed-image-tool-results](SPEC-typed-image-tool-results.md); an original
artifact never replaces preview or context replay authority.

Objects are bounded to 16 MiB and raw chunks to 1 MiB, with the complete encoded
frame bounded separately in both directions. A bounded off-loop worker owns
filesystem I/O. Artifact response retention remains bounded through writer acknowledgement or
retirement; egress overflow disconnects only the affected recipient. Ordinary
semantic event publication retains its existing independent lag policy.
Uploads and reads have finite non-renewable lifetimes and connection ownership;
write chunks acknowledge accepted bytes, accepting only
next offsets or identical retransmissions. Active read transfers hold stable
cross-process coordination so cleanup skips them. Stat alone does not pin.
Consumers verify original size and digest before use; a digest proves equality,
not provenance, safe content, or media validity.

Finalization publishes complete originals and explicit metadata atomically,
synchronizing files and publication directories before a successful descriptor.
New explicit uploads of duplicate bytes renew shared age, without rewriting
the original. Recognized same-upload retries recover a durable receipt and do
not renew age, including after restart. Interrupted finalization intents retain
their original fixed put timestamp. Receipt recovery is bounded independently
of original retention; an expired upload identity fails unavailable rather than
becoming a new put. Cancellation, disconnect, response loss, or an unsuccessful
sync may leave a committed orphan. No blob-plus-tool-journal transaction or
exactly-once producer effect is claimed.

`artifact_retention: null` disables age-based original cleanup, not storage.
Enabled retention uses the existing positive integral single-unit duration
grammar and last successful new explicit put/export metadata. Reads, imports,
sharing, replay, and recognized retries do not renew age. Clock rollback cannot
decrease a duplicate's age timestamp; future timestamps and malformed metadata
are retained rather than replaced by file mtime or epoch zero. Session and agent
deletion have no artifact effect.

Startup cleanup is opportunistic, not a hard TTL. It revalidates age under the
same stable cross-process coordination as finalization, durably detaches before
recursive removal, and recovers prior detached staging. References do not pin
originals; a later read may be unavailable even while a transcript preserves
its digest. A later new put of identical bytes can restore that same key.
Processes sharing a root should use consistent retention settings: any enabled
cleaner may apply its own configured policy.

The protocol revision follows
[SPEC-extension-protocol-versioning](SPEC-extension-protocol-versioning.md).
Detailed helper usage and finite implementation bounds live in
[artifact transfers](../docs/artifacts.md).
