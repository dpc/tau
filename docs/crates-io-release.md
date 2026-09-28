# Crates.io application release

Publishing `dpc-tau` requires publishing its internal Rust crate closure first.
Package preparation does not authorize uploads, tags, or GitHub releases.

## Tau 0.2.0 release record

The application and CLI advance together to **0.2.0**. The current runtime and
package-verification closure has **34 crates**: 29 application/internal crates
at 0.2.0, proto/client at independently versioned **0.9.0**, and three unchanged
published leaves at 0.1.0 (`actions`, `blocking-notify-channel`, `util-fs-err`).
Even otherwise unchanged crates that depend on the new SDK or changed internal
packages need new versions because their exact registry dependency pins change.
The workspace default stays 0.1.0; unrelated evaluation/supervisor packages
are not part of this release closure.

Protocol **10.1** adds source-incompatible public SDK fields and enum variants
since the published 0.8.0 SDK. Cargo SDK 0.9.0 is independent of both the
application version and wire admission. All seven locked external projects use
SDK **0.8.0** / protocol **10.0**, admitted with a same-major warning and
best-effort operation. Rostra, Slack, XMPP and Zulip pins advance to their already
published compatible revisions; PIM, Swarm and Telegram pins remain unchanged.
Upstream extension Cargo versions remain 0.1.0. The release asset verifier
requires core SDK 0.9.0 and external SDK 0.8.0 in the recorded lock metadata.

Fresh official sparse-index and archive checks on September 27, 2026 found the
three reusable leaves published, with matching archive checksums and original
source provenance. None of the 31 candidate uploads existed at preflight; the
Grok crate's index returned an explicit HTTP 404. All 31 were subsequently
published from `4761e99c0375d0b8a4eb8046922b8f3790675398` on September 27.
Each locked verified dry-run passed; every registry archive matched its local
checksum and clean source provenance. The registry-only SDK consumer passed
on Rust 1.91, and a fresh isolated locked `dpc-tau =0.2.0` install passed.

The immutable `v0.2.0` tag identifies that same source. The native
[GitHub release](https://github.com/dpc/tau/releases/tag/v0.2.0) became public
on September 28, 2026, with all 67 expected assets verified. Its two native builds
passed; the publisher initially stopped after draft creation was not immediately
visible. After owner inspection confirmed the matching empty draft, a failed-only
retry reused the unexpired exact-source artifacts and published successfully.

Use `.agents/skills/tau-release/SKILL.md` for the full release procedure,
including the external native inventory, immutable source/tag gates, consumer
verification and post-upload public download updates.

## Historical Tau 0.1.1 release record

The application, CLI, and changed internal dependency closure were prepared as
**0.1.1**, and the tagged native distribution was published.
The CLI owns the embedded `tau --version` string, so both application and CLI
advanced together. Unchanged internal packages remained at workspace version
**0.1.0**; that tagged application release used SDK **0.5.0** and protocol 7.2.
Release tooling reads `crates/tau/Cargo.toml`, not the workspace default, for
the application release version.

The complete 0.1.0 application closure was published and the isolated registry
install passed. The 0.1.1 release required seven new uploads, in order:
`dpc-tau-skills`, `dpc-tau-ext-shell`, `dpc-tau-harness`,
`dpc-tau-harness-tools`, `dpc-tau-test-support`, `dpc-tau-cli`, and `dpc-tau`,
all at 0.1.1. Correcting the embedded extension documentation changes skills
and the harness's release-resource snapshot. Shell embeds skills, and its
dependents must use the corrected version. Test support depends on the harness
and is needed to verify the CLI package's dev-dependencies. This is the exact
reverse dependency closure, not a workspace-wide version bump. Do not republish
or replace any existing version.

The seven 0.1.1 registry versions were still absent in the September 27, 2026
sparse-index check. Native release assets did not prove those uploads happened.
The 0.2.0 release did not depend on completing that historical publication.

At the time of the 0.1.1 application release, the external pins selected SDK
0.4.0 / protocol 7.0 sources; they did not change the extensions' upstream
0.1.0 Cargo versions or the released harness protocol 7.2. The later protocol
8.1 rollout updates all seven external inputs to independently published SDK
0.6.0 / protocol 8.1 revisions without changing that historical release.

## Complete dependency closure

The application has 33 runtime crates. Cargo also resolves dev-dependencies
while verifying a package archive. `dpc-tau-cli` uses
`dpc-tau-test-support` for tests, so a fully verified dependencies-first
publication has 34 crates.

`dpc-tau-e2e-tests`, `dpc-tau-summary-eval`, and `dpc-tau-supervisor` are not
needed. They remain ordinary workspace packages rather than being marked
`publish = false`; that policy can be decided separately without obstructing
the application release.

`dpc-tau-provider-grok` belongs to the application dependency closure after the
shared Responses backend and before the built-in provider.

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

The current dependencies-first order is also checked by
`check-crates-io-packages.py`:

```text
dpc-tau-actions                  0.1.0
dpc-tau-blocking-notify-channel  0.1.0
dpc-tau-themes                   0.2.0
dpc-tau-util-fs-err              0.1.0
dpc-tau-vcr                      0.2.0
dpc-tau-proto                    0.9.0
dpc-tau-client                   0.9.0
dpc-tau-config                   0.2.0
dpc-tau-core                     0.2.0
dpc-tau-delivery-memory          0.2.0
dpc-tau-skills                   0.2.0
dpc-tau-socket                   0.2.0
dpc-tau-term-screen              0.2.0
dpc-tau-ext-rhai                 0.2.0
dpc-tau-ext-std-notifications   0.2.0
dpc-tau-ext-test-dummy           0.2.0
dpc-tau-ext-utils                0.2.0
dpc-tau-ext-websearch            0.2.0
dpc-tau-provider                 0.2.0
dpc-tau-ext-shell                0.2.0
dpc-tau-cli-picker               0.2.0
dpc-tau-cli-term-raw             0.2.0
dpc-tau-provider-chat-completions 0.2.0
dpc-tau-provider-codex           0.2.0
dpc-tau-provider-responses       0.2.0
dpc-tau-provider-grok            0.2.0
dpc-tau-session-inspect          0.2.0
dpc-tau-cli-term                 0.2.0
dpc-tau-ext-provider-builtin     0.2.0
dpc-tau-harness                  0.2.0
dpc-tau-harness-tools            0.2.0
dpc-tau-test-support             0.2.0 (package-verification dependency)
dpc-tau-cli                      0.2.0
dpc-tau                          0.2.0
```

The published `dpc-tau-proto` and `dpc-tau-client` `0.4.0` archives could not be
reused for the Tau 0.1.1 source. Its workspace protocol was 7.2. Protocol 7.1 added
public inter-session notice types and fields, and protocol 7.2 added public
per-agent effort control fields; all are absent from registry proto `0.4.0`.
Those source-breaking additions require proto `0.5.0`; client `0.5.0` pins and
published that new protocol line even though its own Rust source was otherwise
unchanged.

The separately maintained extensions used exact registry SDK `=0.4.0` pins and
advertised protocol 7.0 for the 0.1.1 release. Their current independently
published revisions deliberately use exact SDK `=0.8.0` dependencies and
advertise protocol 10.0. Updating those repositories remains outside the
application release procedure; this checkout consumes their published
revisions through its flake lock.

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
cargo install --locked dpc-tau --version '=0.2.0' --root /path/to/isolated/install
```

The public `v0.1.0` tag and GitHub release already identify the original
prepared source commit `79463b83114722bde95423014c58c39c416b70da`. Never move,
force, recreate, or push that tag. The approved recovery published the remaining
0.1.0 archives from later archive/README repair commits; those commits must stay
ancestors of the new release. Never move the existing `v0.1.1` tag either.
The `v0.2.0` tag now also exists and must not move. Future releases need their own
new version/tag after source and registry gates pass. Never promote manual
candidate artifacts or represent older assets as a new release.
