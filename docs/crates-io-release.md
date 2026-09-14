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
the dependency order below, and `cargo package --list` for every archive. It
does not claim that an unpublished dependency exists in the registry.

## Publication order

The current dependencies-first order is:

```text
dpc-tau-actions                  0.1.0 (already published)
dpc-tau-blocking-notify-channel  0.1.0 (already published)
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

The first two versions are already published and their packaged source still
matches the current leaf-crate source. The other 31 versions require upload.

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

Only after those registry checks succeed should the exact source commit receive
and push tag `v0.1.0`. The tag-triggered workflow then creates the GitHub
release and native package assets. Update the site and public installation
links only after verifying those final artifacts.
