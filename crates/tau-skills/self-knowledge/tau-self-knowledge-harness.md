---
name: tau-self-knowledge-harness
description: >
  Use this skill when the user asks about the Tau harness daemon, including how
  it starts, accepts UI clients, uses Unix sockets, handles activation modes,
  socket activation, readiness signaling, attach behavior, or embedded harness
  runs.
advertise: false
---

# Tau harness daemon

The harness is Tau's central daemon. It owns session state, extension supervision, event routing, agent prompt orchestration, tool routing, and durable logs. UI clients connect to the harness; extensions connect to the harness; the harness is the single coordinator between them.

## Runtime-dir daemon mode

When the CLI starts a harness child, it creates a runtime-dir daemon unless it is attaching to an existing daemon.

Common startup flow:

1. The CLI chooses or resumes a session id.
2. The CLI creates a child `tau component harness` process and passes session metadata through `TAU_SESSION_ID` and `TAU_SESSION_STATUS`.
3. The harness acquires the deterministic lifetime claim at
   `${XDG_RUNTIME_DIR}/tau/harnesses/claims/<session-key>.lock` (or the
   `/tmp/tau-$USER/harnesses/` fallback) before opening durable session storage.
4. The claim winner reclaims only that session's stale socket, binds
   `sockets/<session-key>.sock`, and holds the claim until transport and durable
   storage teardown finish.
5. Later clients derive the same path from the exact session id and complete an
   exact admission handshake before sending semantic frames. PIDs are diagnostic
   only and do not participate in discovery, routing, or liveness.

The runtime-dir harness path always binds its generated socket path itself. It does not use socket activation, because Tau attach/discovery expects the socket to exist at the generated runtime-dir path.

## CLI-spawned initial UI stdio mode

When the terminal UI or one-shot CLI helpers start the harness, the CLI uses the `tau component harness --initial-ui-stdio` mode.

In this mode:

- child stdin/stdout are reserved for the initial UI protocol connection,
- the CLI does not use those pipes for the readiness byte,
- the harness accepts stdin/stdout directly as the initial UI reader/writer,
- fatal startup failures are sent to the initial UI as protocol `Disconnect` frames when possible,
- extension and session startup wait until that initial UI has connected and subscribed,
- runtime markers are written after the startup state is ready for later socket attaches.

This prevents startup events from being missed by the UI that spawned the daemon. Later UIs still attach over the normal runtime-dir Unix socket.

The older readiness-pipe handshake (`TAU_READY_FD`) has been removed. CLI-spawned harnesses use initial UI stdio; attach mode connects to an existing socket and does not spawn a child.

CLI-managed daemon spawns explicitly remove `LISTEN_FDS`, `LISTEN_PID`, `LISTEN_FDS_FIRST_FD`, and `LISTEN_FDNAMES` from the child environment so unrelated socket-activation wrappers cannot accidentally change normal Tau startup.

## Attach mode

`tau attach [SESSION]` does not start a new harness. An explicit target resolves
the live daemon advertising that session; omission opens the running-session
picker. UI admission binds the expected session and rejects a stale or replaced
socket whose harness reports another identity.

Runtime discovery confirms marker identity through the live harness. If no
matching daemon exists, attach fails instead of silently starting a new one.

## Resume mode

`tau resume [SESSION]` starts a new harness for persisted state. An explicit
target must still exist after the child holds its session lock; a deleted target
fails without recreating an empty session. With no target, Tau auto-selects the
sole unlocked session, opens a picker with locked rows disabled when several
sessions are eligible, or reports that every persisted target is locked and
suggests `tau attach SESSION`.

## Foreground daemon APIs

The harness crate exposes the config-resolving `run_daemon` foreground helper
and test-only echo variants. These APIs take an explicit socket path from the
caller; pre-resolved configuration is internal so extension launch and runtime
policy cannot carry different settings snapshots.

Foreground daemon APIs bind the provided path directly unless socket activation provides a listener.

`tau serve --session ID --create|--existing|--create-or-existing` can pair
`--bootstrap-prompt-file PATH` with `--bootstrap-id ID`. After full readiness it
creates one durable parentless user agent and admits the exact UTF-8 file as a
literal prompt, then keeps serving without waiting for model output. `PATH=-`
reads stdin to EOF once. A durable sequence-zero marker makes the id at-most-once
across restarts: the same id skips before reading the source, while a different
id explicitly requests a new generation. Use private service credentials rather
than Nix-store paths for sensitive prompts. Managed services normally use
`--create-or-existing`: it atomically creates an absent exact-ID session or
strictly resumes valid state, while partial, malformed, symlinked, or locked
state fails unchanged.

## Socket activation

Foreground daemon APIs support socket activation via the `listenfd` crate.

Behavior:

- the harness checks `ListenFd::from_env().take_unix_listener(0)`,
- if no listener is present, it binds the requested socket path normally,
- if a listener is present, it must be a Unix stream listener,
- the listener's local pathname must exactly match the requested socket path,
- Tau does not remove the socket path on shutdown when the listener was externally provided.

This is intended for externally supervised foreground harness processes where the supervisor owns the socket. It is not used by the normal CLI-managed runtime-dir harness path.

## Direct `tau component harness`

Running `tau component harness` directly starts the harness component without the terminal UI parent. It uses the default session id when `TAU_SESSION_ID` is not set and binds its own runtime-dir socket.

This path is useful for debugging or embedding the harness component, but it does not receive an initial UI over stdio unless `--initial-ui-stdio` is supplied by the CLI-managed startup path.

Each runtime-dir harness remains bound to its startup session for its entire
process lifetime. Starting another session requires another daemon, normally
from another Tau invocation or terminal. Attach/send and cross-harness message
discovery use the deterministic session claim plus exact admission.

The harness-owned `message` tool can address another running harness with
`<session-id>/<agent_id>`. The current session prefix is treated as local; other
sessions are delivered with a dedicated external-message socket RPC on a helper
thread so the harness event loop is not blocked.

## Embedded one-shot runs

Embedded helpers such as `run_embedded_message` do not create a daemon socket. They construct a harness in-process, run one interaction, and shut it down. Socket activation and runtime-dir attach discovery do not apply to embedded one-shot runs.


## Waiting for input and background tools

Completion notifications mean the result is queued; they do not consume it.
`wait({})` consumes the oldest unconsumed completed background result for the
current conversation. Only if none is complete and an owned background call is
running does it wait for the next completion; otherwise it returns an error.
Use `wait({"tool_call_id":"..."})` when targeting a specific call: it is
unambiguous. A wait can suppress or remove a completion notification only while
it remains pending; a notification already delivered to the model stays visible.
Activating input can interrupt exact and bare background waits with successful
`wait_outcome: interrupted`, `wait_reason: activating_input`, and
`wait_mode: exact` or `any_background` headers. The interruption does not consume
the target result; retry the exact wait after handling the input.
`wait({"timeout_minutes":N})` instead waits for activating input without
consuming either input or background results. `N` must be a positive integer;
`harness.yaml` silently clamps it to the inclusive
`wait_timeout_minimum_minutes` and `wait_timeout_maximum_minutes` bounds (one
and 1,440 minutes by default). `timeout_minutes` and `tool_call_id` are
mutually exclusive.


## Manual compaction tools

The enabled-by-default `compact {}` tool durably compacts the calling agent
after its complete tool round. Role policy can disable it. The separately
enabled `agent_compact {agent_id}` capability may compact any other loaded
agent. Both return background acceptance and can be awaited with `wait`.
`:compact` remains a distinct human/UI command; watches, messages, and ancestry
do not grant either model tool. It preserves the ordinary busy rejection except
when the target's sole remaining foreground call is the same still-installed
harness-owned wait; Tau commits that wait's cancellation and closes the tool
round before starting compaction.
## Supervised extension stderr

Durable sessions keep each supervised extension's raw stderr in its private
per-session log. A foreground fixed-session service can additionally request
`tau serve --mirror-extension-stderr`. The option is default-off and unavailable
to interactive, resume, attach, component, ephemeral, and prompt-stdin modes.

The private file remains authoritative. The process-stderr copy is escaped,
framed, generation/PID attributed, bounded, and best-effort: queue saturation
can drop mirror records and sink failure disables mirroring without impeding
extension draining or harness progress. `TAU_LOG` remains solely the child's
producer-side filter. Custom extension stderr is unredacted, journal readers
are a wider audience, and the mirror never includes stdout/protocol, events,
journals, debug JSONL, provider captures, or Configure payloads.
The mirror worker uses an independent duplicate of inherited stderr when
possible; setup failure disables only mirroring. It may still contribute to the
shared sink's capacity, and ordinary harness tracing remains synchronous.
Records use the fixed prefix `tau: extension stderr:`, then validated
`extension`, immutable `generation`, child `pid`, `boundary`, and an escaped
quoted `message`. Boundaries are `line`, `chunk`, `eof`, or the loss notice
`dropped`. Fragments use a 4096-byte raw cap plus at most three UTF-8 lookahead
bytes, never split valid UTF-8, omit splitting LF, escape controls/invalid bytes
and bidi formatting characters, and end in exactly one LF. Ordering is
guaranteed only within one extension generation.
