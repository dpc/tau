# Native Linux packaging: local candidate harness

This is **manual candidate tooling, not a release pipeline or qualified
distribution**. It inventories immutable Tau/external sources, rejects ELF
runtime linkage outside a conservative GNU baseline, and creates test core
DEB/RPM/archive assets with nFPM. Existing Nix packages remain unchanged. Do not
repackage Nix executables by patching their loader.

The [native build driver and manual Actions workflow](native-builds.md) add
digest-pinned baseline images and checksum-pinned Rust/nFPM downloads. They have
local orchestration tests but have **not been executed on native build runners**.
The low-level supplied-binary commands below remain useful independently.

## Run locally

Requires Python 3.11+, Git, GNU readelf, and nFPM **2.46.3** on PATH. nFPM is
version-checked, not downloaded by `native.py`; this low-level script alone is
not a reproducible toolchain. The nFPM JSON configuration uses its YAML-compatible
config schema.

```console
python3 packaging/test_native.py
python3 packaging/native.py inventory --source-sha FULL_40_CHARACTER_COMMIT_SHA
python3 packaging/native.py audit-elf --binary /path/to/native/tau --arch amd64
python3 packaging/native.py package-core \
  --source-sha FULL_40_CHARACTER_COMMIT_SHA \
  --binary /path/to/native/tau --arch amd64 \
  --maintainer 'Your Name <your-email@example.org>' --output /tmp/new-tau-candidate
```

With nFPM 2.46.3, `dpkg`/`dpkg-deb` and `rpm` on PATH, also run
`python3 packaging/test_tools.py`. This creates real package formats around an
**inert test payload**, checks both package managers' version ordering, inspects
file inventory and absence of activation scripts, and extracts the DEB. The
test replaces ELF inspection only for that inert fixture; it does not run Tau
or install packages and is not runtime qualification. The dependency-free unit
suite runs in SelfCI's lint job; the real-tool suite is a separate explicit gate
alongside the native builder's eventual execution checks.

Use `arm64` for AArch64. `inventory` reads Git objects with replacement objects
disabled, not dirty worktree files or locally substituted history; the commit
must be available locally. It reads external revisions/URLs/NAR hashes
from the selected commit's root flake inputs, never a floating branch. The eight
external binaries remain pending their owning projects' audits; no external
source is fetched or executed here. On September 9, 2026, the local GitHub origin
was verified as `dpc/tau` (default branch `master`), a build/download mirror
candidate; Radicle remains canonical. Older Cargo metadata naming
`dpc/tau-agent` must not be used to infer the publication destination.

`audit-elf` never runs the binary or `ldd`. It requires ELF64 little-endian, the
architecture's standard GNU loader, no Nix-store interpreter/dynamic references,
no RPATH/RUNPATH, only libc/libm/libgcc plus the baseline glibc pthread/dl
compatibility libraries and loader dependencies, and required GLIBC symbols no
newer than 2.34. It rejects unresolved Tau build placeholders. Additional
dynamic libraries require an explicit dependency-policy review.
Passing this filter **does not establish source identity, kernel compatibility,
libgcc symbol compatibility, TLS/CA behavior, or successful startup**.

`package-core` copies the binary into private staging, audits that copy, and
creates all output in a new directory only after both nFPM invocations succeed.
An existing output path is refused. Packages contain only `/usr/bin/tau`,
the source license and source/ELF manifest: no maintainer scripts, service units,
accounts, user configuration, secrets, or automatic activation. The archive
contains the same three files. Uninstall must preserve user state; actual package
manager install/uninstall testing is still required before claiming that behavior
on a distribution.

Every package is an explicitly unqualified test version
`0.0.0~test.<source_sha>-1`, ordered before stable `0.0.0` by modern DEB/RPM
version comparison. This is not the future stable release version policy.
Outputs include `source-manifest.json` and `SHA256SUMS`. The manifest explicitly
records that the caller-supplied binary's source relationship is not attested.
Checksums provide integrity, not authentication. Archive timestamps/ownership
are normalized; byte-for-byte reproducibility is not claimed.

## Qualification and remaining implementation

Before release authority or download links are added:

* Execute the native x86_64 and ARM64 builder with its pinned baseline images,
  Rust and nFPM, bounded resources and `cargo build --locked --release -p dpc-tau`.
  Record compiler, image digest, source/workflow SHAs and build identity;
  validate the actual `tau --version` against source.
* Run real package-manager install/metadata/ownership/uninstall tests with
  dependency resolution on named Debian/Ubuntu and RPM-family userspaces.
  Candidate dependency names are not a claim of support for all RPM systems.
* Exercise default **restricted supervised startup**, a no-credential provider
  round trip, shell/PTY and CA loading on real supported VM kernels/security
  policies. `--help`, a privileged container, or a successful ELF audit is not
  sufficient. Linux 5.12 is the documented minimum for recursive read-only
  mount support; vendor backports remain unverified. Containers share the host
  kernel and cannot prove the floor.
* Audit/build/license/package all seven locked external projects (eight
  binaries, including separate Telegram gateway), preserving their own versions
  and recording SDK/protocol/tested Tau compatibility. Do not omit an
  unqualified package silently.
* Validate the least-privilege exact-SHA manual Actions workflow on approved
  runners, then add separate trusted tag builds and complete-inventory draft publication. Never
  execute candidate scripts/binaries in the publisher or promote arbitrary
  manual artifacts to releases.
* Publish source archives, complete provenance and the approved full inventory.
  Only then add verified asset links to the site/docs. No links are added here.

Local investigation found that the available Nix-built `result/bin/tau` has a
Nix-store loader and RUNPATH and therefore fails the baseline filter. Local
rootless-Docker native execution has begun, but no complete architecture build
or runtime qualification has passed. No alternate host has been qualified or
activated by this work.

See `docs/release-builds.md` for the existing build profile and Nix distribution;
see `docs/extensions.md` for explicit extension configuration and restrictions.
