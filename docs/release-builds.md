# Release build profile

Tau's normal source release uses optimization level 3, ThinLTO, 16 codegen
units, aborting panics, and stripped symbols. The universal `tau` executable
links every bundled component, so fat LTO with one codegen unit turns its final
LLVM optimization into a long, mostly serial build. ThinLTO retains
cross-crate optimization while allowing LLVM to use the release builder's
cores.

The profile is canonical in `Cargo.toml`. This keeps plain
`cargo build --release`, the development shell, the Nixpkgs-style public
package, Nix/Crane legacy packages, and release archives on the same policy
without ambient environment overrides. Nix evaluation asserts the
ThinLTO/16-CGU settings for the Crane graph. Its final package derivation also
records Cargo's final elapsed-time status and GNU `time -v`'s command, wall
time, and maximum-resident-set statistic in the build log. Update the assertion
and repeat the measurements below when intentionally changing the profile; do
not remove the guard while the universal component graph remains in one
executable. The recorded adoption limits and re-evaluation triggers below are
evidence for the current profile rather than permanent product requirements.

Ordinary workspace builds require stable Rust 1.97 or newer. The Flakebox
development shell selects its pinned stable channel and does not require a
nightly compiler or alternate code-generation backend.

`packages.tau` and `packages.default` use the pinned Nixpkgs
`rustPlatform.buildRustPackage` toolchain. The specialized Crane graph remains
under `legacyPackages` and supplies CI and cross-built release archives. The
development shell and that specialized graph retain the Flakebox toolchain.

## Reference measurement

The Crane-graph reference is revision `e2b061229d74` on a 32-thread Ryzen 9
7950X3D, using the pinned Flakebox Rust 1.96.0/LLVM 22 toolchain and GNU ld.
The candidate was a cold Nix/Crane build; the fat baseline is the previously
recorded exact Nix build and was not rebuilt. These measurements do not
describe the public Nixpkgs-style package's compiler or derivation graph.

| Metric | Fat LTO / 1 CGU | ThinLTO / 16 CGUs |
|---|---:|---:|
| Inner final Cargo wall | 7m 36s | 1m 21.77s |
| Speedup | — | 5.6x |
| GNU `time` maximum RSS | not recorded | 5,680,712 KiB |
| Installed binary | 92,420,936 B | 122,616,792 B |
| Nix closure | 140,667,144 B | 170,863,000 B |

The x86_64 binary remains below the 125 MiB adoption limit. Both release
archive targets build with the same profile and preserve their expected
single-directory/single-binary layout. The final concurrent cross builds took
2m18.36s and 5,680,136 KiB maximum RSS for x86_64, and 2m20.53s and
7,899,268 KiB (7.53 GiB) for aarch64. The latter must be considered when
sizing that builder. GNU `time`'s statistic is not a sum of every concurrently
resident Cargo, rustc, and linker process; treat it as lower-bound
builder-capacity evidence rather than aggregate memory accounting.

A 10,000-event session-replay benchmark used 25 interleaved paired rounds and
found a **2.51% median wall-time increase** (fat 21.96 ms versus ThinLTO
22.50 ms per invocation; paired ratios ranged from 1.0179 to 1.0289). This is
within the 3% adoption limit. Startup-only probes found `--help` slower by
0.38 ms (16%) and missing-component diagnostics slower by 0.46 ms (10%).
Those probes deliberately detect small changes, but their sub-millisecond
absolute differences are not representative of long-lived Tau throughput.
Revisit the profile if startup becomes latency-sensitive.

There is no separate fat-LTO profile. Its measured source-build cost is high,
and the available runtime evidence does not establish enough user-visible
benefit to justify maintaining a second release intent.

## Reproducing the checks

Use `nix build -L -o /tmp/tau-nixpkgs .#tau` to validate the native
Nixpkgs-style package. On x86_64 Linux, use
`nix build -L -o /tmp/tau-thin-candidate
.#legacyPackages.x86_64-linux.release.tau` to reproduce the measured native
Crane-graph candidate. Use
`nix build -L -o /tmp/tau-thin-archives .#release-archives` for both
Crane-graph archive targets. Release logs print the exact timed Cargo command,
elapsed wall time, and maximum RSS. Compare the native Crane candidate against
the historical baseline with
`stat -c %s /tmp/tau-thin-candidate/bin/tau` and
`nix path-info -S "$(readlink -f /tmp/tau-thin-candidate)"`;
the baseline inner derivation was
`6fkhad6k9ld613pinsywy4pp6rf7cmi9-tau-0.1.0.drv` and its patched package was
`nmcmpvyckzr6sy5swikd6d8pnm45cicp-tau-0.1.0`. A cold candidate means its
package derivation and release-profile dependency-artifact derivation are
absent before the build; it does not mean bypassing Nix's source/toolchain
inputs.

The baseline store path is a recorded selector, not a permanent artifact; it
can disappear after garbage collection. To reproduce it, create a clean
checkout at `e2b061229d745ce84272b2188b9ded58d3910676`, where `.#tau` still
selected the Crane graph, and run
`nix build -L -o /tmp/tau-fat-baseline .#tau`. That expensive build is not
needed when the recorded baseline evidence still answers the comparison. When
the recorded package still exists, select it without rebuilding:

```console
ln -sfn /nix/store/nmcmpvyckzr6sy5swikd6d8pnm45cicp-tau-0.1.0 \
  /tmp/tau-fat-baseline
```

For archive validation, list each tarball with `tar tzvf`, require exactly one
top-level target-named directory containing executable `tau`, and search the
binary for both `__TAU_BUILD_GIT_REVISION_PLACEHOLDER____` and
`__TAU_BUILD_DATE`, requiring both searches to fail (`! grep -aqF ...`). The
Nix patch derivation also fails when it cannot replace both values or when
either remains. On each target's matching host (or an explicitly configured
emulator), run `tau --version` and compare
`tau component __missing_component_for_discovery_test__` with the expected
registry. The profile-independent source registry gate is
`cargo test --profile ci -p tau --test component_registry`; it does not execute
each archived target.

The recorded session-replay result belongs to the reference revision. Current
`tau session list` queries running daemons and no longer replays arbitrary
persisted session directories, so the old fixture generator and benchmark
driver were retired rather than silently measuring an empty live-session scan.
Check out the reference revision when auditing that historical measurement. A
future release-profile comparison that needs session replay must add a benchmark
with an explicit persisted-session input instead of repurposing the live-session
command.
