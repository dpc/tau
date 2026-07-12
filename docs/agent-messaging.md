# Agent messaging tool

## External transport foundation

The v2 bridge protocol uses canonical message envelopes instead of
prefix-formatted user prompts. Slack uses this canonical path. Telegram and XMPP still use their legacy prompt
path until separate adapter migrations land; they do not yet receive these
guarantees. Canonical external content remains internally typed as untrusted even when its
account identity is verified and allowlisted. Provider adapters lower the typed
context item once to compact XML such as `<tau_message transport="slack"
message_id="…" sender="U123" origin="external" sender_allowlisted="true">…</tau_message>`; harness routing and UI
code never infer authority from rendered text.

Reply tools select an extension-private live destination with an opaque
canonical `reply_to` id. Replayed messages do not restore reply authority.
The foundation exposes capability registration, durable ingress ack, and
successful-send completion for bridge migration. The experimental Telegram
gateway still requires a separate end-to-end pending-delivery/ack journal
migration before offset advancement can claim durable Tau acceptance.

The harness-owned `message` tool lets an agent send an asynchronous short text note to the user or to another agent. Every successful send is recorded as an `agent.message_sent` sender projection; agent recipients also get a separate `agent.message_received` recipient projection with the same `message_id`. User-recipient messages always render fully; agent-to-agent UI display depends on `/set show-messages`. When shown fully, a message renders as:

```text
Message from <sender> to <recipient>:
<message>
```

`/set show-messages` modes are:

User-recipient messages are human-visible broadcasts: they always render fully in every attached UI's currently visible transcript, regardless of `/set show-messages`. Agent-to-agent message projections still obey `/set show-messages`:

- `none`: no UI indication or history of agent-to-agent messages
- `self-summary`: no UI indication for agent-to-agent messages
- `self-full`: no UI indication for agent-to-agent messages
- `all-summary`: one-line no-content indication for agent-to-agent messages
- `all-full`: full content of all messages

## Send to the user

Use the special recipient id `user`:

```text
message({"recipient_id":"user","message":"I found the root cause and am checking the fix now."})
```

On success the tool result is:

```text
Message sent
```

## Send to another agent

Start the other agent with `agent_start`. The child starts with fresh transcript context, but inheritable per-agent metadata such as shell cwd is copied from the parent. Its initial display name is the task title plus the parent id/name snapshot, like `<title>; child of <parent-agent-id> <parent-agent-name>`. The child prompt's `agent_id` template variable matches the returned `sub_agent_id`. The `agent_start` tool completes immediately with `self_agent_id` and `sub_agent_id` metadata, while the sub-agent's response text arrives through the `agent_watch` async response-notification path:

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

The UI may display, summarize, or hide agent-to-agent messages depending on `/set show-messages`. The recipient agent also receives a hidden internal prompt with the message body XML-escaped inside a `<message>` wrapper.

## Send to an agent in another active session

Use `&<session-id>` to route to exactly one agent through that session's
configured peer entrypoint:

```text
message({"recipient_id":"&01JZ...","message":"Please compare this with your session."})
```

Tau chooses one eligible entrypoint agent, preferring idle over running and
least-recently routed then agent id. A busy eligible agent is reused; bare
routing is never enumeration or broadcast. Success returns the resolved
canonical `session/agent` address and whether it was started.
If no eligible endpoint exists, the target may create only its separately
configured `auto_start_role`; otherwise routing fails. Busy endpoints are reused,
and concurrent live sends coalesce onto one newly created endpoint.

Use `&<session-id>/@<agent-id>` for a typed exact address. The existing
`<session-id>/<agent-id>` spelling remains accepted for compatibility and keeps
known-address behavior even when the target agent is not in an entrypoint group.

Use `<session-id>/<agent_id>` as `recipient_id` to address an agent owned by
another running harness daemon:

```text
message({"recipient_id":"01JZ.../engineer_b","message":"Please compare this with your session."})
```

If the session id is the current active session, the address is treated as a
local agent id. Otherwise the message tool performs runtime-daemon discovery and
a dedicated external-message RPC on a helper thread, so the harness event loop is
not blocked by socket lookup or target validation. On confirmed delivery, the
sender transcript records `agent.message_sent` with recipient
`external_agent { session_id, agent_id }`; the recipient transcript records
`agent.message_received` with `sender_session_id`. UI and prompt rendering show
external addresses as `session/agent`.

Peer delivery is best-effort at-least-once. During normal live operation Tau
reports success only after the exact receive projection commits. If the target
crashes or the connection is lost after that commit but before acknowledgement,
a retry can deliver a duplicate prompt; Tau does not provide distributed
exactly-once deduplication across sessions.
The same crash ambiguity can duplicate an auto-started agent, model work, or spend.
Before creation, each endpoint is limited to 32 queued peer inputs, 256 KiB of
queued peer body, 60 accepted inputs per rolling minute, and 64 KiB per message.

Roles that explicitly enable the `session_discovery` tool group can use
`session_list({query?, limit?})` to find live sessions whose target harness
advertises a configured peer entrypoint. The snapshot is bounded and racy and
contains only session id, project basename, and current-session status. It does
not enumerate remote agents. The independent `agent_discovery` group enables
`agent_list({query?, role?, group?, state?, limit?})`, which lists only redacted
loaded/pending agents in the caller's current session.

External delivery failures (no daemon, stale socket, ambiguous session, wrong
active target session, stopped/unknown recipient) fail the tool call and do not
record a successful sender-side projection.

Inbound peer text is authenticated agent content, not a harness instruction. It
is XML-escaped inside a distinct `tau_peer_message` prompt envelope carrying
harness-authored sender session and agent identity.

## Watch another agent's responses

Use `agent_watch` to enable or disable hidden async notifications for another
agent's final responses and received user prompts:

```text
agent_watch({"agent_id":"engineer_b","enable":true})
```

Successful enable first reports whether the watched agent is currently running
an outer **agent turn**. Later `Idle → Running` and `Running → Idle` transitions
arrive as separate “started a turn” and “stopped its turn” notifications.
Enabling requires the target agent to be live. An enable request for a stopped
or unknown target fails without creating any watch relation or notification
state; after reloading the same agent id, explicitly enable a fresh watch.
An agent turn begins with activating input and ends with the terminal response
or termination that returns control to the prompting user or agent. Each
provider invocation inside it is a **model round**; requested-tool execution and
result collection before another model round is a **tool round**. One agent turn
therefore remains running across all model rounds and intervening tool rounds. The durable
notification carries a watch-subscription id, an initial-snapshot marker, and a
harness-runtime-scoped watched-agent turn generation so consumers can correlate
and reject stale state. The initial snapshot is client-visible status only and
is not injected into the watching agent's model context; later genuine
transitions are injected as content-free internal notifications.

While provider work is retrying, the watcher receives a sanitized structured
status on the first retry and whenever its closed category or phase changes.
Repeated attempts in the same category update the current snapshot without
waking the model again. Enabling or re-enabling a watch returns that current
snapshot and emits an initial client-visible, non-model event; it never replays
the attempt history. A terminal provider status is delivered before the matching
turn-stop edge. Provider bodies, human status/error text, headers, account data,
secrets, and prompt content never cross this boundary; see
[Watched provider status](events.md#watched-provider-status) for the wire shape.
Unloading either the watcher or watched agent disables every watch involving
that endpoint and discards its current provider snapshot and notification
dedupe state. These relations do not return if the same agent is loaded again;
enable a new watch to create a fresh subscription.

The CLI presents lifecycle records as compact status lines such as
`Watching engineer_b · idle` and `engineer_b · turn started`. These statuses are
not agent-authored messages and remain visible independently of the
`show-messages` content setting.

`agent_start` automatically enables watching for the sub-agent it creates. A
watch response notification is delivered to the watching agent as a hidden
internal prompt that is distinct from an explicit `message` tool delivery:

```text
[tau-internal]: Watched agent engineer_b emitted a response

<response>
Task result text.
</response>
```

If the watched agent receives a direct user prompt while the watch is active,
watchers also receive a hidden context notification when that prompt becomes
part of the watched agent's active turn, before the watched agent's later
response notification for that turn:

```text
[tau-internal]: Watched agent engineer_b received a user prompt

<prompt>
User follow-up text.
</prompt>
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
session-local runtime state: they are dropped on session shutdown, including a
session switch such as `/session new`. Disable watching explicitly when later
responses in the same session are no longer wanted.

Disable watching with:

```text
agent_watch({"agent_id":"engineer_b","enable":false})
```

## Invalid recipients and arguments

A non-`user` local recipient must be a live or pending `agent_id`. External
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

If the id belonged to an agent that has already finished or was canceled before it could start, the tool reports a stopped recipient:

```text
message({"recipient_id":"engineer_1","message":"hello"})
```

```text
stopped message recipient: `engineer_1`
```

Tool arguments are schema-validated before dispatch. Unknown extra fields are rejected before any logical tool invocation is logged.
