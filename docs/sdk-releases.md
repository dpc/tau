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

### Unpublished SDK 0.16.0

Protocol **16.0** and proto/client SDK **0.16.0** add an optional canonical session
main identity and peer-message review. The client pins proto `=0.16.0`; the two
leaf packages remain 0.1.0. SDK 0.16.0 is ready in source but remains unpublished
and has not passed registry acceptance.

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

Run `./.config/selfci/check-sdk-packages.sh` for archive-source acceptance on Rust
1.91 before publication. Run locked verified Cargo publish dry-runs in dependency
order and publish proto before client. After publication, run the same check with
`--registry` from an isolated Cargo home without local SDK patches. Verify registry
checksums and clean archive provenance before creating source-identifying package
tags. Compatible external rebuilds and activation remain separate gates.
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
