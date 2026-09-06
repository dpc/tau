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

Publish `dpc-tau-actions` and `dpc-tau-blocking-notify-channel` first, followed
by `dpc-tau-proto`, then `dpc-tau-client`. Rust source continues to import
these packages as `tau_actions`, `tau_blocking_notify_channel`, `tau_proto`,
and `tau_client`.

## Package and protocol versions

The initial SDK package set is `0.1.0` and currently advertises extension
protocol revision `1.1`.

Cargo package versions describe Rust source API compatibility. During the
pre-1.0 series, compatible releases remain within `0.1.x`; a source-incompatible
SDK API change requires `0.2.0`. Workspace dependencies use both a local path
and an ordinary Cargo version requirement, so local builds use sibling source
while published packages resolve the registry release.

The protocol revision is independent of every Cargo package version. A package
release does not require a protocol bump unless the harness-extension boundary
changes, and a protocol bump does not prescribe a matching package number. See
[`SPEC-extension-protocol-versioning`](../specs/SPEC-extension-protocol-versioning.md)
for admission behavior and the protocol boundary.

This mapping does not describe or promise journal physical-format
compatibility.

## Release checkpoint

Run the package readiness check before requesting publication:

```console
./.config/selfci/check-sdk-packages.sh
```

The check creates all four package archives, inspects their normalized
manifests, and builds a small consumer outside the workspace against the exact
archives. Before the first registry release, the consumer uses temporary Cargo
patches to stand in for the unpublished packages.

After publication is authorized, dry-run and upload in dependency order:
`cargo publish --dry-run` immediately precedes publishing each leaf package,
then `dpc-tau-proto`, then `dpc-tau-client`. Do not upload a package whose
current dry-run fails.

After the complete set is available, run
`./.config/selfci/check-sdk-packages.sh --registry` to repeat the exact-version
consumer check without patches. Registry upload requires separate explicit
authorization; package readiness does not authorize publication.
