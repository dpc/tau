# DESIGN-release-build-profile: Parallel source-release optimization

Status: confirmed, 2026-07-15, user

Tau's canonical Cargo release profile uses ThinLTO and 16 codegen units. This
policy applies equally to plain Cargo builds, development-shell builds, Nix
packages, and release archives; Nix environment overrides must not create a
different release intent. Nix evaluation guards the profile, and release build
logs retain the final Cargo command, wall time, and maximum RSS so growth of the
universal binary cannot silently restore a multi-minute serial final build.

The universal executable links every bundled component. Fat LTO with one
codegen unit reduced the reference binary but made the final Nix build
multi-minute and mostly serial. ThinLTO/16 parallelizes that work. Adoption
requires representative runtime to regress by no more than 3%. A separate
fat-LTO profile is not maintained because current evidence does not show enough
user-visible runtime benefit to justify its build and maintenance cost.

The installed x86_64 binary must remain at or below the 125 MiB adoption limit.
The size and runtime adoption limits are not permanent product requirements.
Re-evaluate the profile and builder capacity when the
compiler, linker, target, component graph, or release pipeline changes; when
an adoption limit is missed; when startup latency becomes important; or when
native/cross builders approach their memory limits. Measurement details
and the reproduction protocol live in
[`docs/release-builds.md`](../docs/release-builds.md).
