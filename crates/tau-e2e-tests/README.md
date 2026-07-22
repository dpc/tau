# tau-e2e-tests

Hermetic deterministic end-to-end tests and separate opt-in VCR tests.

## Deterministic provider tests

The default workspace test run executes a test-only fake provider as a real
supervised subprocess. No environment opt-in, provider credential, network
access, shell, or VCR cassette is involved:

```sh
cargo nextest run -p tau-e2e-tests --test deterministic_provider
cargo nextest run -p tau-e2e-tests --test cancellation_liveness
cargo build -p tau --bin tau
cargo nextest run -p tau-e2e-tests --test core_resume
TAU_E2E_TAU_BIN=target/debug/tau cargo nextest run -p tau-e2e-tests --test core_shell_resume
```

The acceptance cases cover streaming/final text, a successful tool round
through `tau-ext-test-dummy`, typed errors followed by an explicit later turn,
exact cancellation with same-agent post-cancel liveness, bounded holds, fatal
provider disconnect without restart,
clean resume, concurrent lane isolation, one production-`agent_start`
main/worker cold resume with a preserved worker route and dropped automatic
watch, explicit post-resume watch recreation with exact new-subscription
notifications, loaded/unloaded/ephemeral membership composition, and startup
rejection of invalid
scenario config.
The Unix-only `core_resume` gate additionally spawns the exact universal `tau`
under a real PTY twice. It completes `restart_test_dummy`, reaps Boot A, resumes
with explicit `tau -r <session-id>`, and checks the actual VT projection never
repaints the completed row as pending. A replay-aware side UI observer and typed
CBOR `SessionStore`/`AgentStore` snapshots independently prove replay boundaries,
stable identity, unchanged durable prefix, and one fresh same-agent prompt.
The separate headless `core_shell_resume` gate runs that universal binary as the
bundled `component ext-shell`, exposes only `workdir` and `edit`, and proves a
canonical per-agent workdir plus a relative context-checked edit survive full
daemon and extension replacement. Its scratch canary and exact-byte assertions
are safety oracles, not a filesystem sandbox or directory-lock test.
The independent headless `cancellation_liveness` gate holds two exact provider
lanes, cancels each harness-minted prompt id once, and then completes a fresh
prompt on the second selected agent's existing lane. It does not exercise PTY
restore or core-shell.
The `session_restore` case uses the production harness-owned `agent_start`
built-in under exact two-role policy. It cleanly replaces the daemon, restores
the completed durable worker as an idle independently addressable
conversation, and proves the old automatic watch does not re-fan out fresh
worker activity. A separate fresh fixture recreates the watch through production
`agent_watch`, proves the initial snapshot is non-model, and correlates one
direct worker turn's prompt/running/response/idle facts to the new subscription.
A third fixture adds a promptless ephemeral worker through `UiCreateAgent`, then
seeds a valid durable worker load/unload history while the stores are unowned.
Cold resume restores only the two current durable routes, reports the seeded
worker only in history, rejects both absent routes, and leaves no ephemeral
journal or durable membership record.
The fixture retains its private artifact root on panic, `run_turn` failure, or
any daemon path that exits before exact consumption succeeds. Retained artifacts
include generated config/scenario, durable events, extension/daemon stderr, and
the bounded semantic provider trace. See
[`SPEC-tau-e2e-deterministic-provider`](specs/SPEC-tau-e2e-deterministic-provider.md)
for the coverage ceiling.

## VCR tests

Required for active E2E runs:

- `TAU_VCR=record-if-missing` to record missing cassettes, or
  `TAU_VCR=replay-only` to require existing cassettes.
- `TAU_VCR_DIR=/path/to/cassettes`.
- `TAU_E2E_MODEL=<provider-profile/model>` for the generated harness role.

Optional:

- `TAU_E2E_TAU_BIN=/path/to/tau` to choose the trusted local Tau binary used for
  provider and shell extensions. Defaults to `tau` on `PATH`.
- `TAU_E2E_SESSION_ID=<id>` to override the stable cassette/session id.

Unset, empty, or `off` `TAU_VCR` skips the tests. Active VCR modes fail loudly
when required VCR configuration is invalid or incomplete.

E2E tests should assert the relevant harness or tool progress from
`VcrFixture::run_turn` rather than only checking that the turn completed.
