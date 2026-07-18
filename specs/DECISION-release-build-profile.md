# DECISION-release-build-profile: Parallel source-release optimization

Authority: confirmed, 2026-07-15, user

Tau's canonical Cargo release profile uses ThinLTO and 16 codegen units rather
than FatLTO with one codegen unit. The universal executable links every bundled
component; the selected profile keeps representative runtime and size acceptable
while making the final optimization substantially more parallel and reducing
source-release build time from several mostly serial minutes.

Plain Cargo builds, development-shell builds, Nix packages, and release archives
share this one release intent. A separate FatLTO profile is not maintained because
current evidence does not establish enough user-visible runtime benefit to justify
its build and maintenance cost.

Current profile settings, evaluation guards, measurements, acceptance evidence,
and reproduction instructions are documented in
[`docs/release-builds.md`](../docs/release-builds.md).
