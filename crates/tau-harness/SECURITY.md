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

`agent.stats_updated` and `agent.watches_updated` are likewise transient,
content-free operational events. They may expose local agent ids, watch
relationships, runtime state, tool counters, and token counts, but must not carry
prompts, responses, tool arguments, or tool outputs.

Stats samples are transient and must not be folded into agent transcripts,
editor state, prompt stdin output, or final assistant rendering. The harness
derives byte counts only after provider prompt ownership validation and clears
runtime stats on every terminal or abandoned turn path so a later prompt cannot
inherit stale timing or byte counters.

Providers may send `provider.response_updated.semantic_output` as a
provider-to-harness content-free byte snapshot for non-visible generated output.
The harness must consume and strip that field before subscriber delivery, and
must suppress stripped-empty provider updates so `agent.turn_stats_updated`
remains the only public UI-facing progress event.
