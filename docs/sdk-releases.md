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

Protocol **10.2** is prepared as `dpc-tau-proto` and `dpc-tau-client` **0.10.0**,
with their unchanged leaf dependencies at 0.1.0. Configure's default-empty
optional-secret absence metadata adds a public Rust field, so constructing it
requires a source update and a new pre-1.0 minor SDK line. Client pins exact proto
`=0.10.0`. Older protocol-10 extensions ignore the additional field and retain
their previous strict missing-reference behavior; the seven native external
projects retain SDK 0.8.0 / protocol 10.0 and existing best-effort admission.
Preparation does not establish registry publication.

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
