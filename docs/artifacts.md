# Artifact transfers

## Copy an artifact to your clipboard

Use `:export-to-clipboard KEY` in the interactive UI. Completion suggests up to
ten originals from the whole shared store, newest explicit upload first. Typing
a key prefix searches before limiting, so older originals remain discoverable.
Rows abbreviate the displayed key but insert the full key, with original byte
size and a header-derived format hint or “format unknown.” Canonical
`<tau-artifact:FULL_KEY>` references also work when entered directly.

Tau downloads and verifies the complete size and BLAKE3 digest, then closes the
read before sending any clipboard bytes. Copying/listing does not refresh
artifact retention. Bytes remain unchanged: no newline conversion, text
normalization, transcoding or base64-as-text substitute.

No options or filename are needed:

- PNG/JPEG/GIF/WebP signatures select native image MIME writes, not files.
- PDF, ZIP/GZIP/RAR, MP4, AVI, WAV, Ogg and ID3-tagged MP3 headers select copied
  files with matching extensions.
- Otherwise complete strict UTF-8 without control characters (except TAB/LF/CR)
  becomes ordinary `text/plain;charset=utf-8`, with a `text/plain` alias.
  HTML, SVG, JSON and source code remain plain text, not active formats.
- Unknown binary data becomes a copied file with a `.bin` extension.

These bounded header guesses are not media validation. MP4 recognition checks
only a complete `ftyp` box of at most 512 bytes for `mp41`/`mp42` brands; other
containers (including WebM, HEIF and AVIF) fall back conservatively. No parsing of
archive contents or recovery of original filenames occurs. Completion hints keep
their smaller existing vocabulary; export decides from the verified original.

Use `--raw` to force a raw MIME write even for binary data, retaining inferred
MIME or `application/octet-stream` for unknown binary. Override both mode and type
with `:export-to-clipboard KEY --mime image/svg+xml`, for example;
`--raw --mime image/svg+xml` is equivalent.
The bounded ASCII MIME parser supports `type/subtype` and one optional unquoted
`;charset=TOKEN` parameter, up to 256 bytes; it rejects wildcards, whitespace and
other parameters. Explicit UTF-8 text declarations require valid UTF-8.
Overrides never alter shared metadata or add a plain-text alias.

Native OSC5522 MIME writes reach the workstation through compatible terminals
and multiplexers, including over SSH. Support and write permission are separate;
paste support does not grant export permission. Tau reports a correlated
terminal acknowledgment, denial, unavailable support, or an unknown outcome.
It never silently falls back to OSC52 or runs a remote clipboard utility.
Explicit raw binary writes do not become desktop files and may have no paste
target in another application.

### Copy as a local file

Use `:export-to-clipboard KEY --file` to offer a real file for pasting into a
local file manager, even for text or an image. Automatic binary exports use this
same file path. The default basename is `artifact-<full BLAKE3 digest>.<extension>`
(for example `.pdf`, `.png`, `.txt`, or `.bin` when unknown).
Choose a name with `--file --name "quarterly report.pdf"`; single quotes also
work. Quotes group the whole name literally, without shell expansion or escape
syntax. Names must be 1–255 UTF-8 bytes, not `.` or `..`, and contain no path
separators or control characters. `--name` requires `--file`; `--mime` and
`--file` cannot be combined, nor can `--raw` and `--file`. A supplied name is
preserved exactly: Tau does not append or correct its extension.
Names are per-use hints, not shared artifact metadata.

This requires the dedicated file-export support in the terminal and **every**
multiplexer hop, plus the existing clipboard write permission. WezTerm stages
the verified original on the workstation, including when Tau runs over SSH.
Tau never writes a remote path or substitutes a raw MIME write. An old terminal
or multiplexer may ignore the request; Tau then times out without fallback.
Successful acknowledgment means the terminal offered a copied file, **not**
that a destination application pasted it.

The staged file has a **hard one-hour validity**, even if the clipboard selection
is still owned. Ownership loss does not immediately delete it, but clipboard
managers cannot extend its lifetime. Paste before expiry; export again explicitly
if needed. Stage capacity can be busy rather than evicting an unexpired file.

File export uses OSC5522 `write-file` / `write-file-data` / `write-file` replies,
with the same fresh safe ID on the opener, every chunk and the empty commit.
Every data chunk declares `application/vnd.tau.file-export-v1`; ordinary raw
writes of that MIME remain raw. The decoded count-one `TAUFILES\0` bundle contains
u32BE count, u16BE basename length, u16BE MIME-hint length, u64BE body length,
then UTF-8 basename, optional ASCII hint and exact original body. Tau currently
sends an empty hint. The protocol permits a 16 MiB original plus at most 535 bytes
of framing/metadata; this does not enlarge the existing raw-write limit.
There is no admission handshake, alias, or raw-write fallback. Only an exact
file-family reply can acknowledge this operation.

Only one clipboard transfer runs at a time. Ctrl-C cancels export before agent
prompt cancellation; focus changes, foreground handoff and disconnect revoke
pending work. The operation has a fixed 60-second budget. Cancellation stops
future packet admission, but cannot undo an already admitted commit; a lost
acknowledgment can therefore leave the native outcome unknown. The sole terminal
writer emits bounded packets asynchronously. Existing foreground handoff still
waits for admitted output before releasing terminal ownership; a blocked output
syscall is not forcibly interrupted by that budget.

Listing uses a coalesced asynchronous three-second cache and an off-loop scan
with bounded result memory and a two-second decision budget checked between
filesystem operations. Failed/incomplete scans are reported unavailable, not as
partially recent pages. No durable index, MIME catalog or timestamp migration is
needed. The UI requires harness protocol 13.1 or newer for this feature.

## Shared original-byte storage

Tau's Artifact RPC stores **immutable original bytes under their content hash**.
Model-facing text refers to an artifact with the canonical Markdown autolink
`<tau-artifact:0123456789abcdef01234567>`: exactly 24 lowercase hex digits,
the first 96 bits of BLAKE3, without an algorithm label. The example illustrates
syntax, not an available artifact. Consumers share one formatter/parser;
`import` and `read_image` also accept the bare 24-digit key.
Only the fixed 96-bit identity is accepted. Old algorithm-labelled full-digest
references, bare or wrapped, are rejected. This identity targets mostly trusted
local development.

Private storage still uses full-digest paths, so existing originals and protected
transcript objects remain accessible without renaming. Fixed-width lookup rejects
ambiguous matches, and publication rejects different bytes sharing an identity.
There is no arbitrary-length abbreviation lookup or hash-algorithm negotiation.
Protocol 20.0 and source SDK 0.28.0 require peers to rebuild together.
Old full-key typed image/history records and upload receipts no longer decode;
plain-text history and current short-key records are unchanged. There is no
automatic history or receipt migration. Changing the source does not update an
installed runtime or publish an SDK release.

The shell extension registers `export(path, mime_type?)` and
`import(key)` under ordinary tool-role policy. Export reads one local regular file
or directory under the shell instance's remembered workdir authority, applies the
configured stored-byte limit
(32 MiB by default), and returns `artifact` and `size` output headers plus bounded
`filename` and available `mime_type` hints. Explicit MIME wins; otherwise common
filename extensions supply a best-effort declaration, without content sniffing.
Import validates the reference, downloads and verifies the complete original,
and returns `path` and `size` output headers after writing it to a private
unpredictable mode-0600 temporary file on the shell execution host; that local
path can be passed to filesystem tools. The provider-independent
`read_image(key)` tool instead consumes a verified original directly from
Artifact storage without granting shell or workdir authority. None of these
tools exposes store paths, inline original bytes, or execution.
Tau directory bundles are automatically extracted as described below.

Import creates a generic private temporary name. Rename the returned local file
if a useful name or extension is needed. Export hints are per-use claims, not
stored digest metadata or proof of media validity.

### Directory artifacts

Export packs directories in-process as versioned tar+zstd bundles. The complete
bundle, including its identifying header, is the content-addressed original.
Import recognizes that header from the bytes, not a media declaration. Results
include `kind: file` or `kind: directory`.
Directory exports always advertise `application/vnd.tau.directory`.
Malformed or unsupported typed bundles fail rather than becoming regular files.
Ordinary tar/zstd files without the Tau header remain ordinary files.

The directory codec supports only regular files and directories. Export rejects
directory-root and member symlinks and special files; existing top-level symlinks
to regular files still work. It copies files with multiple hardlinks independently.
Traversal uses directory-relative no-follow handles. The encoder sorts paths,
normalizes ownership, timestamps and modes, and uses fixed compression settings.
Identical quiescent content produces repeatable bytes within an encoder version,
not across library upgrades. This is not an atomic filesystem snapshot.

Import first verifies the entire stored original, then extracts to a new private
directory. It rejects unsafe paths, duplicate/conflicting members, links, sparse
entries and unsupported tar metadata. Files are mode 0600 and directories 0700;
executable bits, ownership, ACLs and xattrs are not restored. Failed or canceled
extraction removes its staging tree. Successful returned paths belong to the
caller and can be read with ordinary filesystem tools.

Set independent startup limits in `harness.yaml`:

```yaml
artifact_limits:
  stored_bytes: 33554432
  expanded_bytes: 33554432
```

Both accept integer bytes from zero through 512 MiB. Invalid values fail config
loading. Stored bytes cover regular originals or complete compressed bundles;
expanded bytes cover aggregate logical regular-file content. There is no ratio
constraint against message size. The shell receives the accepted policy through
Configure; the store rejects over-limit Begin/Open requests. Image, clipboard
and terminal-paste consumers retain their own smaller bounds.

Directory processing also limits entries to 10,000, UTF-8 paths to 255 bytes and
32 components, zstd windows to 8 MiB, and the full decoded tar stream to the
expanded limit plus 16 MiB. Limits apply during I/O, including trailer validation.
The format is a reserved `TAU-DIRECTORY` prefix plus NUL, version byte 1, and
exactly one zstd frame containing restricted tar. The decoder rejects concatenated
frames and hidden nonzero payload after the tar end.

## Compacted-prefix recall

After successful standalone compaction, Tau prepares a searchable directory
artifact before the next inference. A short harness-owned notice tells the agent
to `import` its reference, then read `transcript.txt` or grep the returned directory
recursively to include large payloads in `payloads/`. In `manifest.json`, Tau
identifies the exact replaced semantic prefix and includes at most one parent
archive link or an explicit ancestry gap. The accepted native or local replacement
stays unchanged. Inline provider compaction without an exact standalone cut does
not create an archive.

Tau labels historical sources in the transcript and prefixes inline payload lines
with `| `. It preserves typed watch status and lifecycle payloads, along with
delivery provenance for sessions, the CLI, and peer reviews. It puts tool arguments
and rendered results larger than 1 KiB in numbered `payloads/` files. It also puts
ordinary text larger than 8 KiB and text with control characters other than
tab/newline in separate files, preserving exact
UTF-8 bytes. Tau omits reasoning, opaque provider payloads, transport sidecars,
duplicate raw tool objects, and media bytes. It never follows old paths or artifact
references to fill in missing output. Archived text is historical evidence, not
new instructions.

Automatic recall requires a durable session and agent. Archives use ordinary
shared-store retention, not transcript pinning, and can outlive session deletion.
Tau may reconstruct archives from the journal after restart without repeating a
compactor call or adding a durable publication phase. Preparation is best effort:
if storage is unavailable, work is overloaded, or a limit prevents preparation,
Tau preserves the summary and adds an unavailable notice.
The producer caps encoded whole-history journal replay at 64 MiB, so a long journal
can prevent recall even when the current replaced window is small. It caps stored
originals and cumulative expanded preparation at 32 MiB or the lower configured
artifact limit, and considers at most 32 archive generations. The current window
gets priority; an unavailable older archive leaves an explicit ancestry gap rather
than disabling current recall. Preparation has one absolute 30-second runtime
deadline, including waits for journal writes and artifact queueing and work.
Tau uses existing frontier notifications rather than polling. On expiry, Tau
resumes inference with an unavailable notice even if a blocking filesystem
operation is still running. It retains worker and deletion custody until that
operation finishes; the deadline does not forcibly interrupt filesystem calls.

## Recover from Busy tool calls

Artifact operations share bounded worker and transfer capacity. Store-lock
contention can also cause `Busy`, which tools display as
`artifact storage is busy`. You cannot infer the exhausted resource or storage
corruption from this error alone.

If parallel `export`, `import`, or `read_image` calls return `Busy`, keep
successful artifact references and imported files. Let outstanding calls finish,
then retry only calls that failed with `Busy`, one at a time. Use a small fixed
retry budget (for example, two retries per call). If `Busy` persists, stop and
report it. Do not rerun the whole batch, regenerate source content, or increase
admission limits. These steps describe caller recovery, not automatic tool
retries.

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

- Original: 32 MiB by default, configurable through 512 MiB; raw range: 1 MiB;
  complete encoded Artifact frame: 8 MiB. The outer protocol ceiling is fixed at
  32 MiB, not a promise that every operation admits that size. Existing client
  8 MiB queue/admission, semantic producer 16 MiB, debug 16 MiB, and durable
  record 64 MiB bounds remain separate. Artifact bytes never enter event logs.
- One harness: eight open uploads/reads combined; eight queued/in-flight or
  unconsumed worker completions, plus eight retained Artifact egress frames
  until writer acknowledgement or retirement (a two-stage bound of sixteen,
  not eight end-to-end). Overflow disconnects only that recipient without
  queueing another response. Upload reservations admit 128 MiB concurrently,
  or one larger configured original alone. The shell applies the same aggregate
  policy before preparation/download and keeps it through result publication;
  large configuration values do not multiply by its command queue length.
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
sharing never refresh it. No exact deadline, textual-reference pins, or periodic sweep
is promised. Shared-root operators should select consistent policies.

## Prepared-image history and backups

Durable persistent image invocations with ready scoped storage authority upload
the **prepared, re-encoded bytes**,
not the original artifact or a transformation recipe. Their canonical tool result
stores the exact key, size and per-use image metadata. The harness verifies actual
media and synchronizes transcript protection before admitting the reference.
Ephemeral agents, session-ephemeral and memory-only invocations retain inline
prepared output; they are not silently promoted because a shared store exists.

Roots cover an agent's complete retained history, including inactive branches and
replaced compaction windows. Compaction, unload and provider-cache eviction do not
unpin images. Whole-history deletion releases roots only after history removal is
synchronized; orphan protection is safe. Released objects return to ordinary
shared retention rather than immediate erasure. Prepared copies can appear in
authorized artifact discovery and can outlive their originating session.
Ordinary textual artifact links still do not pin objects.

Back up and restore the complete selected persistent state, including agent/session
journals **and the entire `artifacts` directory**, its protected objects, root
registry and marker. Restore these together before cleanup or referenced-history
use. A journal-only backup is no longer sufficient for reference-bearing images.
Missing/corrupt bytes fail the consuming provider request before model submission;
Tau does not regenerate guessed pixels or reinterpret the original as a repair
recipe. A provider's verified memory-cache hit is not a backing-store health check.

New readers retain old inline history without rewriting it. Old binaries cannot
decode new reference-bearing journals, even though ordinary CAS metadata remains
version 1. Preserve a complete pre-upgrade backup for rollback; do not point an old
harness at new journals. The [source rollout checklist](prepared-image-rollout.md)
separates tested source behavior from publication and external rebuild work.

## Compatibility

Artifact RPC was introduced in protocol **6.0**. Prepared-image references require
protocol **14.0** and source SDK **0.14.0**. The versioning specification requires a
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
While an upload runs, the prompt shows `Uploading artifact; Ctrl-C to cancel`.
This progress notice disappears on success, failure, or cancellation; failures
still report retry/discard guidance.
CRLF and bare CR become LF before measuring and uploading; no other trimming
occurs. Smaller pastes, including pasted file paths, remain ordinary text.
The terminal-paste maximum remains 16 MiB. Tau does not read paths or fetch pasted URLs.

While uploading, the original draft and cursor stay unchanged and editing,
submission, history navigation, and draft-switch bindings are paused. Ctrl-C
discards the paste without canceling an agent prompt. A second paste is rejected
with a busy notice. A successful upload inserts an editable
`<tau-artifact:FULL_KEY> (mime_type: text/plain;charset=utf-8)` reference and
normalized-text media hint, not the original text. It does not submit the prompt.

On failure the source remains in memory outside the draft: Enter explicitly
retries, Ctrl-C discards. Tau never falls back to submitting the wall of text.
Retries preserve acknowledged upload identity/offset where available; an expired
transfer may require discarding and pasting again. A paste above 16 MiB cannot
upload, but remains retained until discarded. Closing the UI discards local
pending source. Cancellation or a lost result may leave a shared original.
Artifacts persist independently of session transcripts, including ephemeral
ones, and these textual references do not pin retention.

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

Tau prefers the copied-file bundle described below, then `image/png`, then UTF-8 `text/plain;charset=utf-8`, then `text/plain`
(strictly decoded as UTF-8). PNG bytes always become an artifact, regardless of
size and inserts `<tau-artifact:FULL_KEY> (mime_type: image/png)` into the draft;
no original filename is available. `Paste event` is a permission label, never a
filename hint. Text still uses the normalization and 8 KiB threshold above. MIME claims
are hints, not image validation. Other native MIME payloads, such as `audio/mpeg`
and `image/jpeg`, become opaque artifacts without UTF-8 conversion or a media
whitelist. URI/file-manager reference formats, portal tokens and selection-control
metadata are not mistaken for original file bytes. Opaque hints retain the media
type/subtype, not request parameters. Tau never dereferences filesystem paths or
`text/uri-list` offers on its execution host.

### Copied files, including over SSH

The matching terminal fork can augment an ordinary paste gesture's native MIME
offer with `application/vnd.tau.files-v1`. Its explicit file-paste configuration
allows **the workstation terminal** to resolve copied local regular files and
capture their bytes at the gesture. Normal symlinks are supported; directories,
devices, network fetching, and cut/deletion effects are not. Remote Tau receives
file bodies through the same OSC5522 stream, including SSH and opted-in tmux
hops, not workstation paths or filesystem authority. Plain URI lists do not
transfer file bodies.

Tau validates the entire completed bundle before uploading any original. There
is no file-extension or media whitelist, archive extraction, execution, automatic
image rendering, or prompt submission. Files upload sequentially through the
existing artifact client. Their canonical references enter the unchanged draft
together only after every upload finishes, with bounded sanitized basename and
optional untrusted media hints. Duplicate names remain separate ordered entries;
identical bodies naturally share a digest. Upload retries retain exact originals
and resume the unfinished file rather than rereading the clipboard or uploading
finalized predecessors again.

Grouped prompt insertion is **not atomic artifact publication**. Failure or
cancellation can leave already-finalized immutable originals, as with ordinary
artifact uploads; no deletion/rollback is promised.

The private v1 MIME encoding uses the **nine-byte** literal `TAUFILES\0`, followed
by an unsigned big-endian 32-bit file count. Each entry has big-endian 16-bit
basename and MIME-hint lengths and a 64-bit body length, then UTF-8 basename,
optional ASCII type/subtype hint, and exact opaque body bytes. Names cannot be
empty, `.` or `..`, or contain separators/control characters. Hint parameters are
not part of this encoding. The declared count and lengths must consume the
encoding exactly; unknown versions, truncation, trailing data, and malformed
metadata are rejected. There is no discretionary sixteen-file limit: framing
and the existing aggregate acquisition bound constrain admission.

The existing 16 MiB acquisition cap includes **all bundle framing and bodies**;
clipboard consumers retain a separate 16 MiB original limit even though general
artifact storage supports larger originals. Neither cap is an
OSC5522 requirement. Existing fixed acquisition/upload deadlines still apply,
so slow SSH transfers can fail explicitly. Terminal file snapshots are ordinary
bounded filesystem reads, not atomic snapshots or a promise to interrupt a
blocking filesystem syscall.

### Acquisition and terminal ownership

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
