# Crates.io application release

Publishing `dpc-tau` requires publishing its internal Rust crate closure first.
Package preparation does not authorize uploads, tags, or GitHub releases.

## Prepared closure

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
dpc-tau-skills                   0.1.0
dpc-tau-socket                   0.1.0
dpc-tau-term-screen              0.1.0
dpc-tau-ext-rhai                 0.1.0
dpc-tau-ext-std-notifications   0.1.0
dpc-tau-ext-test-dummy           0.1.0
dpc-tau-ext-utils                0.1.0
dpc-tau-ext-websearch            0.1.0
dpc-tau-provider                 0.1.0
dpc-tau-ext-shell                0.1.0
dpc-tau-cli-picker               0.1.0
dpc-tau-cli-term-raw             0.1.0
dpc-tau-provider-chat-completions 0.1.0
dpc-tau-provider-codex           0.1.0
dpc-tau-provider-responses       0.1.0
dpc-tau-session-inspect          0.1.0
dpc-tau-cli-term                 0.1.0
dpc-tau-ext-provider-builtin     0.1.0
dpc-tau-harness                  0.1.0
dpc-tau-harness-tools            0.1.0
dpc-tau-test-support             0.1.0 (package-verification dependency)
dpc-tau-cli                      0.1.0
dpc-tau                          0.1.0
```

As of September 14, 2026, the first 28 versions through
`dpc-tau-ext-provider-builtin` are published. Their registry archives match the
prepared release source. The following five versions remain absent:
`dpc-tau-harness`, `dpc-tau-harness-tools`, `dpc-tau-test-support`,
`dpc-tau-cli`, and `dpc-tau`.

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
2. Run `cargo publish --locked --dry-run --package <name>`.
3. Run `cargo publish --locked --package <name>` only with explicit release
   authorization.
4. Wait until the exact version resolves from the registry before proceeding.

Stop after any failed or ambiguous upload. A dry-run for a dependent cannot
pass before its unpublished internal dependencies resolve from crates.io; do
not bypass that boundary with `--no-verify`.

After all crates resolve, run:

```console
./.config/selfci/check-sdk-packages.sh --registry
cargo install --locked dpc-tau --version '=0.1.0'
```

The public `v0.1.0` tag and GitHub release already identify the original
prepared source commit `79463b83114722bde95423014c58c39c416b70da`. Never move,
force, recreate, or push that tag as part of archive repair. Publishing the five
remaining `0.1.0` archives from a newer repair commit creates an intentional
source split and therefore requires explicit release approval before upload.
After an approved recovery publishes all five, rerun the registry checks above
and verify the existing release assets; do not run the tag step again.
