---
name: tau-self-knowledge-ext-swarm
description: Use this extension skill for Tau std-swarm setup, Iroh endpoint pinning, worker credentials, blockers, updates, reconnects, or process-memory lifetime.
advertise: false
---

# Tau std-swarm extension self-knowledge

`std-swarm` is Tau's disabled-by-default Tau Swarm bridge. Enable it only after
setting the pinned `config.endpoint.peer_id`, public `credential_id`, and
`credential_secret` name. Declare that name under the extension's `secrets`;
the extension never reads an ambient credential environment variable.

`endpoint.relay_url` and `endpoint.direct_addresses` are optional route hints.
With neither set, Iroh resolves the pinned identity through standard N0
discovery. Hints never replace or weaken the `peer_id` pin.

The `blocker` tool supports `add`, `cancel`, and `list`. Listing returns the
invoking agent's active, answered, and cancelled blockers for recovery after
compaction. `swarm_update` publishes an immutable title/description and
optional task ID. Ownership always comes from the invoking Tau agent, not tool
arguments.

```text
add:    {action,title,description,recommended_answer?,task_id?}
        -> {status:"active",blocker_id,revision:1}
cancel: {action,blocker_id,reason?} -> cancelled record
list:   {action} -> all owned records in opening order
```

Command deduplication, blocker history, pending updates, and acknowledgements
live only in extension process memory. An ordinary Iroh reconnect preserves
them. A session switch retains command deduplication but clears the other
session-specific state. Tau restart or extension restart clears all of them.
The extension retains one Tau Swarm application-incarnation ID across ordinary
reconnects. A restarted process declares a fresh ID, which lets the server fence
ambiguous commands and lifecycle state owned by the previous process.
