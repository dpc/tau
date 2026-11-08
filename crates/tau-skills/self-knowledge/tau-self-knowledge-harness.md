---
name: tau-self-knowledge-harness
description: Use when operating the Tau harness daemon, including session serve, idle shutdown, upgrade and recovery, UI clients, sockets, attach/resume, or embedded runs.
advertise: false
---

# Tau harness daemon

The harness owns session state, extension supervision, agent/tool routing, and
durable logs. Each daemon keeps one immutable session for its lifetime.

## Start, attach, resume

- `tau` creates a durable session.
- `tau attach [SESSION]` joins a live daemon without ownership; omission opens
  the running-session picker. It verifies session identity and fails rather
  than starting a daemon when no match exists.
- `tau resume [SESSION]` starts a daemon from persisted state. A deleted explicit
  target fails without recreation. Without a target, Tau selects the sole
  unlocked session, opens a picker with locked rows disabled, or suggests attach
  if all targets are locked.
- `tau serve --session SESSION` stays alive across UI disconnects. Supply exactly
  one guard: `--create` requires absent state, `--existing` requires valid state,
  and `--create-or-existing` atomically creates or strictly resumes. Partial,
  malformed, symlinked, or locked state fails unchanged.

Interactive sessions normally stop when the last UI quits. `:detach` keeps the
daemon alive; `:quit-session` requests shutdown. SIGINT/SIGTERM normally shut
down in order; a second signal can interrupt cleanup.

Opt-in `session_idle_shutdown: 48h` in `harness.yaml` stops an idle daemon;
default `null` disables it. Accepted prompts, in-flight inference/tools, and
input waits keep it active; passive probes, attached UIs, and scheduled timers
alone do not. Shutdown preserves state but ends process-local timers. Resume
starts a new idle window. See `docs/session-startup.md`.

## Runtime sockets and startup

Runtime daemons hold
`${XDG_RUNTIME_DIR}/tau/harnesses/claims/<session-key>.lock` (fallback:
`/tmp/tau-$USER/harnesses/`) while owning storage and transport. The claim
winner reclaims only its session's stale socket and binds
`sockets/<session-key>.sock`. Clients verify the exact session on admission;
PIDs are diagnostic, not routing or liveness authority.

CLI-spawned harnesses use `tau component harness --initial-ui-stdio`:
stdin/stdout carry the initial UI protocol, and extension/session startup waits
for its subscription. Startup errors use `Disconnect` frames when possible.
Later UIs use the runtime socket. `TAU_READY_FD` no longer exists.
CLI-managed spawns remove `LISTEN_FDS`, `LISTEN_PID`, `LISTEN_FDS_FIRST_FD`,
and `LISTEN_FDNAMES`; normal runtime-dir startup does not use socket activation.

Direct `tau component harness` uses `TAU_SESSION_ID` or the default session ID,
binds its runtime socket, and has no initial stdio UI unless
`--initial-ui-stdio` is supplied.

The `message` tool accepts `<session-id>/<agent_id>` for another running harness;
the current session prefix resolves locally.

## Service bootstrap and upgrades

Pair `--bootstrap-prompt-file PATH` with `--bootstrap-id ID` on `tau serve`.
After readiness, Tau resolves canonical main when configured, submits the exact
UTF-8 file as literal prompt text, and keeps serving without waiting for output.
A conflicting startup role fails; without main, bootstrap creates an ordinary
durable parentless user agent.
`PATH=-` reads stdin to EOF once. Tau skips the same durable bootstrap ID on
restart before reading the source. This guarantees at-most-once admission, not
successful model completion. Tau does not promote or migrate old ordinary
bootstrap agents.
A new ID requests another generation on the same main; inspect ambiguous
failures before choosing one. Keep sensitive prompts out of world-readable
paths and the Nix store.

Before upgrading, finish active turns, note IDs/version, stop daemons with
`:quit-session`, preserve the old binary/install revision, and back up **both
config and state roots while stopped**. Replace the binary, check `tau --version`,
then resume and inspect one session before the rest. Existing processes do not
adopt a new executable or reread startup settings.

Tau does not promise backward compatibility. Keep the backup until tested;
do not use `tau init --force`, deletion, journal truncation, or repeated model
turns as generic recovery. See `docs/lifecycle-and-upgrades.md`.

## Foreground and embedded APIs

The config-resolving `run_daemon` API takes a caller-specified socket path.
It binds that path unless `listenfd` supplies listener zero, which must be a
Unix stream listener whose pathname matches exactly. The supervisor owns an
externally supplied socket; Tau does not remove it on shutdown.
This differs from CLI-managed runtime-dir startup.

Embedded helpers such as `run_embedded_message` run one in-process interaction
without a daemon socket, socket activation, or attach discovery.

## Waiting for input and background tools

Completion notices leave results queued:

- `wait({})` consumes the oldest completed unconsumed result. If none exists,
  it waits only when an owned call is running; otherwise it errors.
- `wait({"tool_call_id":"..."})` targets one call.
- `wait({"timeout_minutes":N})` waits for activating input without consuming
  input or background results. `N` must be positive; configuration clamps it
  to `wait_timeout_minimum_minutes` / `wait_timeout_maximum_minutes` (defaults:
  1 / 1,440). It is mutually exclusive with `tool_call_id`.

Activating input can interrupt background waits with successful
`internal: true`, `outcome: interrupted`, `reason: activating_input`, and
`mode: exact`, `exact_all`, or `any_background`. The result remains queued;
handle input, then retry the exact wait. A pending wait can suppress/remove a completion
notice but cannot retract a notice already delivered to the model.

## Manual compaction tools

The default-enabled `compact {}` durably compacts the caller after its complete
tool round; role policy can disable it. Separately enabled
`agent_compact {agent_id}` can compact another loaded agent. Both return
background acceptance; use `wait` for completion. Watches, messages, and ancestry
do not grant these tools.

The human/UI `:compact` normally rejects busy targets. The exception is a target
whose sole remaining foreground call is the same still-installed harness-owned
wait: Tau durably cancels it and closes the tool round before compacting.

## Supervised extension stderr

Durable sessions keep raw extension stderr in private per-session files.
`tau serve --mirror-extension-stderr` adds a default-off supervisor-stderr copy;
it is unavailable in interactive, resume, attach, component, ephemeral, and
prompt-stdin modes.

The mirror is escaped, bounded, lossy, and unredacted. It widens access to
journal readers; use raw files for exact bytes. Queue pressure can drop records;
sink/setup failure disables mirroring without blocking extension draining.
`TAU_LOG` controls only the child's producer filter.

Mirror records start with `tau: extension stderr:` and identify `extension`,
`generation`, `pid`, `boundary` (`line`, `chunk`, `eof`, `dropped`), and quoted
`message`. Ordering holds only within one generation. The mirror excludes
stdout/protocol, events, journals, debug JSONL, provider captures, and Configure
payloads. Load `tau-self-knowledge-debugging-extensions` for log inspection.
