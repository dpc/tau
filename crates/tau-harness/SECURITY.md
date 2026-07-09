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

## Provider response stats

Provider response stats are public, content-free metadata on transient `provider.response_updated` events. They may carry routing ids, originator, cumulative backend response byte counts, and elapsed provider request time, but must not include provider text snippets, raw wire payloads, tool names, raw arguments, tool results, prompt text, or backend diagnostics.

The harness validates provider prompt ownership and rewrites the public `agent_id` from prompt ownership before broadcasting provider updates. It must not strip `response_stats`, derive its own response-throughput counters, or emit a harness-owned response-throughput projection. Stats samples are transient and must not be folded into agent transcripts, editor state, prompt stdin output, or final assistant rendering.

## Agent watch notification boundary

`agent_watch` is a model-visible cross-agent content exposure boundary. A watcher
may receive hidden internal prompts containing the watched agent's final response
text or the text of a user prompt accepted by the watched agent. These
notifications must be clearly labeled as watch notifications, not as explicit
`message` tool deliveries:

- `[tau-internal]: Watched agent <agent-id> emitted a response`
- `[tau-internal]: Watched agent <agent-id> received a user prompt`

The watch path must not forward internal steering prompts, background or
foreground tool-completion prompts, explicit `message` tool deliveries to the
watched agent, or other hidden/non-user inputs. A completed `agent_start` result
is the started child agent's terminal final response to its direct delegating
watcher and remains watchable under the response label.
