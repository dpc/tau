# Crates.io application release

Publishing `dpc-tau` requires publishing its internal Rust crate closure first.
Package preparation does not authorize uploads, tags, or GitHub releases.

## Tau 0.1.1 preparation

The application, CLI, and changed internal dependency closure are now **0.1.1**.
The CLI owns the embedded `tau --version` string, so both application and CLI
must advance together. Unchanged internal packages remain at workspace version **0.1.0**; the SDK remains
**0.5.0**. Release tooling reads `crates/tau/Cargo.toml`, not the workspace
default, for the application release version.

The complete 0.1.0 application closure has been published and the isolated
registry install passed. This preparation requires seven new uploads, in order:
`dpc-tau-skills`, `dpc-tau-ext-shell`, `dpc-tau-harness`,
`dpc-tau-harness-tools`, `dpc-tau-test-support`, `dpc-tau-cli`, and `dpc-tau`,
all at 0.1.1. Correcting the embedded extension documentation changes skills
and the harness's release-resource snapshot. Shell embeds skills, and its
dependents must use the corrected version. Test support depends on the harness
and is needed to verify the CLI package's dev-dependencies. This is the exact
reverse dependency closure, not a workspace-wide version bump. Do not republish
or replace any existing version.

This is a release prerequisite, not authorization to publish: complete native
extension packaging and qualification must pass before the coordinated 0.1.1
publication. The updated external pins select SDK 0.4.0 / protocol 7.0 sources;
they do not change the extensions' upstream 0.1.0 Cargo versions or the harness
protocol 7.2.

## Complete dependency closure

The application has 32 runtime crates. Cargo also resolves dev-dependencies
while verifying a package archive. `dpc-tau-cli` uses
`dpc-tau-test-support` for tests, so a fully verified dependencies-first
publication has 33 crates.

`dpc-tau-e2e-tests`, `dpc-tau-summary-eval`, and `dpc-tau-supervisor` are not
needed. They remain ordinary workspace packages rather than being marked
`publish = false`; that policy can be decided separately without obstructing
the application release.

Run the registry-independent metadata and file-selection check:

```console
./.config/selfci/check-crates-io-packages.py
```

It verifies complete package metadata, exact internal version requirements,
the dependency order below, and `cargo package --list` for every archive.
Literal non-test `include_str!` and `include_bytes!` inputs must be present in
the corresponding archive, and release-owned resource snapshots must match
their canonical workspace sources byte for byte. It does not claim that an
unpublished dependency exists in the registry.

## Publication order

The current dependencies-first order is:

```text
dpc-tau-actions                  0.1.0
dpc-tau-blocking-notify-channel  0.1.0
dpc-tau-themes                   0.1.0
dpc-tau-util-fs-err              0.1.0
dpc-tau-vcr                      0.1.0
dpc-tau-proto                    0.5.0
dpc-tau-client                   0.5.0
dpc-tau-config                   0.1.0
dpc-tau-core                     0.1.0
dpc-tau-delivery-memory          0.1.0
dpc-tau-skills                   0.1.1 (new)
dpc-tau-socket                   0.1.0
dpc-tau-term-screen              0.1.0
dpc-tau-ext-rhai                 0.1.0
dpc-tau-ext-std-notifications   0.1.0
dpc-tau-ext-test-dummy           0.1.0
dpc-tau-ext-utils                0.1.0
dpc-tau-ext-websearch            0.1.0
dpc-tau-provider                 0.1.0
dpc-tau-ext-shell                0.1.1 (new)
dpc-tau-cli-picker               0.1.0
dpc-tau-cli-term-raw             0.1.0
dpc-tau-provider-chat-completions 0.1.0
dpc-tau-provider-codex           0.1.0
dpc-tau-provider-responses       0.1.0
dpc-tau-session-inspect          0.1.0
dpc-tau-cli-term                 0.1.0
dpc-tau-ext-provider-builtin     0.1.0
dpc-tau-harness                  0.1.1 (new)
dpc-tau-harness-tools            0.1.1 (new)
dpc-tau-test-support             0.1.1 (new; package-verification dependency)
dpc-tau-cli                      0.1.1 (new)
dpc-tau                          0.1.1 (new)
```

All entries other than the seven new 0.1.1 versions are already published as of
September 14, 2026.

The published `dpc-tau-proto` and `dpc-tau-client` `0.4.0` archives cannot be
reused for this source. The workspace protocol is now 7.2. Protocol 7.1 added
public inter-session notice types and fields, and protocol 7.2 added public
per-agent effort control fields; all are absent from registry proto `0.4.0`.
Those source-breaking additions require proto `0.5.0`; client `0.5.0` pins and
publishes that new protocol line even though its own Rust source is otherwise
unchanged.

Separately maintained extensions pinned to registry SDK `0.4.0` continue to
advertise protocol 7.0. A protocol 7.2 harness admits that same-major minor
skew best-effort with a warning, but those binaries do not gain protocol 7.1
notices or protocol 7.2 effort controls. Exact `=0.4.0` Cargo pins also prevent
their source from resolving SDK `0.5.0` until each external project deliberately
updates. Updating those repositories is outside this release preparation.

## Upload-time procedure

For each unpublished entry, in the order above:

1. Confirm the exact version is absent using the official crates.io sparse
   index. Preserve HTTP errors; do not interpret every failed request as 404.
2. Run `CARGO_PROFILE_DEV_DEBUG=false cargo publish --locked --dry-run --package <name>`.
3. Run `CARGO_PROFILE_DEV_DEBUG=false cargo publish --locked --package <name>` only with explicit release
   authorization.
4. Wait until the exact version resolves from the registry before proceeding.

Stop after any failed or ambiguous upload. A dry-run for a dependent cannot
pass before its unpublished internal dependencies resolve from crates.io; do
not bypass that boundary with `--no-verify`.

The explicit development-profile debug setting avoids a verified-package
linker failure with Wild 0.9.0's debug-section handling in the local environment.
It changes neither the release profile nor host configuration, and does not
skip Cargo's compilation of the extracted package archive.

After all crates resolve, run:

```console
./.config/selfci/check-sdk-packages.sh --registry
cargo install --locked dpc-tau --version '=0.1.1'
```

The public `v0.1.0` tag and GitHub release already identify the original
prepared source commit `79463b83114722bde95423014c58c39c416b70da`. Never move,
force, recreate, or push that tag. The approved recovery published the remaining
0.1.0 archives from later archive/README repair commits; those commits must stay
ancestors of the new release. After all 0.1.1 source, registry, and native asset
gates pass, coordinate a **new** `v0.1.1` tag and release. Never promote manual
candidate artifacts or represent the original v0.1.0 assets as the new release.
