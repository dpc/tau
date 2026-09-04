# tau-ext-swarm

`tau-ext-swarm` publishes one Tau session to one pinned Tau Swarm Iroh peer. It
folds replay through `session.replay_complete` before publishing, routes remote
prompts and blocker answers through Tau's canonical internal-prompt path, and
registers the agent-scoped `task_info`, `task_blocker`, and `task_update` tools. They are disabled
by default even when the extension runs; opt selected roles into their shared
`swarm` tool group:

```yaml
agents:
  role_groups:
    engineer:
      enable_tool_groups: [swarm]
```

Use `enable_tools: [task_info]`, `[task_blocker]`, or `[task_update]` to expose
only one tool. The old `blocker` and `update` names are not aliases. These are
unprefixed instance names. With `tool_prefix: work`, use `work_swarm`,
`work_task_info`, `work_task_blocker`, or `work_task_update` in role policy.
Task IDs have no owner: any loaded agent granted `task_info` may replace
metadata for any valid task ID in its current session.

## Configuration

Configure the bundled, default-disabled `std-swarm` instance in `harness.yaml`:

```yaml
extensions:
  std-swarm:
    enable: true
    require: false
    secrets:
      swarm_worker_secret: {}
    config:
      endpoint:
        peer_id: "kix..."
        relay_url: "https://relay.example.net" # optional
        direct_addresses: ["203.0.113.10:11204"] # optional
      credential_id: "tau-worker-production"
      credential_secret: "swarm_worker_secret"
      hostname: "builder-01" # optional; UTF-8 system hostname otherwise
      reconnect:
        initial_delay_ms: 250
        maximum_delay_ms: 30000
        jitter_per_mille: 200
      command_timeout_ms: 25000
      limits:
        command_entries: 1024
        command_bytes: 16777216
        blocker_entries: 256
        blocker_bytes: 4194304
        update_entries: 256
        update_bytes: 8388608
        task_info_entries: 4096
        change_history_entries: 4096
        change_history_bytes: 33554432
        publication_bytes: 8388608
        agent_entries: 4096
        watch_entries: 16384
        submission_queue_entries: 16
```

`endpoint.peer_id`, `credential_id`, and `credential_secret` are required.
Identity-only endpoints are valid; N0 discovery can resolve them. Relay and
direct addresses are optional hints, not alternate identities. `peer_id` is the
only pinned identity. Relay URLs must parse and contain at most 2,048 bytes;
direct hints must be distinct socket addresses and contain at most 16 entries.
Every config object rejects unknown fields.

The referenced credential must be declared in the extension's `secrets` map and
arrive in Configure. Its value must contain 1..=4,096 bytes and is never read
from an ambient extension-specific environment variable. Credential IDs contain
1..=128 non-control UTF-8 bytes. Hostnames contain 1..=255 ASCII bytes, start
and end alphanumeric, and otherwise use only alphanumeric, `.`, `_`, or `-`.

| field | default | accepted range |
|---|---:|---:|
| `reconnect.initial_delay_ms` | 250 | 10..=60,000 |
| `reconnect.maximum_delay_ms` | 30,000 | 10..=300,000 |
| `reconnect.jitter_per_mille` | 200 | 0..=1,000 |
| `command_timeout_ms` | 25,000 | 1,000..=25,000 |
| `limits.command_entries` | 1,024 | 1..=16,384 |
| `limits.command_bytes` | 16 MiB | 1..=256 MiB |
| `limits.blocker_entries` | 256 | 1..=4,096 |
| `limits.blocker_bytes` | 4 MiB | 256 KiB..=4 MiB |
| `limits.update_entries` | 256 | 1..=4,096 |
| `limits.update_bytes` | 8 MiB | 256 KiB..=64 MiB |
| `limits.task_info_entries` | 4,096 | 1..=4,096 |
| `limits.change_history_entries` | 4,096 | 1..=65,536 |
| `limits.change_history_bytes` | 32 MiB | 1..=128 MiB |
| `limits.publication_bytes` | 8 MiB | 1..=8 MiB |
| `limits.agent_entries` | 4,096 | 1..=65,536 |
| `limits.watch_entries` | 16,384 | 1..=262,144 |
| `limits.submission_queue_entries` | 16 | 1..=64 |

The initial reconnect delay must not exceed the maximum. Only indeterminate
transport failures retry. Authentication rejection, protocol incompatibility,
and projection invariant failure are terminal. A command timeout covers local
queue admission plus exact canonical Tau loopback. It caches an indeterminate
outcome, closes the connection, and makes an exact retry return that outcome
without resubmitting Tau work.

## State and bounds

Command IDs form one no-eviction namespace across prompts and blocker answers.
Their byte limit counts retained request and cached textual-result UTF-8 bytes.
Blocker limits apply to each owner's full current process-memory history. Update
limits cover the unacknowledged immutable outbox. Task metadata retains at most
`limits.task_info_entries` current values and 8 MiB of aggregate canonical
task-ID, title, and description bytes. Change-history limits count
logical UTF-8 fields; `publication_bytes` separately bounds each encoded change
and current encoded snapshot. Falling behind retained changes forces a new
snapshot. Projection overflow or malformed lifecycle replay invalidates and
clears the projection; mutating tools reject until the extension restarts and
replays the same bound session. A terminal worker or panic unwind likewise makes publication health
indeterminate immediately; panic-abort builds terminate the extension process.
The `task_info`, `task_blocker`, and `task_update` tools then reject before mutation rather than
reporting success for state that no live worker can publish.

All command deduplication, task metadata, blocker history, updates, and acknowledgements live
only in extension process memory. Iroh reconnect within that process preserves
them. Extension restart clears all of them. The extension generates one Tau
Swarm application-incarnation ID at
process startup and retains it across ordinary reconnects. A replacement process
declares a new incarnation, so the server fences ambiguous commands and lifecycle
state owned by the old process.

## Verification

Unit tests cover strict config, exact tool names/group/prefixes and no aliases,
task-info schema and canonicalization, transactional entry/content/encoded
bounds, shared revision order, coherent snapshot/live views, cancellation-safe
change waits, initial session replay, and reconnect convergence after an
indeterminate live submission. Existing focused coverage retains blocker and
update lifecycle, acknowledgement, replay, and capacity invariants. A real
`TauExtensionRunner` test verifies registration and paired historical/live
startup selectors. A hermetic fake Swarm transport drives credential
authentication, indeterminate retry, terminal rejection, and complete
resnapshot. A concrete `IrohConnector` test verifies peer mismatch fails before
network connection. The exact `tau-swarm-core` 0.4.0 dev dependency runs
the real published Iroh server through v0 authentication, declaration, task-info
snapshot/restart omission, remote prompt dispatch, and direct application
loopback. A composed
`TauExtensionRunner` vertical additionally drives Configure, replay boundaries,
worker startup, published snapshot observation, transient internal-prompt
emission, matching canonical Tau submission, and the accepted remote result.
Coverage of worker return, retirement ordering, and panic-unwind retirement
verifies health authority, tool-result authority, unchanged state after
rejection, and bounded cleanup. A production-FIFO saturation regression
verifies that detached internal-prompt overload remains a cached indeterminate
command result.
Tau's workspace checks also build the bundled component and default-disabled
harness configuration.
