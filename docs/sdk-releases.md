# Extension SDK releases

Tau publishes the minimum Rust SDK closure needed to build standalone
extensions:

| Package | Direct internal package dependencies |
| --- | --- |
| `dpc-tau-actions` | none |
| `dpc-tau-blocking-notify-channel` | none |
| `dpc-tau-proto` | `dpc-tau-actions` |
| `dpc-tau-client` | `dpc-tau-blocking-notify-channel`, `dpc-tau-proto` |

These four SDK packages retain Rust 1.91 support even though the complete Tau
workspace requires stable Rust 1.97 or newer.

When a release changes the complete closure, publish `dpc-tau-actions` and
`dpc-tau-blocking-notify-channel` first, followed by `dpc-tau-proto`, then
`dpc-tau-client`. Rust source continues to import these packages as
`tau_actions`, `tau_blocking_notify_channel`, `tau_proto`, and `tau_client`.

## Package and protocol versions

### Source SDK 0.31.0: context-scoped incoming guidance (not published)

Proto and client source 0.31.0 use protocol **23.3**. `MessageDelivered` adds
optional `incoming_notice: Option<MessageIncomingNotice>`; initialize it to
`None` in existing struct literals. Constructors leave it absent by default.
`MessageIncomingNotice::new(scope, text)` validates a nonempty scope of at most
256 UTF-8 bytes and text of at most 64 KiB UTF-8. `scope()` and `text()` borrow
those values.

Bridges must derive scope from their authenticated/configured endpoint, not a
remote claimed sender or transient connection, and attach their local snapshot
to each report. The harness namespaces scope by canonical publisher, shows the
first nonempty notice per selected agent context, and rearms after compaction.
The harness preserves empty text but neither displays nor counts it. Later text changes
wait for the next context. Remote bodies and sender-configured notices are not
deduplicated.

Acceptance includes the immutable notice snapshot in content matching. Retrying
an accepted ID after changing the notice is a conflict, not a configuration
update. Both SDK acceptance entry points reject notice-bearing requests below
23.3; requests without a notice still work with 23.2. Ordinary report producers
that require guidance must check Configure themselves rather than assume older
hosts will retain the field.

The client source pins proto at `=0.31.0`. This source preparation does not
publish either crate or activate a runtime.

### Published SDK 0.30.0: canonical message acceptance

Published proto and client 0.30.0 use protocol **23.2**.
`MessageAcceptanceClient` provides an opt-in, bounded, nonblocking request sender. The extension's receive loop
handles correlated `MessageAcceptanceResult` outcomes. A successful enqueue does
not mean the message was accepted. `Accepted { duplicate }` identifies canonical
acceptance within retained history; `Rejected` refuses the current RPC, not every
earlier attempt with that ID. `Unknown` leaves completion uncertain, including
when authoritative recipient history is unavailable. Missing or incompatible
harness versions fail locally before sending.

`start_request` uses `MessageAcceptanceMode::AcceptNew`; `start_existing_request`
uses `ExistingOnly` through the same version guard, correlation and serialized
core admission. Existing-only returns matching retained acceptance or refuses
authoritative absence without appending. Rate-limited bridges can therefore
re-ACK valid, identical, currently authorized retries without accepting new input.
Current consent/authority still applies; changed-content ID reuse is not a valid
retry, and refusal never claims that the ID was historically unaccepted. Both
modes first ship in protocol 23.2. Both methods require 23.2 or later in major 23;
an older runtime could ignore the mode field and admit new input.

For an identical retry, the harness retains one marked canonical message fact,
including after replay. Acceptance does not guarantee fsync, crash-tail survival,
execution, or exactly-once effects. Ordinary message reports keep their existing
behavior. The API neither creates agents automatically nor adds an envelope.

Proto and then client were published on November 10, 2026 from
`d433f67ec5b9ae2e33a03c3aacbd537b51c777de`. Full SelfCI and locked, verified
publish dry-runs passed before publication. Both official sparse-index entries
are unyanked; downloaded registry archives match the regenerated candidate
archives byte-for-byte and contain clean Cargo VCS metadata for that source.
The client pins proto at `=0.30.0`.

| Package | Registry archive SHA-256 |
| --- | --- |
| `dpc-tau-proto` | `fb005facd5389d7c87ab346002b00ad5f3bcc5039ac73f2797a56f36ada04aed` |
| `dpc-tau-client` | `ea69790b3895d8a2721940debb388ab3090639782dd7e751df1d6a0d73c6d011` |

The registry-only consumer passed all 22 tests on Rust 1.91 with an initially
empty `CARGO_HOME` and no local dependency patches. After registry verification,
`dpc-tau-proto-v0.30.0` and `dpc-tau-client-v0.30.0` were pushed and verified at
the exact source on GitHub and the Radicle publishing namespace. This acceptance
record does not change the tagged source or registry bytes.

The unchanged actions and blocking-notify-channel packages remain at 0.1.0.
This SDK-only release does not advance trunk, publish the application, or activate
any runtime.

### Published SDK 0.29.1: artifact tool-input domain type

Published proto/client 0.29.1 retain protocol 23.1.
`tau_proto::ArtifactReference` is the shared tool/UI input type: it deserializes
bare 24-digit lowercase hex keys (preferred), plain `tau-artifact:KEY` URIs, and Markdown autolinks, normalizes to
`ArtifactKey`, and serializes only bare keys. Tool argument structs should use
this type instead of implementing local parsers. `ArtifactKey` remains strict
for wire/history identity fields. Shell export now returns a bare key.

Proto and then client were published from source revision
`841c0854b5d58fba479be2934c0cd66a072428cb`; client pins proto at `=0.29.1`.
Both registry archives match the prepared archives byte-for-byte, have clean
Cargo VCS metadata for that revision, and are unyanked. Their SHA-256 values are
`07c7463492324dedce5d38a47592a1a379569d590f07b47d46a2b18482db21a2`
(proto) and
`4cfca6c813574d6d316c657ecaa073af4173f9f33a3debba9aa9f1f870cee43c`
(client). The registry-only consumer check passed all 21 tests on Rust 1.91
with an empty `CARGO_HOME` and no local patches. Both SDK source tags resolve to
that revision on Radicle and GitHub.

Within the packaged external closure, only the changed Slack and Zulip
artifact-input consumers adopt 0.29.1. Other packaged extensions retain their
exact 0.29.0 dependencies; Slack and Telegram also retain a dev-only proto 0.16.0 lock entry.
Discord is maintained separately and is not a core flake input or native package.
The unchanged actions and blocking-notify-channel leaf packages remain 0.1.0.
SDK publication and downstream dependency updates do not authorize runtime
activation, installation, or restarts.

### SDK 0.29.0: protocol 23.1

The prepared proto/client **0.29.0** packages contain the current protocol
**23.1** source. Published SDK 0.28.0 advertises protocol 21.0 and cannot
connect to a protocol-23 harness. Both SDK crates advance to the next pre-1.0
minor because public Rust types changed incompatibly. The published 0.1.0
actions and blocking-notify-channel leaf packages remain unchanged. Publish
proto first, then client; client pins proto exactly at `=0.29.0`.

External extensions should pin `dpc-tau-proto` and `dpc-tau-client` to
`=0.29.0`, regenerate their Cargo lockfiles, and rebuild. Do not bypass protocol
admission. Depending on the APIs your extension uses, migration from 0.28.0
requires these changes:

- `ArtifactOp::Begin` now requires `identity: ArtifactIdentity`. New uploads
  use `ArtifactIdentity::anonymous()` or `ArtifactIdentity::metadata(...)`.
  `ArtifactDescriptor` also carries `identity`; use
  `ArtifactDescriptor::from_bytes(bytes, identity)` for new objects and
  `descriptor.verify(bytes)` to verify the complete metadata-bound identity.
  `ArtifactDescriptor::new` still represents historical payload-only objects.
  Prefer the SDK upload/download helpers over reproducing the hashing scheme.
- Exhaustive `ToolResultPresentation` matches must handle
  `NativeCapturedOutput`, a harness-owned foreground-output presentation. It is
  not a new extension streaming opt-in. Existing background receipts remain.
- Initialize `updated: Vec<String>` in `DiscoveryContextDelivery` literals.
  Omitted wire fields default to an empty list.
- `TrustedInternalSpan` gains an `envelope` field, and
  `ContentPart::HarnessInternalText` gains `envelope: Option<SemanticEnvelope>`;
  `ToolFinalOutput` gains an `outcome` field.
- `ArtifactListEntry.signature` and `TAU_INTERNAL_HEADER_NAME` were removed.
- The public `ToolOutputPage` DTO was removed without an SDK replacement.
  Harness `tool_output` replies now use tool-response serialization.
- `tool_completion_receipt(call_id,outcome)` now returns a map with `cause` and
  `outcome` keys instead of `original_tool_call_id` and `target_outcome`.
  Receipt parsers should use the new keys and the `"canceled"` spelling returned
  by `ToolCompletionOutcome::Cancelled.as_str()`.

The package check exercises the extracted archives on Rust 1.91, including
protocol 23.1, metadata-bound artifact uploads and verification, native output
presentation, and discovery update identities. Run it without arguments before
publication and with `--registry` after both uploads:

```console
./.config/selfci/check-sdk-packages.sh
./.config/selfci/check-sdk-packages.sh --registry
```

This SDK-only publication does not bump the application version, publish
application assets, migrate extension repositories, change flake pins, deploy,
restart services, or reset stored state. Source-identifying
`dpc-tau-proto-v0.29.0` and `dpc-tau-client-v0.29.0` tags follow successful
registry verification.

### Published SDK 0.28.0

The published 0.28.0 source uses protocol **21.0**, which adds ordered tool report, update,
and final-capture types, declaration-frozen policy, and receipt-only background
waits. Configured extensions and UI peers require a coordinated rebuild. The
SDK package version is **0.28.0**. Publication does not rebuild configured
extensions or activate runtimes. See
[SPEC-streamed-tool-output](../specs/SPEC-streamed-tool-output.md).

Protocol 20.0 / source SDK 0.28.0 remove deprecated full-digest artifact keys.
Only the fixed 24-lowercase-hex identity is accepted in references and typed DTOs.
Rebuild configured extensions and UI peers together before activation. Old full-key
typed image/history records and upload receipts no longer decode; plain-text
history, current short-key records, and physical full-digest object paths remain
unchanged. There is no migration; SDK publication does not deploy a runtime.

Proto and client 0.28.0 were published in dependency order on November 8, 2026
(UTC) from clean source `c3af02b03cd8362b3128550611dcb82b9ee0b278`, after final
SelfCI, normal merge-queue run 2, and locked verified publish dry-runs passed.
The merge queue preserved that source identity. Radicle canonical master and the
GitHub mirror both identified that revision before either upload.

Both official sparse-index entries are unyanked. Downloaded registry archives
match the verified dry-run archives byte for byte. Their checksums match the index.
Their Cargo VCS metadata identifies that clean source and the respective
`crates/tau-proto` and `crates/tau-client` directories:

| Package | SHA-256 |
| --- | --- |
| `dpc-tau-proto` | `cb6c3e17345a659f933bb12ef3cb4f7b545b23ff3428adb3698ceac0e105d450` |
| `dpc-tau-client` | `251232d9d86d23626671df39d079fe8fddf7cb8c9788518cdfb9e900eab93114` |

The registry-only consumer passed all 18 tests on Rust 1.91.1 with a fresh Cargo
home and no local SDK patches. It checks protocol 21.0, the streamed output,
capture, frozen policy, receipt, and compaction-effectiveness surfaces, and
rejection of removed legacy artifact keys.

After registry acceptance, the source-identifying `dpc-tau-proto-v0.28.0` and
`dpc-tau-client-v0.28.0` tags were published at that source on Radicle and
GitHub. The unchanged 0.1.0 leaf packages were not republished. This publication
did not change the application version, publish an application release, migrate
external extensions, activate services, or reset stored state.

### Unpublished source SDK 0.27.0

Proto/client 0.27.0 targets protocol 19.1 and adds optional
`AgentCompacted.effectiveness`: four same-snapshot token estimates
(prefix and total before compaction, replacement and total after compaction)
with a versioned estimator identity.
Mixed 19.x readers can omit or ignore the field; missing measurements are unknown.
Source consumers constructing boundary literals must initialize the new field.
Provider usage fields, byte-growth policy, journal framing, and peer admission
are unchanged. This source change does not publish crates or activate runtimes.

### Unpublished source SDK 0.26.0

Proto/client 0.26.0 targets protocol 19.0 and emits artifact keys with 24 lowercase
hex digits (96-bit BLAKE3, no algorithm label). At that revision, parsers retained
full keys; protocol 20 removes them. Older peers cannot decode short keys, so configured
extensions and UIs must rebuild together. Journal framing and existing artifact
paths remain unchanged. This source update does not publish packages or activate
installed binaries.

Canonical-main bridge receivers require Configure's advertised protocol major to
match the SDK's compiled major, currently 21, and accept all minors within that
major. This replaces the stale guard for the feature's introduction in protocol
16.5. Missing revisions and other majors, including future majors, fail closed.
Ordinary receiver requests remain unchanged.

### Unpublished source SDK 0.25.0

Proto/client 0.25.0 targets protocol 18.0: fixed 32 MiB outer framing,
independent Configure artifact limits and larger chunked original objects.
Configured extensions and UI peers rebuild together; source Configure literals
must initialize `artifact_limits` and box `settings_files`, now a
`Box<BTreeMap<...>>` with unchanged wire encoding. Boxing keeps the startup-only
Configure size from inflating every runtime message enum after the addition of
`ArtifactLimits`. Client image/clipboard download defaults,
semantic producer budgets and durable record limits remain unchanged. No package
publication or runtime activation is implied.

### Unpublished source SDK 0.24.0

Proto/client 0.24.0 targets protocol 17.0. New typed size failures and
growth-stopped compaction recovery require configured extensions and UI peers
to rebuild together. Compaction reports and boundaries add an optional
ordinary-input byte comparison; Rust struct literals must initialize it.
Older journals remain readable with an unknown comparison. These source changes
do not publish packages or activate installed binaries.

### Published SDK 0.23.0

Proto/client 0.23.0 adds an opt-in shared build-script helper without changing
protocol 16.7. The helper verifies ownership of the selected source root and watches
Git state and tracked files so Cargo refreshes metadata after commits or dirty-state
changes. The no-argument `package_build_identity!()` reads the captured values at
compile time.
The helper reports unknown for tracked inputs it cannot reliably watch. The
integration recipe describes the missing-file, gitlink, and symlink limits.
Bundled extensions and core use the same helper; package stamps still take
precedence. See [the integration recipe](../crates/tau-client/README.md#build-identity).

Proto and client 0.23.0 were published in dependency order from clean source
`00206cb06b0d6634d632ed692d0b6165a9838157` after final SelfCI, merge-queue run 33,
and locked publish dry-runs passed. Both official sparse-index entries are
unyanked. Downloaded archives match the local publication archives and index
checksums. Their Cargo VCS metadata identifies that exact source and each
package's crate directory:

| Package | SHA-256 |
| --- | --- |
| `dpc-tau-proto` | `e358f4110abb9ef6be08450bbf97bbf9f0b7c5882046697333ad0374f0de8c6c` |
| `dpc-tau-client` | `81e6998d9d7f3633e2053488f477b07a2a06a841f7ddc9dd76bb72631dc45d8e` |

The registry-only consumer passed all 15 tests on Rust 1.91 using a fresh Cargo
home without local SDK patches. At verification, GitHub and Radicle master
pointed to that source, as did the `dpc-tau-proto-v0.23.0` and
`dpc-tau-client-v0.23.0` tags on both remotes.
The 0.1.0 leaf packages and application version were unchanged. This publication
did not activate or restart any extension or service.

### Published SDK 0.22.0

Protocol 16.7 adds optional build identity to Hello. Proto/client source versions
are 0.22.0; the SDK leaf versions are unchanged. Existing 16.x peers remain
compatible, but producers must adopt the new field to report build metadata.
Publication does not rebuild or activate downstream extensions.
See [the client integration recipe](../crates/tau-client/README.md#build-identity).

The original recipe's plain `built` fallback is not a provenance guarantee:
ancestor discovery can attribute an archived package to an unrelated consumer's
repository, and Cargo can retain stale Git metadata after commits. The
temporary-directory archive test did not cover those cases. Prefer the corrected
0.23.0 capture helper or independently validated caller metadata; the 0.22.0 wire field and
shared runtime stamp support remain usable.

Proto and client 0.22.0 were published in dependency order from clean source
`c817c724fe995bef09091ad3efac22ee95ead9ca` after merge-queue run 32 and locked
publish dry-runs passed. Both official sparse-index entries are unyanked.
Downloaded registry archives match the index checksums and identify that exact
source and their respective crate directories in Cargo VCS metadata:

| Package | SHA-256 |
| --- | --- |
| `dpc-tau-proto` | `1c4550e86e6ac0608309d7b1cdaec6893c9a5d984bf1d99fb19546b14cc0108e` |
| `dpc-tau-client` | `5ff750c6f72452f399a63e0bbfa4223bd99b3e5a1a61f2bf3ad24d00bbcb5045` |

The registry-only SDK consumer passed all 15 tests on Rust 1.91 with an isolated
Cargo home and no local SDK patches. At verification, GitHub and Radicle master
pointed to the published source. The `dpc-tau-proto-v0.22.0` and
`dpc-tau-client-v0.22.0` tags identify that source on both remotes. The unchanged
0.1.0 leaf packages were not republished.
Application tag `v0.3.0` remains at `3eae7dc621b3f2d2f953a182b85cde03c0037fbb`.
This SDK publication included no application release, deployment, configuration
change, or service restart.

### Published SDK 0.21.0

Protocol **16.6** adds optional typed message links and linked-message retrieval
metadata. A message's own ID, branch/thread `root_id`, immediate same-conversation
`previous_id`, and separate native `reply_to_id` let agents recover specific context
without a search or history-list interface. Bridges supply each link only with
reliable evidence; links are descriptive, not read or write capabilities.
Bridge-owned retrieval tools check current bridge policy and return untrusted
current content rather than an archive.
Proto/client versions are **0.21.0**, with client pinning proto `=0.21.0`; the leaf
packages remain 0.1.0. See the linked-message retrieval contract in
`specs/SPEC-external-message-reports-and-facts.md`.

Proto and client 0.21.0 were published in dependency order from
`fdba964291e17e16a25aec68ef85b949c9d5a2a0`. The verified official sparse-index
records are unyanked, and downloaded registry archives identify that exact source
in Cargo VCS metadata:

| Package | SHA-256 |
| --- | --- |
| `dpc-tau-proto` | `2c532c8b5b1fe51bcdef03aadc8da00a1de9c79bcf7bf7487c9000c5dac024ac` |
| `dpc-tau-client` | `93afd8b82773d9989bac57504e9b9d9397035661d4d89ecf99299e9842cd1433` |

The source passed merge-queue run 18. GitHub and Radicle master identified the
published source at verification; both `dpc-tau-proto-v0.21.0` and
`dpc-tau-client-v0.21.0` tags identify it exactly. The registry-only SDK consumer
passed all 14 tests on Rust 1.91 without local dependency patches. Application tag
`v0.3.0` remains unchanged at `3eae7dc621b3f2d2f953a182b85cde03c0037fbb`.
Package publication does not deploy or activate any bridge, change live grants,
or restart services.

### Published SDK 0.20.0

Protocol **16.5** adds `BridgeReceiverTarget::{Ordinary, CanonicalMain}`.
`BridgeReceiverRequest.target` and `BridgeReceiverConfig.target` default to
`Ordinary`; existing Rust struct literals must supply the field. Proto/client
source versions are **0.20.0**, with client pinning proto `=0.20.0`; the leaf
packages remain 0.1.0. Package publication and activation are separate.

`BridgeReceiverClient::start_request` retains ordinary behavior. Bridges opting in
use `start_request_with_target(session_id, role, mode, target,
harness_protocol_version)` for every operation, passing Configure's advertised
revision. The client rejects canonical-main requests unless that revision is
16.5 or newer within 16.x, preventing older harnesses from ignoring the
constraint. Shared `resolution_mode` permits main Ensure without an explicit
role, but only with `register_on_start` and admitted input.

Harness config `tool_policy.canonical_main_only` lists exact final registered
internal tool names, such as `fedi_zulip_send` when the assigned tool prefix is
`fedi_zulip` and the local tool is `send`. Entries are exact tool names, not
instance names, aliases, groups, or tag patterns. Role/model grants and old prompt
snapshots cannot bypass the final committed-main identity check. A same-role worker
with a withheld `required_tools` entry fails prompt validation rather than
receiving the tool.

Receiver targeting and outbound tool restriction are separate controls; apply
both to a main-only bridge. Manage dedicated Zulip credentials, permitted DM
participants, routes, and prompt fragments in the deployment and bridge.
Serve bootstrap now targets canonical main when configured and rejects a
conflicting startup role. Tau skips previously admitted IDs and does not promote
or migrate old ordinary bootstrap agents. Bootstrap admission is at most once;
it does not guarantee model completion. You can also start a fixed session
without bootstrap prompt/id, then submit once with
`tau message '&SESSION' < recovery.txt` after readiness.
Use `tau serve --session SESSION --create` only for an absent session; use
`--existing` or the explicit `--create-or-existing` policy for restart.
Do not retry a message without checking delivery if you lost its acknowledgement.

Proto and client 0.20.0 were published in dependency order on November 4, 2026
from `70e8a4eb068c9daf427aa37853cac23bed8048e3`. Both official sparse-index
records are unyanked. The downloaded registry archives match the verified bytes
from locked dry-runs. Their Cargo VCS metadata identifies the same clean source:

| Package | SHA-256 |
| --- | --- |
| `dpc-tau-proto` | `cc9bbcae914828b6bd22c752ddd2c473163ebb6d6c61ab93df0821d10638ecd9` |
| `dpc-tau-client` | `42e98315efa67bdbbfbdaa8fc0588c114ee5ee415ba5c9834d10324cfb260331` |

The registry-only SDK consumer passed all 13 tests on Rust 1.91 with a fresh,
isolated Cargo home and no local dependency patches. The source passed independent
review and final local CI, then landed through normal MQ. The immutable
`dpc-tau-proto-v0.20.0` and `dpc-tau-client-v0.20.0` tags identify the published
source on GitHub and the Radicle owner namespace. Application tag `v0.3.0` remains
unchanged. Zulip API adoption, deployment activation and service restarts remain
separate from this publication.

### Published SDK 0.19.0

Protocol **16.3** adds optional descriptive `mention_evidence` to
`MessageDelivered` and `MessageEdited`. Add `mention_evidence: None` to existing
public struct literals unless opting in; constructors default to absent.
Proto/client source versions are **0.19.0**, and client pins proto `=0.19.0`;
the two unchanged leaf packages remain 0.1.0.

The evidence carries independently optional bridge-self observations and a
potentially partial participant list. Optional validated UTF-8 byte spans map
participants to exact tokens in the final fact body. Unknown is not false; each edit
supplies current evidence or unknown, without inheriting metadata. Mentions, sender
admission, and reply capability do not establish an invitation to respond. The source API and projection
preserve existing participant disclosure policies and exact message text.

Protocol **16.4** also adds optional `reply_context` to delivered and edited
reports and facts. Add `reply_context: None` to existing public struct literals
unless opting in; constructors default to absent. When parent text is supplied,
the renderer projects one incoming fact as adjacent `message` and `quoted_message`
text parts. Reference-only replies and thread roots need no empty quote. Sender,
admission, routing, ownership, and activation remain those of the outer message.
Each edit supplies its chosen frozen snapshot; projection does not
inherit a prior quote.

Proto and client 0.19.0 were published in dependency order on November 4, 2026
from `170d4b513af0e2558788760fbe6aae7e8b7f8b19`. Both official sparse-index
records are unyanked. The downloaded archives match the verified dry-run bytes
and identify the same clean source in Cargo VCS metadata:

| Package | SHA-256 |
| --- | --- |
| `dpc-tau-proto` | `7c89a9f94ae029854a0a49118962032d0695fd11fd305c0c701d6357976e7d42` |
| `dpc-tau-client` | `caa0d58e339e3d4bc0c16661b256b09d3290c7502de89b3f2c830a5ca1469de1` |

The package check passed all 12 tests on Rust 1.91 using a fresh, isolated Cargo
home and only registry dependencies. The
`dpc-tau-proto-v0.19.0` and `dpc-tau-client-v0.19.0` tags identify that source on
Radicle and GitHub. Source publication used normal MQ and passed final local CI.

This SDK-only release is separate from application 0.3.0 and SDK 0.18.0; the
immutable application tag remains unchanged. Existing 16.x producers remain
admitted, and older readers may ignore the optional fields. To expose the new
metadata, update the producer's proto/client pins, rebuild the extension, and use
an updated harness. Publication does not deploy or activate those binaries.

### Published SDK 0.18.0

Protocol **16.2** adds typed discovery snapshots and passive delivery receipts.
The new public `discovery_items` field on `AgentInitializationContextSet` and
`discovery_context` fields on `AgentUserMessageInjected` and `RenderedPromptResult`
require a new Rust SDK minor line, **0.18.0**. The client pins proto `=0.18.0`;
the two unchanged leaf packages remain 0.1.0.

Consumers constructing these structs must supply the new fields. Discovery DTOs
describe applicable guidance, newly delivered text and preview content; a passive
receipt does not activate inference on its own. Missing fields in older
records default to empty or absent. Older 16.x extensions may ignore the metadata
and remain admitted; this addition does not require all extensions to rebuild.

Proto and client 0.18.0 were published on November 3, 2026 from
`3eae7dc621b3f2d2f953a182b85cde03c0037fbb` for application 0.3.0.
Both unyanked registry archives matched their verified dry-run checksums and clean
source identity. A fresh registry-only consumer passed all 11 tests on Rust 1.91.

### Published SDK 0.17.0

Protocol **16.1** adds optional `MessageDelivered.presentation`, with
`Some(MessagePresentation::Notification)` hiding model-facing message references,
sender references, and sender display. Constructors default to `None`; consumers
using struct literals must add `presentation: None` unless they opt in. Internal
identifiers, sender validation, acknowledgement, activation, and trust metadata
are unchanged. Older 16.x peers ignore the hint and keep full presentation, so
existing extensions do not need to rebuild.

The new public field breaks existing Rust struct literals, so this change uses
`dpc-tau-proto` and `dpc-tau-client` **0.17.0** while keeping wire protocol
**16.1**. The client pins proto `=0.17.0`; both leaf packages remain 0.1.0.
Both packages are published and have passed registry-only consumer acceptance;
see the acceptance record below. Activation remains separate.

### Published SDK 0.16.0

Protocol **16.0** and proto/client SDK **0.16.0** add an optional canonical session
main identity and peer-message review. The client pins proto `=0.16.0`; the two
leaf packages remain 0.1.0. Both packages are published and have passed
registry-only consumer acceptance; see the acceptance record below.

Consumers must update struct literals for `AgentMessageSent.review_recipient`,
`AgentMessageReceived.peer_review`, `SessionAgentLoaded.main`,
`SessionReplayComplete.main_agent_id`, and
`ExternalAgentMessageResult.review_recipient`. They must handle
`Event::AgentRuntimeUnavailable`, `HarnessInputMessage::UiEnsureMainAgent`, and
`UnloadSessionAgentOutcome::MainAgent`. Runtime unavailability does not unload
session membership. `PeerMessageReview::Pending` names the intended worker;
`Released` retains a `PeerMessageReviewReference` identifying main and the durable
event sequence. An acknowledgement naming main with `review_recipient` confirms
acceptance for review; it does not confirm delivery to the intended worker.
Client function signatures are unchanged.

Rebuild configured extensions and UI clients against this SDK before activation.
Compatible external rebuilds, downstream configuration and lock updates, and
activation remain separate gates. New readers accept older mainless history;
old readers cannot decode the new variants. Journal framing is unchanged.
See the [main-agent migration guide](main-agent-migration.md) for configuration
and consumer updates.
See [SPEC-session-main-agent](../specs/SPEC-session-main-agent.md).

### Published SDK 0.15.0

Protocol **15.0** adds `MessagePhase::PartialAnswer` and
`ProviderStopReason::Continue`. The published proto/client packages are **0.15.0**
with an exact client-to-proto pin; the two leaf packages remain 0.1.0.
Exhaustive enum matches must handle the new variants. A `Continue` response is
not a completed outer turn, and partial-answer text must not become a final-answer
summary. Existing journals remain readable; old binaries cannot decode newly
written enum values. There is no downgrade migration.

Rebuild configured extensions and UI clients against this SDK before activation.
Registry publication and registry-only consumer verification passed; see the
acceptance record below. The Tau flake pins compatible, independently published
peers; downstream lock updates and activation remain separate rollout gates. See the
[protocol-15 rollout checklist](protocol-15-rollout.md).

### Published SDK 0.14.0

Protocol **14.0** uses published `dpc-tau-proto` and `dpc-tau-client` **0.14.0**,
with an exact client-to-proto pin of `=0.14.0` and unchanged leaf dependencies at
0.1.0. All configured peers must rebuild together before activation; a
protocol-13 peer is rejected before configuration. SDK publication does not
install or restart any extension.

Source migration:

- Handle `ToolResultContentPart::ImageReference` separately from inline `Image`.
  Its descriptor addresses the exact prepared image, not the original artifact.
- `ToolInvocationPolicy` gains `prepared_image_storage`; use
  `..Default::default()` for ordinary initializers. `Inline` is the default.
  Only the harness may select `Artifact` for a captured invocation.
- Generic observers must remove fetchable keys, not merely image bytes. Shared
  projection helpers do this; derived `Debug` also hides the reference key.
- New readers accept legacy inline history. Old readers cannot decode new
  reference-bearing records; no downgrade migration or journal-only backup
  guarantee is provided. Physical journal framing is unchanged.

The source endpoint captures Artifact for authorized persistent image invocations
in durable sessions once scoped persistence authority is Prepared. Ephemeral,
memory-only and not-yet-prepared invocations stay Inline. Producer upload,
verified/rooted admission and provider fetch/cache are integrated; unsupported
provider routes omit historical images without fetching them. Registry-only
acceptance has passed for 0.14.0. External rebuild, downstream closure and
activation gates remain separate; see the
[prepared-image rollout checklist](prepared-image-rollout.md).

### Published SDK 0.13.0

Protocol **13.1** uses published `dpc-tau-proto` and `dpc-tau-client`
**0.13.0** package line, with unchanged leaf dependencies at 0.1.0 and an exact
client-to-proto pin of `=0.13.0`. It adds UI-only
artifact discovery enum variants and verified clipboard export to the preceding
unpublished 0.12.0 / protocol 13.0 work. It includes
authenticated delivery publishers, generic custom-event agent privacy scope,
and passive context append requests/results/facts. It replaces live first-party
shell routing with an opaque endpoint protocol. Configured extensions must
rebuild together with the harness. Journal framing is unchanged; new binaries
read legacy shell records, but older readers cannot decode new passive-append
or failure variants. Do not roll back after writing them without a compatible
reader. SDK publication does not install or activate compatible extension binaries.

Since published SDK 0.10.0 / protocol 10.2, the public API gained provider error
categories, CLI message DTOs and optional CLI-origin metadata, typed
exhausted-transient compaction failure variants, and optional original discovery
paths and unusable-local-summary failure authority. These field and enum additions require a new pre-1.0 minor SDK line.
Configured protocol-10 extensions are rejected before Configure/Ready by a
protocol-13 harness and must migrate to the actual SDK API, not merely advertise
a new wire version. The 0.13.0 archive consumer check verified protocol 13.1 and its
typed failure variants under Rust 1.91; registry-only acceptance must follow
publication before source-identifying package tags are created; this acceptance
has passed for 0.13.0.

## Protocol 16.1 release acceptance

Both packages were published on October 29, 2026 from clean source revision
`fec75e54754187cea087afb19684b2d9b6f48f61`, after locked Cargo publish
dry-runs passed verification in dependency order. Both official sparse-index
entries are unyanked. Downloaded registry archives match the index checksums.
Their Cargo VCS metadata identifies that clean revision and the respective
`crates/tau-proto` and `crates/tau-client` subdirectories.

| Package | Registry archive SHA-256 |
| --- | --- |
| `dpc-tau-proto 0.17.0` | `1018c7ed01b361cab8adb3198da39eae751caabced258779ec99e6fb066fe5e1` |
| `dpc-tau-client 0.17.0` | `d79d99b50033c279c13f6bb42fa35a42eedbc1423c035c586abe454dd5c21061` |

The unchanged 0.1.0 leaf packages were not republished.
`./.config/selfci/check-sdk-packages.sh --registry` passed all ten consumer
tests on Rust 1.91 with an isolated Cargo home and no local SDK patches.

After acceptance, `dpc-tau-proto-v0.17.0` and `dpc-tau-client-v0.17.0` were
published at the exact source revision on Radicle and the GitHub mirror.
Both public remotes were verified. This documentation follow-up does not move
those package tags or change archive provenance.

This acceptance does not publish an application release, update external
repositories or personal configuration, activate a system, or restart services.
Wire protocol remains 16.1, and older 16.x peers remain compatible.

## Protocol 16.0 release acceptance

Both packages were published on October 28, 2026 from clean source revision
`37f7518558d230f3d0e8edf791c2b850d3d49a8f`, after locked Cargo publish
dry-runs passed verification in dependency order. Both official sparse-index
entries are unyanked. Downloaded registry archives match the index checksums.
Their Cargo VCS metadata identifies that clean revision and the respective
`crates/tau-proto` and `crates/tau-client` subdirectories.

| Package | Registry archive SHA-256 |
| --- | --- |
| `dpc-tau-proto 0.16.0` | `09ecbaf380ca7e53330753e61c95b1b061fad6f94ec7843141ce9ec1b37309fa` |
| `dpc-tau-client 0.16.0` | `109243178a6a4b462e0ecfc7b017bc9acb9d0dc4edf0ffbb2ee82d59fcbc5406` |

The unchanged 0.1.0 leaf packages were not republished.
`./.config/selfci/check-sdk-packages.sh --registry` passed all nine consumer
tests on Rust 1.91 with an isolated Cargo home and no local SDK patches.

After acceptance, `dpc-tau-proto-v0.16.0` and `dpc-tau-client-v0.16.0` were
published at the exact source revision on Radicle and the GitHub mirror; both
public remotes were verified. This documentation follow-up does not move those
package tags or change archive provenance.

This acceptance does not publish an application release, update external
repositories or personal configuration, activate a system, restart services, or
authorize old readers to open new journal variants.

## Protocol 15.0 release acceptance

Both packages were published on October 27, 2026 from clean source revision
`a82ab5f0e11a28997e93c56dc70059c6a7c24c1f`, after locked verified Cargo
publish dry-runs and verification in dependency order. Both official sparse-index
entries are unyanked; downloaded registry archives match their checksums and
identify that clean revision and the correct crate subdirectories in Cargo VCS
metadata.

| Package | Registry archive SHA-256 |
| --- | --- |
| `dpc-tau-proto 0.15.0` | `727c730e075f8df6542fe5a0fba1ddc16e7b7a725670a863b9ae38d8badc1c04` |
| `dpc-tau-client 0.15.0` | `37e525d9a495e4433dd847460f084914460bf5a5ffa698419114ad24fe580333` |

The unchanged 0.1.0 leaf packages were not republished. Their source still matches
`2243c6857446bd253a505ce1ba20671a445d899c`; their downloaded archives match
official unyanked index checksums and original clean provenance.
`./.config/selfci/check-sdk-packages.sh --registry` passed all seven consumer
tests on Rust 1.91 with an isolated Cargo home and no local SDK patches.

After acceptance, `dpc-tau-proto-v0.15.0` and `dpc-tau-client-v0.15.0` were
published at the exact source revision on Radicle and the GitHub mirror.
SDK publication did not advance application master. Consumers that require
source-only validation may use the GitHub URL with this exact revision; fresh
Cargo homes passed both default libgit2 and CLI locked-update checks there.
Telegram retains its own-host namespace/tag validation source.
Extensions use exact registry SDK 0.15.0 runtime dependencies, not these
validation-only source dependencies.

This acceptance does not publish an application release, activate a system,
restart services, or authorize old readers to open new journal variants.

## Protocol 14.0 release acceptance

Both packages were published on October 18, 2026 from clean source revision
`fbdf287f452db63fc387509535f5c33777f4f04e`. The approved eight-commit source stack
passed the corrected normal merge queue, and the public Radicle canonical master
was verified at that exact revision before the uploads.

Locked verified Cargo publish dry-runs passed in dependency order: proto first,
then client after proto was available from the registry. Both official sparse
index entries are unyanked. Their checksums match both the downloaded registry
archives and the preserved verified dry-run archives. Cargo VCS metadata identifies
the same clean source revision and the respective `crates/tau-proto` and
`crates/tau-client` subdirectories.

| Package | Registry archive SHA-256 |
| --- | --- |
| `dpc-tau-proto 0.14.0` | `78980b230431a9d33f4cc58c0627dca5a28d7da0045ecc664374e4c943f68872` |
| `dpc-tau-client 0.14.0` | `5359cf5c7eb3d87a14ccdbefc792d43c6d7fa44b8d7da04caf740fec944f1499` |

The unchanged 0.1.0 leaves were checked against their official index checksums
and clean provenance at `2243c6857446bd253a505ce1ba20671a445d899c`; their Rust source
files remain unchanged. The registry-only acceptance command
`./.config/selfci/check-sdk-packages.sh --registry` passed all six consumer tests
on Rust 1.91 from a fresh isolated Cargo home, with no local SDK patches. This
includes protocol 14.0 and the actual prepared-reference/invocation-policy API,
not merely advertised version numbers.

Only after registry acceptance, the Radicle tags `dpc-tau-proto-v0.14.0` and
`dpc-tau-client-v0.14.0` were created and published at the exact package source
revision. The public seed's owner-namespaced refs were verified. The documentation
acceptance follow-up does not move those immutable package tags or change archive
provenance.

This is SDK and source publication, not an application registry/native/GitHub
release or deployment. No external extension repository, installed binary or
live state was migrated by this publication.

## Protocol 13.1 release acceptance

Both packages were published on October 14, 2026 from clean source revision
`04e2610481bfbdabfeb582d7360f8ac5797f06e9`. Locked verified Cargo publish dry-runs
passed in dependency order. The official sparse-index checksums matched the
downloaded registry archives, whose Cargo VCS metadata identifies that revision
and the respective `crates/tau-proto` and `crates/tau-client` subdirectories.

| Package | Registry archive SHA-256 |
| --- | --- |
| `dpc-tau-proto 0.13.0` | `fa5a4e4a10255474547c3a53da1d0b803332fb1a3660ffc17f7a1d541e71a035` |
| `dpc-tau-client 0.13.0` | `f09fee020a05ad7f8f8cf9ad6884c194be84fe71ae42ab2dcd43bb0e56f83b4d` |

The unchanged 0.1.0 leaf archives were checked against their official index
checksums and original clean provenance at
`2243c6857446bd253a505ce1ba20671a445d899c`.
`./.config/selfci/check-sdk-packages.sh --registry` passed all five consumer
tests on Rust 1.91 without local SDK patches. After that acceptance, the Radicle
tags `dpc-tau-proto-v0.13.0` and `dpc-tau-client-v0.13.0` were published at the
exact source revision; the public seed's owner-namespaced refs were verified.
The source passed the normal merge queue and is public on Radicle.
This is an SDK and source publication, not a new application registry/native
release or a deployment. Protocol-10 extension binaries remain incompatible;
each owning project must publish its actual migration before its Tau flake pin
or installed binary can be updated.

## Earlier package lines

Protocol **10.2** uses published `dpc-tau-proto` and `dpc-tau-client` **0.10.0**,
with their unchanged leaf dependencies at 0.1.0. Configure's default-empty
optional-secret absence metadata adds a public Rust field, so constructing it
requires a source update and a new pre-1.0 minor SDK line. Client pins exact proto
`=0.10.0`. Older protocol-10 extensions ignore the additional field and retain
their previous strict missing-reference behavior; the seven native external
projects retain SDK 0.8.0 / protocol 10.0 and existing best-effort admission.
Both packages were published on September 29, 2026 from
`009b3c3f0cc60435402ba6146d20943980f3f048`; their exact registry checksums and
clean archive provenance were verified, and a registry-only consumer passed
on Rust 1.91.

The protocol `10.1` SDK release uses `dpc-tau-proto` and `dpc-tau-client`
`0.9.0`; both leaf dependencies remain `0.1.0`. The optional Configure harness
revision field and papercut-history enum variants change Rust source construction
and exhaustive matching, so they require a new pre-1.0 minor SDK line.
The client pins exact proto `=0.9.0`. Both packages were published on September
27, 2026 from `4761e99c0375d0b8a4eb8046922b8f3790675398`; a registry-only
consumer passed on Rust 1.91. Protocol 10.0 extensions built with SDK 0.8.0 remain admitted with
a same-major warning and best-effort operation; they do not need the optional
10.1 papercut-history operations. Cargo source compatibility and wire admission
remain separate.

The protocol `10.0` SDK release uses `dpc-tau-proto` and `dpc-tau-client`
`0.8.0`; both leaf dependencies remain `0.1.0`. Workdir-driven discovery
refresh adds public DTO fields and events. A setter now waits for the harness's
installed acknowledgement, which older harnesses cannot produce. Configured
extensions must rebuild together for protocol 10; UI and cooperative peer
best-effort skew exceptions are unchanged. This SDK release is published and
accepted.

The protocol `9.0` SDK release uses `dpc-tau-proto` and `dpc-tau-client`
`0.7.0`; both leaf dependencies remain `0.1.0`. Directed bridge
receiver resolution adds source-incompatible public enum variants. Older
harnesses cannot answer this operation, so all configured extensions must be
rebuilt for protocol 9 before rollout, even when their receive behavior is
unchanged.

The protocol `8.1` SDK release uses `dpc-tau-proto` and `dpc-tau-client`
`0.6.0`, with their unchanged leaf dependencies remaining at
`dpc-tau-actions` and `dpc-tau-blocking-notify-channel` `0.1.0`. Since the
`0.5.0` release, protocol 7.3 added public context-visibility, discovery
diagnostic, and eligible-skill projection fields and types, while protocol 7.4
added optional compaction activity counts, protocol 7.5 added hosted-tool
definitions to cache-maintenance requests, protocol 8.0 separated provider
terminal reports from canonical responses and made final-status authority
mandatory, and protocol 8.1 admitted upload-only Artifact RPC for authenticated
UIs. These accumulated public field and type changes move the source API to the
new `0.6` minor line. The `0.6.0` package line was not published before these
later protocol changes, so its first registry release contains the complete
protocol 8.1 SDK.

The protocol `7.2` SDK release uses `dpc-tau-proto` and `dpc-tau-client`
`0.5.0`, with their unchanged leaf dependencies remaining at
`dpc-tau-actions` and `dpc-tau-blocking-notify-channel` `0.1.0`. Since the
`0.4.0` release, protocol 7.1 added typed inter-session notice policy and
notice fields, while protocol 7.2 added per-agent effort selection and
override fields. These accumulated public field and type additions change Rust
struct construction, so the source API moves to the new `0.5` minor line.

The protocol `7.0` SDK release uses `dpc-tau-proto` and `dpc-tau-client`
`0.4.0`, with their unchanged leaf dependencies remaining at
`dpc-tau-actions` and `dpc-tau-blocking-notify-channel` `0.1.0`. Protocol 7
adds typed sender-trust metadata to external message parties. Adding the field
changes Rust struct construction, so the source API moves to the new `0.4`
minor line. Extensions compiled for protocol 6 are rejected before
configuration and must be rebuilt or updated together with the harness.

The protocol `6.0` SDK release uses `dpc-tau-proto` and `dpc-tau-client`
`0.3.0`, with their unchanged leaf dependencies remaining at
`dpc-tau-actions` and `dpc-tau-blocking-notify-channel` `0.1.0`. Protocol 6
adds directed shared Artifact transfers and extends provider tool declarations
with optional provider scope. These source API changes require the new `0.3`
minor line. Extensions compiled for protocol 5 are rejected before
configuration and must be rebuilt or updated together with the harness.

The protocol `5.0` SDK release uses `dpc-tau-proto` and `dpc-tau-client`
`0.2.0`, with their unchanged leaf dependencies remaining at
`dpc-tau-actions` and `dpc-tau-blocking-notify-channel` `0.1.0`. Protocol 5
removes the obsolete delegate-progress compatibility surface and adds the
closed provider-attempt timing capture classification. Extensions and UI
clients compiled for protocol 4 are rejected before configuration and must be
rebuilt or updated together with the harness.

Cargo package versions describe Rust source API compatibility. During the
pre-1.0 series, compatible releases remain within the current `0.x` minor
line; a source-incompatible SDK API change increments that minor version.
Workspace dependencies use both a local path and an ordinary Cargo version
requirement, so local builds use sibling source while published packages
resolve the registry release.

The protocol revision is independent of every Cargo package version. A package
release does not require a protocol bump unless the harness-extension boundary
changes, and a protocol bump does not prescribe a matching package number. See
[`SPEC-extension-protocol-versioning`](../specs/SPEC-extension-protocol-versioning.md)
for admission behavior and the protocol boundary.

This mapping does not describe or promise journal physical-format
compatibility.

## Protocol 10.0 release acceptance

The package readiness check verifies the complete SDK archive set:

```console
./.config/selfci/check-sdk-packages.sh --registry
```

The protocol `10.0` SDK release is published and accepted. Registry
`dpc-tau-proto` and `dpc-tau-client` `0.8.0` both come from source revision
`cd852ff26dcdad8027aea0bc2fd396c1ca61f9fb`; the client requires exact proto
`=0.8.0`. The registry-only SDK consumer check passes under the supported Rust
1.91 environment and verifies protocol 10.0 without local patches. The
`dpc-tau-proto-v0.8.0` and `dpc-tau-client-v0.8.0` Radicle tags both resolve to
that source revision.

The accepted registry archive SHA-256 values are
`1c01022ecd4ec9f8f117d346f5a60fd4c46747671a64bbfaf5781b8177f11bdd`
for proto and
`2788dd292eb94c457cc672494df79d46515a92a464cae7f9d5c86634f41bdf6c`
for client. Both registry archives' Cargo VCS metadata identifies the same
source revision and the expected package subdirectory.

## Protocol 9.0 release acceptance

The package readiness check verifies the complete SDK archive set:

```console
./.config/selfci/check-sdk-packages.sh
```

The check creates all four package archives, inspects their normalized
manifests, and builds a small consumer outside the workspace against the exact
archives. Its consumer uses temporary Cargo patches unless `--registry` is
selected.

The protocol `9.0` SDK release is published and accepted. Registry
`dpc-tau-proto` and `dpc-tau-client` `0.7.0` both come from source revision
`51bee43399254519e1c86164aa9301713b9f8cd3`; the client requires exact proto
`=0.7.0`. The registry-only SDK consumer check passes under the supported Rust
1.91 environment and verifies protocol 9.0 without local patches. The
`dpc-tau-proto-v0.7.0` and `dpc-tau-client-v0.7.0` Radicle tags both resolve to
that source revision.

The accepted registry archive SHA-256 values are
`7211c62006283b33aa230bd95df8d29dc0f696727474cab029c800cc016bfc0a`
for proto and
`c004113aa616b701588e3b004dbb74d7781ab51f5c240780e43b05e59cf42fad`
for client.

## Protocol 8.1 release acceptance

The package readiness check verifies the complete SDK archive set:

```console
./.config/selfci/check-sdk-packages.sh
```

The check creates all four package archives, inspects their normalized
manifests, and builds a small consumer outside the workspace against the exact
archives. Its consumer uses temporary Cargo patches unless `--registry` is
selected.

The protocol `8.1` SDK release is published and accepted. Registry
`dpc-tau-proto` and `dpc-tau-client` `0.6.0` both come from source revision
`24def4156c132074912e86ad94f53b82ce871933`; the client requires exact proto
`=0.6.0`. The registry-only SDK consumer check passes under the supported Rust
1.91 environment and verifies protocol 8.1 without local patches. The
`dpc-tau-proto-v0.6.0` and `dpc-tau-client-v0.6.0` Radicle tags both resolve to
that source revision.

The accepted registry archive SHA-256 values are
`5f4bf56b5e0990944f4dd05228aa20122ce996d22f74c247ab83e6cb7cf596c7`
for proto and
`b373bc6b4d94d70fb60c568325c098daf7886dd2627d1fd1287a9031dcfa5ccf`
for client.
