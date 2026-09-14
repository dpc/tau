# Complete native builds and GitHub release packages

## Local invocation

Use a non-root user on a matching native x86_64 or ARM64 Linux Docker host,
with Python 3.11+, Git, Docker access, network access to source/tool/dependency
servers, and ample disk/RAM. No emulation fallback exists.

```console
python3 packaging/build.py \
  --repo /path/to/source-repository \
  --source-sha FULL_40_CHARACTER_SOURCE_COMMIT_SHA \
  --workflow-sha FULL_40_CHARACTER_TOOLING_COMMIT_SHA \
  --arch amd64 --output /path/visible/to/docker/new-native-candidate \
  --maintainer 'Your Name <your-email@example.org>'
```

Use `arm64` for ARM64. The output parent and tooling checkout must be visible
at identical paths to the Docker daemon; sandbox-private `/tmp` may not be.
The source commit must exist in the supplied local Git repository.
All build-control files must match their immutable tooling Git objects.
Source and workflow identities remain separate, but both must select the same
reviewed commit when qualifying an exact release candidate.

The driver checks out the core and seven external source SHAs into fresh shallow
repositories without caller Git configuration, credentials, hooks, templates,
replacement objects, or interactive prompts. External URLs/SHAs/NAR hashes come
from the core commit's flake lock. The selected inventory must byte-match trusted
tooling; arbitrary source cannot quietly redefine the required product set.

## Pinned inputs and sequential builds

`build-inputs.json` pins architecture-specific PyPA `manylinux_2_28` images,
Rust **1.97.0** archives, nFPM **2.46.3** archives, and qualification image indexes.
The Dockerfile also verifies the cargo-about **0.9.0** registry archive against
its registry checksum and compiles it natively using its own locked dependency
checksums. It does not copy a Nix-linked generator into a manylinux image.

The AlmaLinux 8/glibc 2.28 build baseline avoids the previously evaluated
`manylinux_2_34` alpha/x86-64-v2 caveats. This is not manylinux certification.
The conservative package dependency floor remains glibc ≥2.34.
No loader or RPATH relocation is performed.

One job per native architecture builds core, then each external project,
sequentially using `cargo fetch --locked` followed by
`cargo build --locked --release -p <package> --bins`,
default features, and the selected source profile. Telegram's two binaries come
from one source build. Each project has private writable Cargo/target storage.
After offline notices succeed, its fresh finish container retains binaries and
notices and removes only that project's compiler/Cargo scratch. This bounds
sequential-job disk growth; it does not clean unrelated runner directories.
Rust requirements above the pin fail rather than auto-upgrading.

Cargo-about runs against each delivered package's manifest and exact lockfile
with `--frozen --fail` and the same architecture target. The earlier fetch covers
all locked platforms because Cargo metadata needs their source manifests even
when notice output is target-filtered. `about.toml` declares
the accepted license policy; `notices.hbs` emits full texts and crate identities.
Build dependencies remain included because generated/bundled code may survive
linking; dev-only dependencies are excluded. The resulting notice is installed
and its digest recorded for every binary package (shared by the two Telegram
binaries from the same source).

A fresh offline assembly container stamps only Tau's existing fixed-size
revision/clean/date placeholders. Each slot must exist exactly once and retain
its length. All nine ELF payloads are audited and packaged with project licenses,
notices, and source manifests. The release-set package revision preserves the
external Cargo version; [README.md](README.md) specifies the payload/asset layout.

## Isolation and executable qualification

Build containers use the caller UID with rootful Docker, or namespaced root
with rootless Docker (which maps to the unprivileged host caller). They have
read-only root/source/tooling, no capabilities, no-new-privileges, 512 processes,
two CPUs, 12 GiB RAM, and bounded scratch/log/time use. No Docker socket, host
home, authorization file, or Actions token is mounted.

Cargo fetching is network-enabled. Notice generation, assembly, archive probes,
and installed-package checks are network-disabled. Archive probes execute only
extracted payloads, in another fresh container, never in assembly or on the host.
They compare complete/individual binaries and prove `--help`, Tau build identity,
and real protocol-7 Hello admission with deliberately invalid configs.
Hello admission is not Ready or integration functionality.

Distro setup images use digest-pinned Debian 12 and Fedora 43 bases and resolve
their ordinary runtime/Python dependencies from distro repositories. Those setup
resolutions are not reproducible build inputs: the derived image IDs and exact
installed package versions are retained as qualification evidence. Actual
package installation/removal runs offline in disposable containers with writable
container rootfs and only CHOWN/FOWNER/DAC_OVERRIDE/SETUID/SETGID added to the otherwise empty
capability set. All host mounts are read-only. The checks do not configure or
start services, and they must preserve a user-state sentinel. The Debian slim
test image explicitly includes Tau documentation otherwise excluded by its
space-saving dpkg policy, so notices/manifests must actually install.

Every image uses a private `--iidfile`, never a mutable shared tag. Containers
are force-removed after errors/timeouts; trusted Docker build layers remain
available for ordinary operator cache maintenance. The 8 GiB free-space precheck
is not a capacity guarantee for nine binaries. Measure hosted peak usage;
there is no destructive runner cleanup or silent omission on exhaustion.

## Manual workflow versus tagged publication

`native-candidates.yml` is dispatch-only for `dpc/tau` on `master`. It checks out
tooling at `github.workflow_sha` and selected application source at the independent
40-character input SHA, with persisted credentials disabled in both checkouts.
It grants only `contents: read`, has no release/signing/OIDC lane or secret, and
uploads architecture/source/run-specific test artifacts for 14 days.
The configured native runners are `ubuntu-24.04` and `ubuntu-24.04-arm`.

Successful candidates contain all 30 package/archive files, source/build/toolchain
manifests, build/notice/probe/setup/qualification logs, and `SHA256SUMS`.
The build manifest records source/workflow identities, pins, image IDs, per-asset
hashes, upstream inventory, archive probes, and distro qualification evidence.
These labels/checksums are not independently authenticated provenance.

`release.yml` runs only for `v*` tags in `dpc/tau`. The tag must exactly match the
application version (`crates/tau/Cargo.toml`, resolving workspace inheritance only
when explicit). Both builds use that tag's exact source as tooling. Only the
final publisher has `contents: write`; manual artifacts cannot feed it.

After both native builds pass, `release_assets.py` verifies the complete
66-file, two-architecture inventory (30 packages plus three metadata files each),
source/workflow/tag identities, source lock inventory, per-asset hashes, and
qualification evidence. No shell glob decides what is required. The publisher
rechecks the remote lightweight/annotated tag SHA and writes aggregate checksums.
`publish.py` creates or resumes only a Tau-marked draft bound to the exact
tag/source/workflow identities. It checks every existing remote asset's name,
size, uploaded state, and GitHub SHA256 digest, then uploads only missing assets.
Unexpected, partial-starter, or differing assets fail closed without overwrite
or deletion. Foreign drafts and published releases are never modified.
A fully matching already-published release is a read-only verified no-op.

After all remote assets and checksums match, the publisher rechecks the tag
and marks the draft non-draft. An interrupted upload or finalization can be
retried; the same identity/content checks run again. This explicit resume
contract does not rely on undocumented CLI cleanup behavior (current `gh release
create` itself also stages asset uploads as a draft). SemVer prereleases are
marked prerelease; build metadata is not supported for release-set versions.

Source readiness is not hosted qualification. Review and mandatory local SelfCI
must pass before the owner lands source without a tag and dispatches the manual
candidate workflow. Verify both recorded SHAs match that exact candidate; a
newer-master race must not silently qualify different tooling. Both hosted native
results remain release blockers before creating the 0.1.1 tag or publishing
registry packages. No site links to uncreated assets are added here.

## Verification limits

Run the four Python unit suites listed in README, optional real `test_tools.py`,
actionlint for both workflows, and mandatory project SelfCI. Unit fixtures are
not execution evidence. Successful container checks do not establish default
restricted supervision, no-credential provider round trips, shell/PTY/CA behavior,
the Linux 5.12 mount-API floor, or every distro/CPU/libgcc ABI.

Historical Actions run 34556681071 built core only on both architectures on
September 11, 2026. It is not evidence for this new complete distribution.
Original build pins were checked against PyPA/Quay, Rust distribution checksums,
nFPM release checksums, and official Actions repositories on September 9, 2026.
The cargo-about registry checksum and Debian/Fedora image indexes were resolved
on September 14, 2026. Committed digests, not mutable tags, control selection.
