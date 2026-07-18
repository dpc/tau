# Release build profile

Tau's normal source release uses optimization level 3, ThinLTO, 16 codegen
units, aborting panics, and stripped symbols. The universal `tau` executable
links every bundled component, so fat LTO with one codegen unit turns its final
LLVM optimization into a long, mostly serial build. ThinLTO retains
cross-crate optimization while allowing LLVM to use the release builder's
cores.

The profile is canonical in `Cargo.toml`. This keeps plain
`cargo build --release`, the development shell, Nix/Crane packages, and release
archives on the same policy without ambient environment overrides. Nix
evaluation asserts the ThinLTO/16-CGU settings. Its final package derivation
also records Cargo's final elapsed-time status and GNU `time -v`'s command,
wall time, and maximum-resident-set statistic in the build log. Update the
assertion and repeat the measurements below when intentionally changing the
profile; do not remove the guard while the universal component graph remains
in one executable. The governing profile choice is in
[DECISION-release-build-profile](../specs/DECISION-release-build-profile.md).
The recorded adoption limits and re-evaluation triggers below are evidence for
that choice rather than permanent product requirements.

## Reference measurement

The reference is revision `e2b061229d74` on a 32-thread Ryzen 9 7950X3D,
using the pinned Nix Rust 1.96.0/LLVM 22 toolchain and GNU ld. The candidate
was a cold Nix/Crane build; the fat baseline is the previously recorded exact
Nix build and was not rebuilt.

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

Use `nix build -L -o /tmp/tau-thin-candidate .#tau` for the native package and
`nix build -L -o /tmp/tau-thin-archives .#release-archives` for both archive
targets. Release logs print the exact timed Cargo command, elapsed wall time,
and maximum RSS. Compare
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
checkout at `e2b061229d745ce84272b2188b9ded58d3910676` and run
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

The runtime fixture contained 100 sessions with 100 alternating durable
load/unload events each. After warming each binary, the benchmark pinned itself
to one available CPU and timed 25 fixed-seed, randomized-order pairs of ten
`tau session-list --sessions-dir FIXTURE` invocations, discarding output.
Record every paired ratio, absolute median, and range; do not infer sustained
runtime behavior from startup-only loops. Create the fixture with a temporary
`tau-session-inspect` example that opens a `SessionStore` and, for session ids
`benchmark-session-0000` through `0099`, appends 100 alternating
`SessionAgentLoaded`/`SessionAgentUnloaded` events for agent `main`:

```console
(
  set -e
  example_dir=crates/tau-session-inspect/examples
  example=$example_dir/release_profile_fixture.rs
  created_dir=0
  if [ ! -d "$example_dir" ]; then
    mkdir "$example_dir"
    created_dir=1
  fi
  if [ -e "$example" ]; then
    echo "refusing to overwrite $example" >&2
    exit 1
  fi
  cleanup() {
    rm -f "$example"
    if [ "$created_dir" = 1 ]; then rmdir "$example_dir"; fi
  }
  trap cleanup EXIT
  cp misc/generate-release-profile-fixture.rs "$example"
  rm -rf /tmp/tau-profile-sessions
  cargo run --profile ci -p tau-session-inspect \
    --example release_profile_fixture -- /tmp/tau-profile-sessions
)

python3 misc/benchmark-release-profile.py \
  --fat /tmp/tau-fat-baseline/bin/tau \
  --thin /tmp/tau-thin-candidate/bin/tau \
  --sessions-dir /tmp/tau-profile-sessions \
  --seed 2000 --rounds 25 --iterations 10
```
