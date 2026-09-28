---
name: tau-release
description: Use when preparing or carrying out an explicitly authorized Tau application release; not for SDK-only releases.
---

# Tau application release

Use this procedure for a **new application release**, not for an SDK-only
release. Establish the exact release version, source commit, owner, and
authorization before making irreversible changes. Keep a durable ledger of
each published package, source ref, tag, workflow run, asset, and verification
result. Never infer today's registry or hosted-release state from an old ticket
or from a prepared manifest. For example, the initial core-only audit in
ticket `9jho` was superseded by the final `v0.1.1` native release: inspect its
*final* outcomes, and independently recheck registry state (the historical
`0.1.1` native release did not itself prove all `0.1.1` Cargo uploads).

Read [crates.io release](../../../docs/crates-io-release.md),
[SDK releases](../../../docs/sdk-releases.md),
[release builds](../../../docs/release-builds.md), and
[native distribution](../../../packaging/README.md) with
[native build details](../../../packaging/native-builds.md). Inspect
`packaging/distribution.toml`, `flake.lock`, `.github/workflows/{native-candidates,release}.yml`,
`packaging/{build,release_assets,publish}.py`, and the current public install
instructions before forming a release plan. These documents include historical
measurements and release records, not guarantees that today's source, registry,
or external pins match them.

## 1. Scope and preflight

1. Confirm the user approved the specific release and irreversible destinations:
   crates.io uploads, source publication to Radicle and GitHub, a new immutable
   `v<application-version>` tag, GitHub release/assets, and updated public links.
   Designate **one** publisher for each irreversible step. Approval to publish
   does not authorize Nix activation, replacing a running binary, restarting
   services, exposing private projects, moving an older tag, or adding speculative
   packaging targets.
2. Confirm prerequisites and holds: required feature changes are complete;
   checkout graph is clean, linear, and based on the intended master; source
   remotes and registry are reachable; credentials work *through the supported
   path*. If broker or credential access fails, stop rather than bypassing it.
   Check draft-read and release-body-write permissions separately: public release
   reads and workflow reruns do not prove either permission. In v0.2.0, the
   publisher's Actions credential could publish while the local broker could
   neither inspect the draft nor update the public notes. Arrange an authorized
   owner handoff if those operations are unavailable.
   Identify independently maintained extension repositories and exact locked
   revisions. Reconcile SDK/protocol admission and extension builds with current
   harness requirements; coordinate necessary updates in their owning projects
   and lock their published inputs **before** freezing the Tau candidate. A
   same-major minor protocol warning alone is not proof of incompatibility or
   proof of which binary is deployed.
   Inspect every pinned lock independently: during 0.2.0 preparation four pins
   still selected protocol 8.1 despite newer compatible sources already being
   published. Ask owning projects for verified existing revisions before
   commissioning another migration. Also update and regression-test the
   release asset verifier's explicit SDK lock expectations when the release
   closure changes; prepared manifests alone do not update that gate.
3. Inventory the baseline: full source SHA, application version from
   `crates/tau/Cargo.toml`, `tau --version` (CLI embeds this string), SDK/protocol
   versions, flake-lock extension sources, expected native inventory, and release
   notes. This is **not** the release candidate: version/dependency edits and
   extension-lock preparation below must happen before the final freeze.
   Determine which versions are already published by checking actual
   remote refs, the official crates.io sparse index, and release assets. Preserve
   HTTP/auth errors distinctly from explicit absence; the old REST 403→404
   mistake caused a false publication conclusion. Existing versions are
   immutable: inspect their archive checksum and source provenance before
   treating them as satisfied, never overwrite them.

## 2. Prepare and qualify the exact candidate

1. Advance the application and CLI together to the approved version; update
   changed internal crates and their exact path-plus-version dependencies in
   the full runtime **and package-verification/dev-dependency** closure. Do not
   bump every workspace crate by default or reset independently versioned
   `dpc-tau-proto`/`dpc-tau-client` to the application number. Determine SDK
   compatibility and protocol revision separately using
   [SDK releases](../../../docs/sdk-releases.md) and the protocol spec.
   The current closure includes `dpc-tau-provider-grok`; recompute the DAG
   rather than copying the `0.1.1` upload list. Published SDK leaves may remain
   unchanged. A source-breaking SDK change may require publishing proto and
   client first, followed by rebuilding compatible external extensions.
2. After version/dependency edits and external pin updates, freeze the final
   candidate: record its full Git SHA, application/CLI and SDK/protocol versions,
   locked extension SHAs, and expected package/asset inventory. Re-freeze and
   requalify if any source changes. Check metadata, license/README, embedded
   resources, and exact files in archives with
   `./.config/selfci/check-crates-io-packages.py` and
   `./.config/selfci/check-sdk-packages.sh`. Keep release-owned snapshots
   byte-aligned with canonical resources. `cargo package --list` and local
   package checks establish archive completeness, **not registry presence**.
   Build and test using the project's normal gates (`cargo check --workspace
   --all-targets`, affected `cargo nextest run` / full workspace suite when
   appropriate, `treefmt`); independently review the fixed candidate and run
   mandatory `selfci check --candidate <change-id>`. Record the reviewed/tested
   tree and rerun affected checks after substantive changes.
3. Verify archives against the frozen candidate. In dependency order, perform
   `CARGO_PROFILE_DEV_DEBUG=false cargo publish --locked --dry-run --package
   <crate>` where dependencies already resolve from crates.io; defer dependent
   dry-runs until their prerequisites become visible. Do not use `--no-verify`
   to hide a missing archive input or an unavailable registry dependency.
   Where useful, qualify exact-source amd64/arm64 native builds through the
   **manual** `native-candidates.yml` workflow before the tag: check for an
   existing in-flight or successful exact-source run first. Its artifacts are
   test candidates, never promoted to release assets. The `v0.1.1` path used
   a direct tagged build after source gates instead of requiring a redundant
   extra candidate run; document which route was actually qualified.
4. Draft human release notes and review installation claims. Distinguish
   tested functionality from unverified platform/runtime claims; static ELF
   and archive checks do not establish all distro/CPU/kernel behavior.

## 3. Publish dependencies, then source and release

1. For **each missing exact crate version** in the recomputed topological
   closure, confirm explicit registry absence, run the locked verified dry-run,
   then `CARGO_PROFILE_DEV_DEBUG=false cargo publish --locked --package <crate>`
   under the approved single publisher. Wait for the official index to show
   that exact version; check its archive checksum and provenance before starting
   dependents. Record success immediately. On timeout, ambiguous response,
   mismatched pre-existing version, or upload failure, **stop**, re-read remote
   state, and reconcile before any retry. Cargo's dev-profile debug setting
   addresses the documented local verified-package linker issue; it neither
   changes the release profile nor skips verification.
2. After registry visibility, run
   `./.config/selfci/check-sdk-packages.sh --registry` against the intended
   SDK source and a fresh registry-only consumer/isolated
   `cargo install --locked dpc-tau --version '=<version>'` (use an isolated
   install root; do not replace the active Tau). Check the installed CLI version
   and source/archive identities. A prior SDK consumer check for another source
   revision does not qualify this candidate.
3. Publish the reviewed exact candidate via the project's normal source/MQ
   path to **both Radicle and the GitHub mirror** and verify both remote source
   refs resolve to its full SHA. Reconcile tested tree against final commit;
   if publication changes contents, requalify. Only after registry and source
   gates pass, create/push the **new** `v<application-version>` tag at that exact
   commit; confirm remote tag target. Never move/recreate an earlier public tag
   (including `v0.1.0`/`v0.1.1`), or use an SDK version as the app tag.
4. The tag triggers `.github/workflows/release.yml` in `dpc/tau`. Watch both
   native builds (`ubuntu-24.04` amd64 and `ubuntu-24.04-arm` arm64), then its
   publisher. Check the version/tag/source SHA, pinned external input inventory,
   build/notice/source/toolchain manifests, licenses, `SHA256SUMS`, asset digests,
   and complete remote inventory. Current `packaging/distribution.toml` delivers Tau plus
   seven external projects (nine binaries including `tau-telegram-gateway`),
   individual and `tau-full` DEB/RPM/tar.gz on **both** architectures: 30
   packages/archives + three metadata files per architecture, plus aggregate
   `SHA256SUMS` (67 assets total when the inventory remains unchanged).
   `tau-full` DEB/RPM is an exact-dependency metapackage; the tarball combines
   payloads. Validate against the inventory, not a remembered asset count.
   `release_assets.py` and `publish.py` check identities and refuse conflicting
   assets; manual candidate artifacts cannot feed this workflow. Confirm the
   published GitHub release is nondraft, tagged at the expected SHA, and contains
   accurate human notes, not just an autogenerated changelog.
5. **Only after real assets and registry packages exist**, update release/download
   references in `README.md`, `docs/getting-started.md`, `site/index.html`, and
   other affected public docs as appropriate. Use verified asset names/URLs and
   installation syntax; keep supported architecture and runtime-qualification
   limits explicit. Review and validate the documentation change separately.
   Hand off any Nix flake/config version or pin update for review; do not imply
   that a new package was activated or a service restarted.

## Recovery and completion

Keep an exact-version/SHA progress ledger and stop dependent stages whenever
evidence is missing. Registry publishes cannot be undone; if a later step fails,
resume from confirmed exact published versions rather than replaying uploads or
revising the candidate beneath them. If a tag-triggered workflow partially
uploads assets, inspect its run and draft identity: the publisher may resume
only matching missing assets and refuses conflicts, foreign drafts, or edits to
published releases. A failed-only publisher retry may reuse successful build
artifacts **after** checking run/attempt and identities. Do not replace old
assets or disguise a partial release as complete. Report the exact completed
and blocked steps, source/version/SHA, registry and consumer results, CI/review
evidence, GitHub workflow/release/asset evidence, documentation state, and
remaining manual Nix/activation handoff. Update this skill when the real
procedure changes; do not convert a historical ledger into current facts.

For a `created draft is not visible` failure, creation may already have succeeded.
Have an authorized owner inspect the draft's tag, source/workflow identity marker,
draft/prerelease flags and assets before any retry. The v0.2.0 recovery confirmed
an exact matching empty draft, then used `gh run rerun RUN -R dpc/tau --failed`;
it did not recreate the tag, rebuild successful architectures or overwrite assets.
Check that the original artifacts are unexpired and belong to the exact source;
this workflow retains them for only one day. Workflow artifacts are not published
release assets. Preserve the `tau-native-release-v1` identity marker when applying
reviewed human notes, and read back both body and unchanged asset identities.
If the local broker denies the write, hand the exact reviewed body to the owner;
do not bypass credentials or retry blindly.
