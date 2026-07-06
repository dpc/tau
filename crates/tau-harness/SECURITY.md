# tau-harness security notes

## Daemon IPC listener lifecycle

The harness daemon listener is local IPC for trusted same-user Tau clients and
runtime discovery. Listener ownership and cleanup must preserve the socket
identity checks in `tau-socket`; a daemon-owned listener should outlive cloned raw
listener fds used by accept-forwarder threads, and socket-activated listeners must
not be unlinked by the harness.

Accept-loop shutdown must use an owned wake/cancellation primitive tied to the
accept thread, not polling sleeps and not the filesystem socket pathname. Runtime
socket paths can be removed or replaced while a cloned listener fd remains live, so
shutdown correctness must not depend on reconnecting to that path. Internal wake
traffic is control-plane state only and must never be forwarded as a harness
client.

## Agent turn stats

`agent.turn_stats_updated` is a harness-owned operational event, not transcript
content. It must remain content-free: only cumulative semantic-output byte
counts, elapsed turn duration, routing ids, and originator are allowed. Do not
include provider text snippets, tool names, raw arguments, tool results, prompt
text, or backend wire payloads in stats events.

Stats samples are transient and must not be folded into agent transcripts,
editor state, prompt stdin output, or final assistant rendering. The harness
derives byte counts only after provider prompt ownership validation and clears
runtime stats on every terminal or abandoned turn path so a later prompt cannot
inherit stale timing or byte counters.
