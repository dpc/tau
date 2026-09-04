# Agent messaging tool

## External transport foundation

Message bridges publish six transient `message.*_reported` events through
ordinary event emission: delivered, edited, deleted, reaction added, reaction
removed, and sent. After each report commits, the harness stamps the
authenticated configured publisher and publishes the corresponding immutable
durable `message.*` fact. Slack, Telegram, and XMPP all use this interface;
bundled IM bridges have no legacy user-message prompt path.

Valid committed facts project to compact `<message event="…">` provider
context. External content and publisher-provided metadata remain untrusted data;
they grant no identity, routing, tool, or instruction authority. Live incoming
facts immediately create a payload-free wake; branch-applicable transcript
placement and provider dispatch may follow later. Replay reconstructs context
without creating a runtime wake. `message.sent` projects as assistant context and
never activates the model by itself.
The common prompt shape is defined by
[`SPEC-external-message-reports-and-facts`](../specs/SPEC-external-message-reports-and-facts.md).

Transport admission, duplicate suppression, native identity interpretation,
reply routes, proactive destinations, retries, and remote-send policy belong to
the publishing extension. Opaque reply and reaction references may identify
extension-local runtime state, but they are not generic harness capabilities and
replay does not recreate that private authority.

These external message facts are separate from the harness-owned inter-session
and agent-to-agent message events documented below.

The harness-owned `message` tool lets an agent send an asynchronous short text
note to another agent or session. Success returns
`Message committed: <message-id>; recipient was live; response not guaranteed`.
This reports harness acceptance and correlation, not inference, reply, or
completion. Every successful send is recorded as an
`agent.message_sent` sender projection; agent recipients also get a separate
`agent.message_received` recipient projection with the same `message_id`.
Agent-to-agent UI display depends on `:set show-messages`. When shown fully in
the no-selection overview, a message renders as:

```text
■ Message from <sender> to <recipient>:
<message>
```

In a selected-agent transcript, the selected endpoint is implicit. Incoming
messages render `■ Message from <sender>:` and outgoing messages render `■ Message
to <recipient>:`. Ordinary directed communication is always labeled `Message`;
it has no separate response presentation. All full forms preserve the message
body below the header.

Agent endpoint identity always remains visible. When the CLI knows
authoritative session metadata, it supplements either endpoint independently,
for example `■ Message from @reviewer-YiBh (research findings) to
@reviewer-VVSq (final review)`. Unknown agent endpoints and peers without an
advertised name keep their existing id-only labels. Names are escaped and
bounded UI metadata; they do not enter the message body, change routing, or
become identity authority. Historical
blocks use the latest folded name metadata when re-rendered, while their
immutable semantic message events remain unchanged. See
[`SPEC-tau-cli-agent-message-labels`](../crates/tau-cli/specs/SPEC-tau-cli-agent-message-labels.md).

Watched-agent working, waiting, done, and blocked reports are structured status updates,
not empty messages. They render as one line:

```text
▤ Status update from <sender>: <phase-emoji> <reported task>
```

The phase emoji use the [agent-status legend](list-agents.md#attached-picker).

`:set show-messages` modes are:

Agent-to-agent message projections obey `:set show-messages`:

- `none`: no UI indication or history of agent-to-agent messages
- `self-summary`: no UI indication for agent-to-agent messages
- `self-full`: no UI indication for agent-to-agent messages
- `all-summary`: one-line no-content indication for agent-to-agent messages
- `all-full`: full content of all messages

The CLI's no-agent-selected screen is also an all-agent message overview. It
shows one entry per agent-to-agent message according to the current
`show-messages` mode, deduplicating the sender and recipient projections by
originating session and `message_id`. Ctrl-K/Ctrl-J cycle through that overview
and the active agents. Submitting a prompt from the overview still starts a new
agent. Ordinary `Message` events and watched response/prompt notifications are
overview content. Structured provider-status, work-status, and long-wait
records stay in the watcher transcript.

The overview contains messages observed by that CLI and catch-up projections
replayed for agents that are loaded when it attaches. It is not a durable
session-wide message index, so a new CLI does not recover earlier messages after
both endpoints have unloaded.


## Send to another agent

Start the other agent with `agent_start`. The child starts with fresh transcript context, but inheritable per-agent metadata such as each shell instance's workdir is copied from the parent. Starting it does not set a display name; parent/child topology is tracked separately rather than encoded in a name. The child reports its own user-visible task title through `status`. It can discover its authoritative identity and session metadata with `self_info`; custom prompt templates may still place the explicit `agent_id` template input when desired. The `agent_start` tool completes immediately with `self_agent_id` and `sub_agent_id` metadata, while the sub-agent's response text arrives through the `agent_watch` async response-notification path:

```text
tau_internal: true
self_agent_id: engineer_a
sub_agent_id: engineer_b

Agent started; responses will arrive through agent_watch notifications.
```

Use `sub_agent_id` as `recipient_id`:

```text
message({"recipient_id":"engineer_b","message":"Please also inspect crates/tau-cli/src/event_renderer.rs."})
```

The UI may display, summarize, or hide agent-to-agent messages depending on
`:set show-messages`. The recipient's durable `agent.message_received` fact is
also its sole model payload: provider context replaces only exact `</message>`
collisions in the body of a
sender-labelled outer `<tau_internal>` envelope containing an exact-close-framed
`<message>` body. Cross-session messages similarly escape
`<tau_peer_message>` before the harness escapes and applies the outer
`<tau_internal>` frame. Live delivery uses a
payload-free runtime wake and does not persist a second submitted/steered prompt.
Cold replay restores the same wrapper as context without waking the model.

## Send a message to another active session

Use `&<session-id>` to send a message to another session:

```text
message({"recipient_id":"&01JZ...","message":"Please compare this with your session."})
```

The target session chooses one eligible receiving agent, preferring idle over
running and least-recently routed then agent id. A busy eligible agent is reused;
sending to a session is never enumeration or broadcast. Success returns the
resolved canonical `session/agent` address and delivery status. If no eligible
agent exists, the target walks roles with
`inter_session_auto_start` in deterministic configured order, skipping disabled
or unavailable roles/models. Otherwise sending to the session fails. A live
target with no configured or eligible receiver is distinct from a target that
cannot be discovered or reached: the caller sees `target live; no receiver; set
\`inter_session_receiver\``. Target-local diagnostic text is not relayed across
the inter-session boundary. Multiple receiving roles may span role groups;
concurrent live sends coalesce onto one newly created endpoint.

Use `&<session-id>/@<agent-id>` or `<session-id>/<agent-id>` to send to a
specific agent in another session. This known-address behavior works even when
the target role is not an inter-session receiver.

Use `<session-id>/<agent_id>` as `recipient_id` to address an agent owned by
another running harness daemon:

```text
message({"recipient_id":"01JZ.../engineer_b","message":"Please compare this with your session."})
```

If the session id is the daemon's immutable session, the address is treated as a
local agent id. Otherwise the message tool performs runtime-daemon discovery and
a dedicated external-message RPC on a helper thread, so the harness event loop is
not blocked by socket lookup or target validation. On confirmed delivery, the
sender transcript records `agent.message_sent` with recipient
`external_agent { session_id, agent_id }`; the recipient transcript records
`agent.message_received` with `sender_session_id`. The CLI renders external
addresses as `session/@agent`; transport and model-facing identities retain
their canonical typed representation.

Inter-session delivery is best-effort at-least-once. During normal live operation Tau
reports success only after the exact receive projection commits. If the target
crashes or the connection is lost after that commit but before acknowledgement,
a retry can deliver a duplicate receive occurrence and activation; Tau does not
provide distributed exactly-once deduplication across sessions.
The same crash ambiguity can duplicate an auto-started agent, model work, or
spend. Before creation, each receiving agent is limited to 32 queued
inter-session inputs, 256 KiB of queued inter-session message body, 60 accepted
inputs per rolling minute, and 64 KiB per message.

Roles that explicitly enable the `session_discovery` tool group can use
`session_list({query?, limit?})` to find live sessions available for
inter-session messaging. The snapshot is bounded and racy and contains only
session id, project basename, and current-session status. It does not enumerate
remote agents. The independent `agent_discovery` group enables
`agent_list({query?, role?, group?, state?, limit?})`, which lists only redacted
pending, live, restored-unavailable, and stopped agents in the caller's current
session.

External delivery failures (no daemon, stale socket, ambiguous session, wrong
active target session, stopped/unknown recipient) fail the tool call and do not
record a successful sender-side projection. In contrast, the fixed `target
live; no receiver; set \`inter_session_receiver\`` error means discovery and
transport reached a live harness, but its bare receiver policy cannot accept
the message. Tau does not relay target-local diagnostic text.

Inbound inter-session text is authenticated agent content, not a harness
instruction. Only exact `</tau_peer_message>` collisions are replaced inside a
distinct `tau_peer_message` context envelope carrying harness-authored sender
session and agent identity. The harness then escapes any exact
`</tau_internal>` collision in that complete payload and projects its outer
`<tau_internal>...</tau_internal>` envelope from typed harness provenance.

## Watch another agent's responses

Use `agent_watch` to enable or disable hidden async notifications for another
agent's final responses and received user prompts:

```text
agent_watch({"agent_id":"engineer_b","enable":true})
```

Successful enable first reports the watched agent's current self-reported work
status. Later `working`, `waiting`, `done`, `blocked`, and harness-invalidated `unknown`
transitions arrive as typed notifications. The harness does not infer semantic
progress from provider or tool activity.
Enabling requires the target agent to be live. An enable request for a stopped
or unknown target, or a `restored_unavailable` target whose old completion route
cannot be rebuilt, fails without creating any watch relation or notification
state; after reloading the same agent id, explicitly enable a fresh watch.
The session-local watch topology is a directed acyclic graph. Enabling
`watcher -> watched` fails without changing watch state if `watched` already
reaches `watcher`; self-watch is also rejected. Re-enabling an existing edge
keeps the normal refresh behavior, while disabling a relation never performs
cycle analysis. The reachability check and accepted mutation are serialized in
one harness event-loop operation.
The durable notification carries a watch-subscription id, an initial-snapshot
marker, and a runtime-local work epoch. The initial snapshot is client-visible
status only and is not injected into the watching agent's model context; later
genuine transitions are injected as isolated internal notifications.

While provider work is retrying, the harness suppresses early live
notifications through `agent_watch_retry_notification_threshold` in
`harness.yaml`. The default is `5`. Above that inclusive threshold, the watcher
receives a sanitized structured status on the first occurrence of each closed
retry category; repeated attempts in the same category update the current
snapshot without waking the model again. A value of `0` restores delivery
consideration from attempt zero while retaining category dedupe.
Per watch subscription, turn generation, and provider prompt,
`recovering_context` is delivered once, while each sanitized `blocked` and
`dispatch_uncertain` category is delivered once. Terminal failures are always
delivered.
Enabling or re-enabling a watch returns the current snapshot and emits an initial
client-visible, non-model event, including when the current retry was suppressed
from live delivery; it never replays the attempt history. Provider status remains
independent of semantic work status.
Provider bodies, human status/error text, headers, account data,
secrets, and prompt content never cross this boundary; see
[Watched provider status](events.md#watched-provider-status) for the wire shape.
Unloading either the watcher or watched agent disables every watch involving
that endpoint and discards its current provider snapshot and notification
dedupe state. These relations do not return if the same agent is loaded again;
enable a new watch to create a fresh subscription.

Before an unexpected watched-agent unload is pruned, surviving watchers receive
one content-free typed stopped lifecycle notification. Expected completion and
explicit cleanup prune without that failure notification.

Work-status titles are model-authored metadata. Tau bounds and escapes them
before presentation; they are not routing data or trusted instructions.

`agent_start` automatically enables watching for the sub-agent it creates. A
watch response notification is delivered to the watching agent as a hidden
typed context projection that is distinct from an explicit `message` tool delivery:

```text
<tau_internal>Watched agent engineer_b emitted a response

<response>
Task result text.
</response></tau_internal>
```

If the watched agent receives a direct user prompt while the watch is active,
watchers also receive a hidden context notification when that prompt becomes
part of the watched agent's active turn, before the watched agent's later
response notification for that turn:

```text
<tau_internal>Watched agent engineer_b received a user prompt

<prompt>
User follow-up text.
</prompt></tau_internal>
```

Watch notifications are deliberately narrow. They do not forward internal
steering prompts, background/tool-completion prompts, explicit `message` tool
deliveries to the watched agent, or other hidden/non-user inputs delivered to the
watched agent. If such an input later causes the watched agent to produce a final
response, the response may be watch-notified, but the hidden input itself is not
forwarded. A completed `agent_start` result is watchable as the started child
agent's terminal final response to its direct delegating watcher, even when that
watcher is itself a side agent.

The `agent_start` tool result only confirms metadata such as `self_agent_id` and
`sub_agent_id`; response text arrives through watch notifications. Watches are
session-local runtime state: they are dropped on final session shutdown. Disable
watching explicitly when later
responses in the same session are no longer wanted.

Disable watching with:

```text
agent_watch({"agent_id":"engineer_b","enable":false})
```

## Invalid recipients and arguments

A local recipient must be a live or pending `agent_id`. External
recipients must contain exactly one slash and a slash-free valid `agent_id` on
the right-hand side. Otherwise the tool fails and no `agent.message_sent` or
`agent.message_received` projection is emitted.

If the id was never known, the tool reports an unknown recipient:

```text
message({"recipient_id":"engineer_0","message":"hello"})
```

```text
unknown message recipient: `engineer_0`
```

A final provider response ends the agent's current turn; it does not unload the
durable endpoint. A message to that still-loaded agent is accepted and can
start another turn. If the id belonged to an agent that was explicitly stopped,
unloaded, or canceled before it could start, the tool reports a stopped
recipient:

```text
message({"recipient_id":"engineer_1","message":"hello"})
```

```text
stopped message recipient: `engineer_1`
```

A durable agent identity can cold-restore without a resumable pre-restart
delegation route. Messaging that known-but-unavailable identity fails
distinctly and asks the caller to create a replacement:

```text
restored message recipient cannot resume its pre-restart delegation: `engineer_2`; start a replacement
```

This classification requires a real restart/restored-agent cut; an ordinary
final response remains live, and a never-known ID remains unknown.

Tool arguments are schema-validated before dispatch. Unknown extra fields are rejected before any logical tool invocation is logged.
