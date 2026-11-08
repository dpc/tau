# SPEC-shared-artifacts: Shared original-byte artifacts

## Record justification

Protocol DTOs, client transfer state, harness admission and filesystem workers,
startup configuration/retention, and extension consumers jointly implement
original-byte identity, authority, durability, and lifetime, so none can own the
complete contract locally.

Artifacts bind immutable original bytes and optional filename/media declarations
into one BLAKE3 identity. The selected persistent state root defines a shared
cross-session namespace. Identical bytes and metadata share one original and
age; different metadata produces a different identity rather than overwriting
first- or last-writer hints. Metadata remains a declaration, not media validation.
There is no public overwrite, delete, reference-pin, or implicit remote-fetch API.
Whole-store discovery is restricted to authenticated UIs; configured extensions
retain known-key read authority without enumeration.
Model-facing text names an artifact with the canonical Markdown autolink
`<tau-artifact:0123456789abcdef01234567>`: exactly 24 lowercase hexadecimal
digits, the first 96 bits of BLAKE3, with no algorithm label. Consumers may
also accept the bare key or the plain `tau-artifact:0123456789abcdef01234567` URI.
Owned plain-text UI locators use the URI without brackets; prompt and model-facing
Markdown references use the bracketed autolink. This identity is fixed, not a variable abbreviation.
Only the fixed-width key is accepted; former algorithm-labelled full-digest
references and typed keys are rejected.

Private CAS paths retain full BLAKE3 digests, so existing objects, receipts, and
protected aliases need no renaming. Short identities resolve to one unique
full digest across ordinary and protected catalogs; duplicate aliases of that
digest are not ambiguous. Distinct matching digests fail closed, and publication
under the store lock rejects collisions rather than overwriting or aliasing
different bytes. Lookup scans have bounded memory and a two-second decision
budget between filesystem calls. Current typed roots retain their corresponding
physical objects. No physical-store migration is required. Old full-key typed
history and upload receipts do not decode; ordinary textual history is unchanged.

The 96-bit identity targets mostly trusted local development. It makes no new
adversarial-content security claim and adds no algorithm-negotiation mechanism.
Knowing a digest permits lookup through configured Artifact RPC peers; it does
not grant model-role, tool, or remote-recipient authority. Configured local
extensions and cooperative peers remain the existing trust boundary, not
untrusted Internet ingress.

New uploads use the shared version-one identity framing: the ASCII domain
`tau.artifact` followed by a zero byte and version byte one; optional filename
then MIME; unsigned 64-bit big-endian payload length; then raw payload bytes.
Each optional UTF-8 field has a zero absent tag or a one present tag followed by
its unsigned 64-bit big-endian byte length and bytes. Each field is bounded to
255 UTF-8 bytes; absent and present-empty are distinct. Payload limits exclude
framing. Upload clients require the returned scheme and metadata to equal their
request, and downloads verify the complete framed identity before use.
Stored metadata without an identity remains legacy payload-only data. Legacy
objects and short-key receipts remain readable without migration or invented
metadata; re-export creates a new metadata-bound identity. Unknown schemes fail
closed. Descriptors, protected aliases, roots, retry receipts and listing preserve
the exact immutable identity.

Persistent ephemeral sessions may explicitly create and read shared artifacts.
Consumers must disclose that this persists original bytes independently of
ephemeral transcripts in user-facing documentation or tool guidance. The CLI
paste upload notice is a short, transient progress indicator and need not repeat
that disclosure; [artifact documentation](../docs/artifacts.md) explains paste
retention. Memory-only harnesses reject artifact operations before
persistent root access and perform no artifact cleanup. Availability is an
explicit preflight before an expensive producer effect, not a reservation or
proof that a later filesystem write will succeed.

Transfers use separate directed, non-event Artifact request/result messages.
Neither original bytes nor transfer bookkeeping enters semantic journals,
interception, generic broadcasts, or debug JSONL. Existing canonical typed
image previews remain inline and independently replayable under
[SPEC-typed-image-tool-results](SPEC-typed-image-tool-results.md); an original
artifact never replaces preview or context replay authority.

Authenticated UIs may upload, read known originals, and query a bounded recent
page from the whole shared store on their
existing connection. Exact-session Hello admission captures the session binding
generation; both that authority and transfer connection ownership remain current
for each request. Initial owned stdio and attached socket UIs use the same
admission route; unrelated external peers, probes, and in-memory/non-admitted
clients gain no artifact authority. Mutation/deletion authority is not added.
Large terminal pastes upload normalized editor-equivalent UTF-8 and insert
the canonical editable artifact reference with a per-use media hint. The pending source never enters
prompt drafts, history, or event presentation. Failure retains it for explicit
retry/discard without submitting the original text. Cancellation leaves the
original draft unchanged and may leave a committed shared original.

Validated native clipboard file bundles upload exact original bodies through
the same UI authority, without granting remote path resolution. Per-use filename
and bounded media/name hints become immutable digest metadata, not content
authority. A multi-file
paste inserts its complete reference list only after every original finalizes;
this grouped editor update does not make backing-store publication transactional.
Failure/cancellation may retain finalized predecessors, and explicit retry
resumes retained unfinished transfer state without rereading the clipboard.

UI discovery ranks validated metadata by latest explicit put, descending, with
full-key ties ascending. Prefix filtering precedes the page limit. A scan
returns a complete advisory top-N page or fails, never presents an incomplete
traversal as globally recent. Discovery is off-loop, bounded in retained memory
and decision time, revocable on disconnect, and does not renew retention.
Listing returns stored metadata only, without reading or classifying payload
headers. Completion displays recorded MIME when present and omits it otherwise.

Clipboard export is an explicit UI-local operation. The consumer verifies the
complete original size/digest and closes its read before admitting terminal
output. It preserves bytes under an explicit per-use media declaration or
conservative local inference, including generic binary fallback. This does not
validate media; ordinary raw writes do not create desktop files. Copied-file
intent (explicit or inferred for non-image binary originals) instead sends a
count-one bundle through a dedicated terminal
packet family, never by interpreting a private MIME on the raw-write path.
The terminal owns local staging and native copied-file publication; success
acknowledges an offer, not a destination paste. Staged files have hard one-hour
validity, not indefinite clipboard-manager retention.
Export uses the sole terminal writer,
one bounded packet at a time, and requires a correlated native acknowledgment;
permission denial never triggers a different clipboard protocol. Lifecycle
revocation prevents later packet admission, including commit, but cannot retract
an already admitted packet. A lost acknowledgment or cancellation racing commit
has an unknown native outcome. No clipboard bytes or control payloads enter
semantic history or diagnostic logs.

The startup `artifact_limits.stored_bytes` policy defaults to 32 MiB, bounded by
a 512 MiB protocol object ceiling. Expanded directory content has an independent
`expanded_bytes` policy with the same default and ceiling.
Configure propagates the accepted policy to consumers; store Begin/Open and
consumer allocation admission enforce it independently. Raw chunks remain 1 MiB,
with the complete encoded frame bounded separately in both directions. A bounded
off-loop worker owns filesystem I/O. Artifact response retention remains bounded
through writer acknowledgement or retirement; egress overflow disconnects only
the affected recipient. Ordinary semantic event publication retains its existing
independent lag policy.
Uploads and reads have finite non-renewable lifetimes and connection ownership;
write chunks acknowledge accepted bytes, accepting only
next offsets or identical retransmissions. Active read transfers hold stable
cross-process coordination so cleanup skips them. Stat alone does not pin.
Consumers verify original size and digest before use; a digest proves equality,
not provenance, safe content, or media validity.

Directory exports use an in-process versioned tar+zstd codec shared with other
producers. The identifying bytes are part of the digest; recorded MIME remains
an unverified declaration and cannot trigger extraction. Key-only import
recognizes
directory bundles, verifies the full stored original, then validates and extracts
regular files and directories into private staging. Unsupported versions,
unsafe paths, links, special entries, exceeded limits and malformed/trailing
streams fail closed with staging cleanup. Returned files are non-executable.
Directory traversal does not follow symlinks; existing top-level regular-file
symlink exports remain supported. The encoder uses sorted paths, normalized
metadata and fixed settings for repeatable bytes within an encoder version,
without cross-version canonicalization or filesystem snapshot semantics.
Artifact transfer bytes remain outside event logs.

Finalization publishes complete originals and explicit metadata atomically,
synchronizing files and publication directories before a successful descriptor.
New explicit uploads of duplicate bytes and metadata renew shared age, without
rewriting
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
deletion do not delete shared objects.

Startup cleanup is opportunistic, not a hard TTL. It revalidates age under the
same stable cross-process coordination as finalization, durably detaches before
recursive removal, and recovers prior detached staging. Ordinary textual references do not pin
originals; a later read may be unavailable even while a transcript preserves
its digest. A later new put of identical bytes and metadata can restore that same
key.
Processes sharing a root should use consistent retention settings: any enabled
cleaner may apply its own configured policy.

Generic corruption notices expose only closed bookkeeping categories, never
object paths or digest components. Those paths would otherwise grant known-key
read authority for prepared images. Detailed storage inspection remains an
explicit local operator action, not a generic notice or diagnostic projection.

### Internal transcript retention

Harness-owned roots conservatively retain the union of a durable agent's entire
history, including inactive branches and replaced compaction windows. Root
establishment verifies exact bytes, size, and digest under the same exclusive
cross-process lock as cleanup and synchronizes protection before a referencing
record may be admitted. Media validation and tool authority remain separate
admission requirements; a root is not permission to append. Failed publication,
cancellation, or a failed journal append may leak protection, never acknowledge
unprotected history. No extension can pin or release roots.

Protected bytes have a private hardlink alias within the same CAS and key
namespace. The audited older cleaners only traverse ordinary objects, transfer
operations, and cleanup staging; their canonical detach/unlink cannot remove
the protected inode. Published data is immutable: writers must never truncate or
mutate it in place. Older duplicate uploads replace only ordinary age metadata;
an older upload after ordinary deletion may create another inode for the same
digest. No object-format promotion breaks existing old readers.

Known-key reads can fall back to a protected alias when the ordinary object is
absent. Corrupt ordinary metadata does not authorize silent repair or replacement.
Consumers still verify the complete bytes. Private aliases are not extra UI
catalog entries: an ordinary entry expired by an older cleaner can disappear
from discovery while its exact bytes remain available for retained history.
Reads and rooting do not renew age. New explicit puts renew existing aliases;
older writers know only ordinary metadata, so a protected-only alias retains
its last known explicit-put timestamp, not a reconstructed later put time.

Cleanup must finish a bounded, complete root scan before deleting any candidate
object or alias. Detectably missing expected metadata, malformed authority, read failures, and scan-budget
exhaustion preserve candidates rather than interpreting a partial scan as an
empty set. Root-set overflow rejects establishment, never drops older roots.
Filesystem calls themselves do not acquire a hard deadline from the scan budget.
Whole-transcript deletion releases roots only after staged history removal and
its directory sync succeed. Compaction, unload, and cache eviction do not release
roots. Ambiguous deletion or failed release may leave conservative orphan roots.
Unpinning returns bytes to ordinary age policy, not immediate erasure; retention
disabled means unrooted objects may remain indefinitely.

Backups that use this foundation must preserve the selected CAS, its protected
objects and root registry alongside agent history. Restore protection before
allowing cleanup or referenced-history use; a journal-only backup cannot retain
the bytes of reference-bearing records. This is cooperative filesystem
crash consistency, not tamper detection: arbitrary loss of a whole owner directory
(or both the registry and its marker) cannot be distinguished from absent roots.
Partial backups or independently deleted metadata subtrees are not safe cleanup
inputs; restore the complete registry, rather than treating absent owners as an
authoritative empty set.

The shell `export` consumer reads one bounded local regular file using its
ordinary remembered-workdir filesystem authority and performs a new explicit
upload. Its model-facing result exposes only kind and canonical artifact reference.
Export persists the bounded basename and the explicit file MIME override, or the
MIME inferred from a known extension. Directory bundles record their fixed MIME.
Shell `import` validates a canonical reference or bare key, verifies the complete
download, and returns kind, private path, payload size and recorded MIME when
present. A safe filename becomes a mode-0600 child inside a fresh mode-0700
`tau-artifact-*` directory. Missing, unsafe or unusable filenames fall back to a
generic private file. The returned file kind/path is separate from ownership of
its private parent, which is removed on failure or cancellation before successful
result publication. Directory extraction layout remains unchanged.
These tools never expose harness store paths or original bytes through
model text; import and inspection do not renew age.

The protocol revision follows
[SPEC-extension-protocol-versioning](SPEC-extension-protocol-versioning.md).
Detailed helper usage and finite implementation bounds live in
[artifact transfers](../docs/artifacts.md).
