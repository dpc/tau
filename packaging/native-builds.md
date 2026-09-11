# Native core builds and manual Actions artifacts

The driver has begun local native execution, but **neither a complete local
architecture build nor the Actions workflow is qualified**. Local tests verify
orchestration, immutable inputs, metadata stamping and workflow policy, not
compilation or portability. A successful compilation or candidate assembly
would still leave the install, runtime and cross-architecture gates below. No
release publication or activation is part of this tooling.

## Local invocation

Use a non-root user on a matching native x86_64 or ARM64 Linux Docker host.
The host needs Python 3.11+, Git, Docker access, network access to the pinned
image/tool sources and Cargo dependencies, and ample disk/RAM. No emulation
fallback is provided; both client and Docker-server architectures are checked.

```console
python3 packaging/build.py \
  --repo /path/to/source-repository \
  --source-sha FULL_40_CHARACTER_SOURCE_COMMIT_SHA \
  --workflow-sha FULL_40_CHARACTER_TOOLING_COMMIT_SHA \
  --arch amd64 --output /tmp/new-native-candidate \
  --maintainer 'Your Name <your-email@example.org>'
```

Use `arm64` for ARM64. The source commit must be available in the local
repository. The tooling commit identifies the checkout containing this driver,
not necessarily the application source. Every build-control file must match its
Git object at that tooling commit, so an uncommitted tooling edit fails instead
of being mislabeled. The supplied repository is a trusted local Git repository;
its selected source tree/build scripts may be arbitrary executable code.

The driver creates a fresh shallow source checkout from the exact commit with
global/system Git configuration, templates, hooks and replacement objects
disabled. It does not copy caller credentials or Git configuration. Dirty
caller worktree files are not build inputs.

## Pinned build inputs and limits

`build-inputs.json` pins per-architecture PyPA `manylinux_2_28` image digests,
Rust **1.97.0** distribution archives and nFPM **2.46.3** archive SHA256s. The
trusted Dockerfile verifies both downloaded archives before installation and
does not install floating distro packages. The image pins its existing C
toolchain and Python environment. Changes to any pin require reviewed source,
architecture and runtime checks; do not substitute another URL on failure.

The AlmaLinux 8/glibc 2.28 baseline was selected instead of the documented
`manylinux_2_34` alpha images and their x86-64-v2 distro-library caveat. This
does not make Tau a Python wheel or confer manylinux certification. The existing
ELF filter and candidate dependency metadata still conservatively require no
newer than GLIBC 2.34 symbols and advertise package dependencies of glibc >=2.34.
There is no claim of support for old kernels, every RPM distribution, old CPU
variants or every libgcc ABI. No ELF loader/RPATH relocation is performed.

Source compilation uses the selected source's default features and release
profile with `cargo build --locked --release -p dpc-tau`, Rust installed directly
rather than rustup, two Cargo jobs and no shared writable dependency/target
cache. Source Rust requirements above the pin fail; no toolchain auto-upgrade.
`SOURCE_DATE_EPOCH` is the source commit timestamp. Source-only Cargo builds
retain Tau's intentional packaging slots, so a fresh assembly container fills
the existing fixed-size revision/clean/date slots, just as Nix packaging does.
Only build metadata is stamped; this is not a workaround for Nix runtime
linkage. Each slot must occur exactly once and retain its length; missing,
duplicate or incompatible slots fail closed. The version probe compares the
source revision, version and exact UTC minute from the source commit epoch.
These are consistency checks, not authentication of a binary's source.

Each Docker build writes its immutable image ID to a private per-invocation
`--iidfile`; all subsequent containers use that ID, never a shared mutable tag.
Trusted builder images/layers remain in the local Docker cache for reuse;
ordinary Docker cache maintenance is the operator's responsibility. Untrusted
source compilation runs in discarded containers, not cached Docker build layers.

Build containers use the caller's non-root UID on a rootful Docker daemon. On
a rootless daemon they use container UID 0, which the daemon's user namespace
maps to the caller's unprivileged host identity; using the caller's numeric UID
inside that namespace would instead map bind-mount writes to a subordinate UID.
Both modes use a read-only root and source, no capabilities, no-new-privileges,
a 512-process limit, two CPUs and 12 GiB memory. Only scratch storage is
writable; no Docker socket, host home, Actions credentials or runtime
authorization files are mounted. Cargo can fetch locked dependencies over the
network. Package assembly uses a **fresh, network-disabled container** with
build output read-only. A third fresh network-disabled container probes the
packaged `tau --version`; candidate code never executes in the host or assembly
process.

The driver limits build/probe time and captured output, and attempts to
force-remove its named container after failure or timeout. It requires at least 8 GiB free after
preparing the builder, but that check is not a sufficient-capacity guarantee.
The configured public GitHub VM labels have documented 16 GiB RAM and 14 GiB
SSD; the image, compiler and target tree may still exhaust available storage.
No destructive runner cleanup is included. Measure actual peak memory/disk and
adjust approved capacity before calling the workflow operational.

## Manual workflow trust boundary

`.github/workflows/native-candidates.yml` is dispatch-only and restricted to the
verified `dpc/tau` repository's `master` workflow ref. It checks out tooling at
`github.workflow_sha`, validates the independent full lowercase `source_sha`,
then checks out that source separately. Both checkout steps disable persisted
credentials. Application source cannot replace the build driver/Dockerfile.

Actions are pinned to full commits: checkout v6.1.0 and upload-artifact v7.0.1.
Only `contents: read` is granted. There is no OIDC, environment secret, signing
or release token, shared cache action, privileged/self-hosted runner, tag
trigger, promotion switch, or publisher. The configured native hosted runners
are `ubuntu-24.04` and `ubuntu-24.04-arm`, not a mutable `latest` label.
The workflow must first reach the approved mirror/default branch through the
normal owner-controlled process; this change does not push or dispatch it.

Successful jobs upload architecture/source/run-specific artifacts for **14
days**. The eight output files comprise core DEB/RPM/archive, source manifest,
build manifest, toolchain report, Cargo log and SHA256SUMS. The build manifest
separates source and workflow SHAs, records pin-file digest, image ID, source
epoch, run identity, compiler/profile/features and version probe result.
All files are checksummed after assembly. A failure leaves no final output
directory; escaped diagnostic tails are printed instead of interpreting source
output as terminal or Actions commands.

These are unqualified manual test assets, not releases, signatures, independently
authenticated provenance or promises of bit-for-bit reproducibility. Neither a
successful build nor matching `--version` proves actual restricted supervisor
startup. No external package is built by this slice: all seven/eight locked
external roots/binaries are still inventoried rather than silently presented as
qualified. No site/release asset links have been added.

## Verification and remaining gates

```console
python3 packaging/test_native.py
python3 packaging/test_build.py
python3 packaging/test_tools.py
actionlint .github/workflows/native-candidates.yml
```

The first two suites run in SelfCI without Docker. The real nFPM/dpkg/rpm
format suite requires those tools and still uses an inert payload. Actionlint
checks workflow syntax; policy regressions separately check the pinned actions,
permissions, checkout separation and artifact retention.

Remaining gates include actual native builds on both architectures; named
distro dependency-resolving install/ownership/uninstall tests; genuine
default-restricted supervised/no-credential startup, shell/PTY/CA tests on
approved real kernels/security policies; source/license closure and all external
packages; and the separate trusted complete-inventory tagged publisher.
Linux 5.12 remains the documented mount API minimum, not a claim established by
containers sharing a newer host kernel.

## Input provenance references

Pins and platform behavior were checked against official sources on September
9, 2026: PyPA's `pypa/manylinux` README and Quay manifests; Rust's
`static.rust-lang.org/dist/rust-1.97.0-<target>.tar.xz.sha256`; nFPM's v2.46.3
release checksums; the official checkout/upload-artifact Git tags; and GitHub's
Actions contexts, hosted runner and artifact documentation. The committed
digests, not today's mutable image tags, drive builds. Availability of those
inputs and runner labels is not an executed Tau build result.
