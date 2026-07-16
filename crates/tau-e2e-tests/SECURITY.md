# tau-e2e-tests security boundaries

The deterministic and VCR fixture families have different trust boundaries.

`DeterministicFixture` starts same-UID local subprocesses, which are trusted
configured extensions rather than a sandbox boundary. It ignores ambient Tau
startup override variables, checks the exact resolved extension allowlist
before spawn, uses exact canonical test binaries, and places generated config,
synthetic scenario data, session state, stderr logs, and provider trace below a
fresh private temporary root. The fake provider opens only the fixed
`fake-provider.trace` filename in its fixture-owned working directory. It has no
network, authentication, shell, evaluation, dynamic plugin, child-process,
prompt-control, or arbitrary input-file behavior. Scenario counts and bytes are
bounded; Nix additionally runs the exact lane in a network-denied build sandbox.
Children still inherit the ordinary process environment, but the closed fake
does not read provider credentials or use environment values as control.

Mismatch, startup, or exact-consumption errors retain the private root and print
its path. Successful roots are deleted unless `TAU_E2E_KEEP_ARTIFACTS=1`.
Artifacts contain only synthetic fixture data but remain private by default.
The embedded harness owns supervised process termination; this fixture asserts
successful harness shutdown, not terminal rendering or universal packaging.

`VcrFixture` is deliberately non-hermetic. It can use real provider credentials
and lets `core-shell` execute with the user's permissions. Its cassettes can
contain prompts, provider traffic, tool calls, output, and local paths. Run and
share them only under the policy in the crate README.

Re-review this boundary when adding scenario actions, subprocesses, environment
inputs, filesystem reads, network access, live control, concurrency, new tools,
or broader artifact retention.
