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
notifications, loaded/unloaded/ephemeral membership composition, two-worker
restore with reverse-creation activation, live `active` publication/retention,
input-free cold recomputation of delegated `active_auto`, and ID-keyed
ordering/isolation, a
synchronized held-worker crash followed by two dispatch-uncertain fail-closed
resumes, an acknowledged interrupted worker foreground tool repaired exactly
once across two cold resumes, and startup rejection of invalid
scenario config.
The Unix-only `core_resume` gate additionally spawns the exact universal `tau`
under a real PTY twice. It completes `restart_test_dummy`, reaps Boot A, resumes
with explicit `tau resume <session-id>`, and checks the actual VT projection never
repaints the completed row as pending. A replay-aware side UI observer and typed
CBOR `SessionStore`/`AgentStore` snapshots independently prove replay boundaries,
stable identity, unchanged durable prefix, and one fresh same-agent prompt.
A live-attach case starts one fixed PTY, completes one closed text turn, then
attaches a second exact public CLI by explicit session. Normalized semantic row
classes cover session,
extension-ready, initialization, prompt, response, editable-prompt, and status
order in both views; a late replay-aware observer separately fixes protocol
delivery order, while an exact one-action provider trace,
bounded synchronization, process-group teardown, and clean runtime artifacts are
independent oracles. It does not claim byte-identical rendering, broader
multi-client ordering, tools, production providers, or multi-agent behavior.
A companion attach case holds one correlated provider prompt, attaches only
after hold readiness, proves both UIs show the same selected agent, and then
proves both settle on its editable status after one exact cancellation. Typed
observer prompt facts exclude duplicate submission, observer stats own the
running-to-idle transition, and the provider snapshot excludes duplicate
cancellation.
Its multi-agent case instead creates and completes the S1 production
main/worker pair in a headless Boot A, then runs the universal resumed Boot B
under the PTY. It selects both restored transcripts only by stable ID, submits
one targeted worker follow-up, and combines the narrow VT evidence with directed
rosters, replay metadata, exact fake-provider consumption, typed multi-agent
store prefixes/suffixes, and bounded process/socket cleanup.
An independent PTY case keeps a fresh target session agentless until one
authenticated bare inter-session message auto-starts its first receiver. It
requires the live `active_auto/running` snapshot and the correlated hold-ready
notice rendered later by the target PTY, uses the ordinary Ctrl-J binding once
to select the exact recipient, then explicitly cancels the provider hold.
The separate headless `core_shell_resume` gate runs that universal binary as the
bundled `component ext-shell`, exposes only `workdir` and `edit`, and proves a
canonical per-agent workdir plus a relative context-checked edit survive full
daemon and extension replacement. Its scratch canary and exact-byte assertions
are safety oracles, not a filesystem sandbox or directory-lock test.
The independent headless `cancellation_liveness` gate holds two exact provider
lanes, cancels each harness-minted prompt id once, and then completes a fresh
prompt on the second selected agent's existing lane. It does not exercise PTY
restore or core-shell.
The first three `session_restore` fixtures use the production harness-owned
`agent_start` built-in under exact two-role policy. The first cleanly replaces
the daemon, restores
the completed durable worker as an idle independently addressable
conversation, and proves the old automatic watch does not re-fan out fresh
worker activity. A separate fresh fixture recreates the watch through production
`agent_watch`, proves the initial snapshot is non-model, and correlates one
direct worker turn's initial work-status, prompt, and response facts to the new subscription.
A third fixture adds a promptless ephemeral worker through `UiCreateAgent`, then
seeds a valid durable worker load/unload history while the stores are unowned.
Cold resume restores only the two current durable routes, reports the seeded
worker only in history, rejects both absent routes, and leaves no ephemeral
journal or durable membership record.
A fourth fixture configures one main with only `agent_start` plus two distinct
tool-free worker roles. It cold-restores all three durable routes, compares
roster rows as an ID-keyed set, activates workers in reverse creation order, and
proves retained lane ownership and per-worker transcript isolation. Each accepted
worker prompt publishes live `active` stats and leaves the worker `active` after
completion; an input-free second cold resume consumes no provider action and
recomputes both delegated workers as `active_auto`.
A fifth fixture reuses the exact two-role `agent_start`/`agent_watch` surface. It
correlates one held worker prompt across the durable dispatch checkpoint, decoded
fake cursor, and live `hold_ready` trace before killing the private process
group. Boot B and Boot C require both routes, the mandatory dispatch-uncertain
warning, and zero automatic worker submission; only Boot B creates a fresh watch
whose initial typed provider status identifies the checkpointed prompt. This
proves a conservative harness response at the synchronized cut, not backend
acknowledgement, exactly-once work, transactional cursor/journal persistence, or
retry, abandonment, or recovery behavior.
A sixth fixture gives only the worker the closed no-side-effect dummy tool. It
kills after one durable request/start pair and canonical hold readiness, then
requires the eager restart repair pair, no live tool redispatch, and one explicit
repair-aware worker continuation with an exactly balanced error result. Boot C
receives no further input and must preserve the exact Boot B membership,
execution-restore, and agent streams without another repair pair. This is a
conservative foreground-tool repair oracle, not evidence of exactly-once effects
or a general recovery operation.
A seventh fixture composes a quiescent main and completed worker with one held
dispatch-uncertain worker and one interrupted-tool repair worker. The first
resume emits only the uncertain worker's warning and the repair worker's sole
durable error; the second receives no input, emits no provider work, and must
preserve every durable stream and ID-keyed current/history roster. One explicit
repair-worker continuation after those assertions consumes only its retained
fake lane. The uncertain worker remains fail-closed and unterminated.
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
