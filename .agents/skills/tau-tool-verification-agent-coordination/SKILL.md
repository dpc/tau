---
name: tau-tool-verification-agent-coordination
description: >
  Use this skill when verifying Tau agent_start, message, or agent_watch coordination, including routing, validation, interruption, notification formatting, watch lifecycle, and deduplication.
advertise: false
---

# Tau Tool Verification Agent Coordination

Load `tau-tool-verification` first for the shared output structure, escaping,
line handling, tool-description, availability, and reporting guidelines.
This skill supplies the focused verification guidance for this tool group.

### Message tool verification plan

Use this plan when asked to verify the `message` tool, especially in multi-agent scenarios. The goal is to prove that messages are routed correctly among the main agent, sub-agents, sibling sub-agents, sessions, and completed or invalid recipients. Also verify timing, sender IDs, async delivery, payload escaping in hidden prompts, exact payload preservation in durable `AgentMessage` events, and error behavior.

Do not rely on memory. Give every sub-agent a self-contained prompt. A delegated agent starts with a clean context and does not know this skill, the parent conversation, or the IDs of other agents unless you include them in its prompt or later messages.

#### What to verify

Record all of these observations:

* Main agent to sub-agent delivery.
* Multiple messages to the same live sub-agent.
* Sub-agent to sibling sub-agent delivery.
* Sub-agent to the main agent using the main agent recipient ID.
* Exact `user` recipient rejection without sender or recipient projection.
* Main agent to itself, after the main agent recipient ID is known.
* Delivery while a sub-agent is sleeping, backgrounded on a long tool, queued behind another tool call, or otherwise between model turns.
* Delivery order, or any reorderings, especially for parallel `message` calls.
* Sender IDs visible to recipients.
* Message payload preservation in durable events, and exact-close framing in hidden prompts, for multiline content, blank lines, unicode, JSON-like text, backticks, and literal `<message>` tags inside the payload.
* Error for an unknown recipient ID.
* Error for a completed sub-agent recipient ID.
* Error for an empty message.
* `agent_start`, `agent_watch`, and `wait` behavior around long-running sub-agents; in current Tau, sub-agent responses arrive by watch notifications rather than slow `agent_start` results.

#### Phase 1: spawn two peer agents and report to the parent

Start with two shared delegates. Name them Agent A and Agent B. Their initial
prompts wait for a bootstrap message because the parent `self_agent_id` is
returned only after `agent_start` submits those prompts.

Use this prompt for Agent A, replacing only the agent name where needed for Agent B:

```text
You are Agent A in a Tau message-tool verification test. Goal: verify
cross-agent messaging behavior. You have a clean context; follow these
instructions exactly.

Important:
- Incoming messages from the Tau `message` tool may appear as hidden prompts in your conversation. Treat every new prompt/message you see after starting as an inbound test message.
- Keep a full log of every inbound message you receive after this initial task prompt. Include exact text, apparent sender/recipient if visible, and when you noticed it.
- You may use only safe commands. Use short `sleep` commands only to stay alive and give the parent/peer time to send messages.
- If you receive a message containing `COMMAND: SEND_PEER`, parse `recipient_id={id}` and `text={text}`, then call the `message` tool to send exactly `{text}` to that recipient. Log the tool result.
- Your first inbound message must be `BOOTSTRAP parent_id={main_agent_id}`.
  Save that ID and use it as `{main_agent_id}` below.
- If you receive a message containing `COMMAND: REPORT`, send a `message` to `{main_agent_id}` with your current full log.
- Do not finish early. Run four observation rounds.

Procedure:
1. Wait for the `BOOTSTRAP` message, then send a message to `{main_agent_id}`
   with exactly: `READY Agent A: started message-tool test`.
2. For rounds 1 through 4:
   a. Run `sleep 3` using the shell tool.
   b. After the sleep result, inspect any new inbound messages/prompts you have received.
   c. Execute any `COMMAND: SEND_PEER` instructions you have newly received.
   d. Send a message to `{main_agent_id}` starting with `REPORT Agent A round {n}:` and include all newly observed inbound messages since the previous report and any message-tool actions/results. If none, say `none`.
3. Final answer: return `FINAL Agent A` plus your complete inbound-message log and all message-tool actions/results.

You are expected to receive messages from the parent and possibly from Agent B. Be precise and do not invent messages.
```

After the `agent_start` results return, note the caller `self_agent_id` and each
`sub_agent_id`. In legacy/background `agent_start` sessions, also note any
returned agent_start tool-call ids. Send
`BOOTSTRAP parent_id={self_agent_id}` to each `sub_agent_id`, wait for both
`READY` reports, then send the first batch of messages in parallel:

```text
To Agent A:
- MAIN to A direct message 1. nonce=main-a-001. Please log exact text.
- MAIN to A direct message 2. nonce=main-a-002. Please log exact text.
- COMMAND: SEND_PEER recipient_id={agent_b_id} text=PEER A to B message from Agent A. nonce=peer-a-b-001. Please log exact text.
- COMMAND: REPORT from main to Agent A after initial sends. nonce=report-a-001.

To Agent B:
- MAIN to B direct message 1. nonce=main-b-001. Please log exact text.
- MAIN to B direct message 2. nonce=main-b-002. Please log exact text.
- COMMAND: SEND_PEER recipient_id={agent_a_id} text=PEER B to A message from Agent B. nonce=peer-b-a-001. Please log exact text.
- COMMAND: REPORT from main to Agent B after initial sends. nonce=report-b-001.
```

Sleep for about four seconds in the main agent, then send a delayed batch in parallel:

```text
To Agent A:
- MAIN to A delayed direct message 3. nonce=main-a-003. Please log exact text.
- COMMAND: SEND_PEER recipient_id={agent_b_id} text=PEER A to B delayed message from Agent A. nonce=peer-a-b-002. Please log exact text.
- COMMAND: REPORT from main to Agent A after delayed sends. nonce=report-a-002.

To Agent B:
- MAIN to B delayed direct message 3. nonce=main-b-003. Please log exact text.
- COMMAND: SEND_PEER recipient_id={agent_a_id} text=PEER B to A delayed message from Agent B. nonce=peer-b-a-002. Please log exact text.
- COMMAND: REPORT from main to Agent B after delayed sends. nonce=report-b-002.
```

Also send one message to a clearly invalid recipient such as `engineer_does_not_exist_message_validation`; expect a tool error with the unknown recipient ID and echoed message fields.
Send one message to exact recipient `user`; expect an unsupported-recipient
tool error and, when event logs are available, confirm that it creates neither
a sender nor recipient projection.

Wait for both delegates. In their final logs, verify that:

* The agent_start result exposes `self_agent_id` and `sub_agent_id` without redundant aliases.
* Each agent saw the direct main-agent messages addressed to it.
* Each agent saw the peer message from the other agent.
* Each `COMMAND: SEND_PEER` caused exactly one peer `message` call with result beginning `Message committed: msg-` and ending `response not guaranteed`.
* Delayed messages arrived even though the agents were already running.
* The visible sender ID for messages from the main agent is present and matches the `self_agent_id` from the agent_start result. Save that sender ID; it is the main agent recipient ID for the next phase.

After both delegates complete, try to send a post-completion message to each old `sub_agent_id`. Expect an error until completed-agent wakeup is implemented. Current behavior may report this the same way as an unknown recipient.

#### Phase 2: verify delivery to a delegate queued behind a backgrounded tool

Start one shared sub-agent with `agent_start` whose job is to create the message-delivery edge case where the sub-agent has a long backgrounded tool still actually running and a second not-yet-started tool queued behind it.

Use this prompt:

```text
You are a Tau message-tool verification sub-agent for queued-tool preemption. Goal: prove parent messages are delivered even when your next tool call would otherwise be queued behind a long backgrounded tool.

Procedure:
1. Start a long shell command: `sleep 30`.
2. After Tau returns the background placeholder for that shell call, immediately request another shell command: `sleep 5`.
3. If you receive any inbound `[tau-internal]` message from the parent, respond to `{main_agent_id}` exactly: `QUEUED-TOOL MESSAGE RECEIVED nonce=queued-tool-message-001` plus the exact inbound message text and visible sender id.
4. After sending the parent message, final answer exactly: `FINAL queued-tool message probe reported to parent`.
5. If no inbound message arrives, final answer exactly: `UNEXPECTED queued-tool message missing`.

Do not invent messages. Do not finish before checking for the parent message.
```

After the `agent_start` result returns, wait until the delegate has had enough time for the first `sleep 30` to background and for the second `sleep 5` request to be queued. In normal UI output this often looks like delegate progress with a running/backgrounded shell call and no response from the second shell yet.

Send a message to the delegate `sub_agent_id`:

```text
Parent queued-tool delivery probe. nonce=queued-tool-message-001. Reply via message to `{main_agent_id}` when received.
```

Expected behavior:

* The message call returns `Message committed: <message-id>; recipient was live; response not guaranteed`.
* The delegate responds to the parent with `QUEUED-TOOL MESSAGE RECEIVED nonce=queued-tool-message-001` instead of remaining stuck behind the queued `sleep 5`.
* If event logs are available, verify that the `AgentMessage` was recorded, the not-yet-started queued tool call was terminalized with `ToolCancelled`, and a new `AgentPromptCreated` was emitted for the delegate message prompt.
* The long backgrounded `sleep 30` may still complete later in the delegate conversation. Its completion should not be delivered to the parent conversation or block the message response.

This scenario specifically protects the code path where `agent.message` delivery preempts queued-but-not-started tool calls behind an already-backgrounded exclusive tool. Without that behavior, the message can be received by the harness but never become a model-visible prompt for the sub-agent.

#### Phase 3: verify sub-agent to main-agent routing

Use the main agent recipient ID returned by `agent_start` in Phase 1. Spawn two fresh shared sub-agents with `agent_start`, Agent C and Agent D. These agents should report back to the main agent recipient ID, not to `user`. This proves that parent-directed messages are delivered as model-visible `[tau-internal]` inbound messages to the main agent.

Use this prompt for Agent C, replacing only the agent name where needed for Agent D and filling `{main_agent_id}` with the ID returned in Phase 1:

```text
You are Agent C in a second Tau message-tool verification test. Parent/main agent recipient_id is `{main_agent_id}`. Goal: verify messages among parent, Agent C, and Agent D.

Rules:
- Incoming `message` tool messages may appear as hidden prompts. Log every inbound message you receive after this initial task prompt, with exact text and visible sender id.
- For every report, use the `message` tool to send to `recipient_id={main_agent_id}` (the parent/main agent), not `user`, unless the parent message fails. If it fails, log the failure and continue.
- If an inbound message contains `COMMAND: SEND_PEER recipient_id={id} text={text}`, send exactly `{text}` to `{id}` using the `message` tool and log the result.
- If an inbound message contains `COMMAND: REPORT_PARENT`, immediately message your current log to `{main_agent_id}`.
- Stay alive for three observation rounds using `sleep 2` each round. Do not finish early.

Procedure:
1. Send to `{main_agent_id}`: `READY Agent C to parent. nonce=ready-c-parent-001`.
2. Repeat three rounds: sleep 2 seconds; inspect new inbound messages; execute any SEND_PEER commands; message the parent with `REPORT Agent C round {n}:` plus new inbound messages and actions since previous report, or `none`.
3. Final answer: `FINAL Agent C` plus complete inbound log and all message-tool actions/results.
```

After the `agent_start` placeholders return, send this batch in parallel:

```text
To Agent C:
- MAIN to C direct message. nonce=main-c-001. Please log exact text and sender id.
- COMMAND: SEND_PEER recipient_id={agent_d_id} text=PEER C to D from Agent C. nonce=peer-c-d-001. Please log exact text.
- COMMAND: REPORT_PARENT nonce=report-c-parent-001.

To Agent D:
- MAIN to D direct message. nonce=main-d-001. Please log exact text and sender id.
- COMMAND: SEND_PEER recipient_id={agent_c_id} text=PEER D to C from Agent D. nonce=peer-d-c-001. Please log exact text.
- COMMAND: REPORT_PARENT nonce=report-d-parent-001.
```

The main agent should receive `[tau-internal]` inbound messages from each sub-agent. Record whether the sender ID in those inbound messages matches the sub-agent `sub_agent_id`. Sleep for about three seconds, then send one delayed direct message to each agent:

```text
To Agent C:
- MAIN to C delayed message. nonce=main-c-002. Please log exact text and sender id.

To Agent D:
- MAIN to D delayed message. nonce=main-d-002. Please log exact text and sender id.
```

Wait for both delegates. Verify that their final logs match the parent-visible reports already received by the main agent.

After both complete, again send post-completion messages to both old `sub_agent_id` values and expect errors until completed-agent wakeup is implemented.

#### Phase 4: verify self, content, and simple validation errors

After the main agent recipient ID is known, send a message from the main agent to itself. Expect a model-visible `[tau-internal]` inbound message whose sender is the same main agent ID and whose payload is exact.

Then send a multiline self-message like this:

```text
MULTILINE self content probe. nonce=self-main-002
line 2 unicode: café 🚀

line 4 xml-ish: <message>inner</message> & chars
line 5 code-ish: `backticks` and {"json":true}
```

Verify that blank lines, unicode, ampersands, backticks, JSON-like text, and
literal inner `<message>` openings remain exact and readable. Exact `</message>`
collisions must appear as `&lt;/message&gt;`, and the delivered wrapper must contain
exactly one exact close. If you inspect durable `AgentMessage` events, verify that
the stored payload is still exact and unframed.

Finally, call `message` with an empty string to a valid recipient. Expect a tool error such as `` `message` must not be empty ``. Also verify an unknown recipient error if it was not already checked in Phase 1.

#### Reporting format for `message` verification

Report concise but complete findings:

* List each tested route and whether it passed: main to child, child to child,
  child to parent, rejected exact `user`, main to self, invalid recipient,
  completed recipient, empty payload, rich content payload.
* Include exact unexpected errors or output.
* Mention any timing surprises, missed messages, duplicate messages, or ordering uncertainty.
* Confirm the `message` success output includes a stable message ID and `response not guaranteed`; it confirms async acceptance, not delivery completion.
* Include whether errors distinguish completed recipients from unknown recipients. Current behavior may use the same unknown-recipient error for both.
* Include whether parent recipient ID discovery was clear from `self_agent_id` or still had to be inferred from sub-agent logs.
* Include whether the delivered wrapper preserved literal `<message>` openings
  and ampersands while replacing every exact `</message>` collision.


### Agent watch tool verification plan

Use this plan when asked to verify the `agent_watch` tool. The goal is to prove that watch subscriptions deliver only the watched agent's final response notifications and received user-prompt notifications, that `agent_start` auto-watches its child, and that disabling a watch stops delivery without hiding errors.

Enabling a watch also delivers one content-free current model-turn state
notification, followed by separate started/stopped notifications for whole
turns. Tool execution and its provider continuation remain one turn. Verify
that these state notices do not expose prompt, message, tool, response, or error
content, and that notification-only turns do not recursively amplify cyclic
watches.

Watch notifications must be distinguishable from explicit `message` tool deliveries in the model-visible prompt. Explicit messages use a “received a message from ...” wrapper with a `<message>` block. Watch response notifications use this exact shape:

```text
[tau-internal]: Watched agent engineer-aSSq emitted a response

<response>
The task is complete.
</response>
```

Watch prompt notifications use this exact shape:

```text
[tau-internal]: Watched agent engineer-aSSq received a user prompt

<prompt>
Please continue.
</prompt>
```

`agent_watch` must not forward tool-completion notices, background-tool wakeups, ordinary internal steering prompts, prompts delivered through the `message` tool, or any other internal prompt delivered to the watched agent. A watched agent may later emit a final response after processing such an input; the final response is the watchable event, not the internal input itself. A completed `agent_start` result is also a watchable final response of that started agent to the delegating watcher, even if the delegating watcher is itself a side agent.

If the watching agent repeats the same notification text in a commentary/final response, that is the agent echoing the notification, not a second watch delivery. When checking for duplicate delivery, count actual message events/results, not streamed echoes of text the model chose to repeat.

A watch notification that arrives while the watcher is blocked in `wait` may interrupt the wait with a `tau_internal: true` result saying new input is queued. Treat that as expected: it is the same active-wait interruption behavior used for ordinary agent messages.

Calls with exact recipient `user` must fail as unsupported and create no message projection. Use the parent agent ID or a watched response/final answer when proving that the parent observed a notification.

#### What to verify

Record all of these observations:

* `agent_start` automatically watches the returned `sub_agent_id` for the starting agent.
* Watch response notifications are model-visible as “Watched agent <id> emitted a response” with a `<response>` block, not as “received a message” with a `<message>` block.
* Watch prompt notifications are model-visible as “Watched agent <id> received a user prompt” with a `<prompt>` block.
* The watched agent's final responses arrive as async response notifications from the sub-agent to the watcher until disabled.
* A completed `agent_start` result from a watched child is reported as the child agent's final response to the watcher.
* Tool-completion notices and other internal/steering prompts delivered to the watched agent are not forwarded as watch prompt notifications.
* The `agent_start` final tool result contains metadata such as `self_agent_id` and `sub_agent_id`, but does not duplicate the sub-agent response text as `output`.
* `agent_watch({"agent_id": id, "enable": false})` disables notifications for that watcher.
* Re-enabling with `enable: true` restores notifications for later responses.
* Unknown, empty, or self `agent_id` values fail clearly. Stopped but known agents can still be watched or unwatched.
* Mid-turn tool-call responses do not notify early; notifications should correspond to final response semantics.
* Prompts delivered through the `message` tool do not produce watch-prompt notifications; they remain ordinary explicit-message deliveries to the watched agent.
* If a watched sub-agent errors, the starter should still receive a useful watch/error notification. In legacy/background `agent_start` sessions where the agent_start call itself is cancellable, cancellation should still report a useful error.

#### Suggested procedure

1. Start a sub-agent with `agent_start` whose prompt final-answers a nonce, for example `WATCH auto final nonce=watch-auto-001`. Do not ask the sub-agent to also message the parent with the same nonce in this phase.
2. Confirm the starter receives an async `[tau-internal]` response notification from the `sub_agent_id` containing that nonce, using a “Watched agent <id> emitted a response” wrapper and a `<response>` block. It must not use the explicit-message “received a message from ...” wrapper or a `<message>` block. Avoid echoing the full notification text in commentary; summarize it when reporting.
3. Confirm the `agent_start` result exposes `self_agent_id` and `sub_agent_id` without returning the nonce as duplicated tool output.
4. Start a long-lived sub-agent. Disable watching it with `agent_watch({"agent_id":"<sub_agent_id>","enable":false})`, then cause or wait for a later response. Confirm no watch message is delivered to the starter for that response.
5. Re-enable the watch and cause another response. Confirm a watch message is delivered again.
6. Exercise validation: watch self, watch an empty id, watch an unknown id, and watch a stopped completed sub-agent. Unknown, empty, and self ids should error; the stopped but known sub-agent should be accepted. Record exact tool results or errors.
7. Deliver a real user prompt to a watched agent, or inspect event logs from a test that does so, and confirm the watcher receives exactly the “Watched agent <id> received a user prompt” wrapper with a `<prompt>` block.
8. Deliver an internal prompt to a watched agent if you can do so safely, or inspect event logs around a watched agent's background tool completion. Confirm no watch prompt notification is delivered for `[tau-internal] Tool call ... completed. Its result is queued; use wait to consume it.` or similar internal/steering text. If the watched agent later responds after processing that internal prompt, record that later response as a separate final-response notification.
9. In current watch-based sessions, verify an erroring watched sub-agent reports a useful watch/error notification. If legacy/background `agent_start` cancellation is available, cancel a watched long-running `agent_start` and confirm cancellation still reports a useful error; if watch delivery reached the starter, generic watch-delivered wording is acceptable, otherwise the original `Tool call canceled` must be preserved.

#### Reporting format for `agent_watch` verification

Report concise but complete findings:

* List each tested route and whether it passed: auto-watch, final response wrapper, user-prompt wrapper, internal/tool-completion non-forwarding, disable, re-enable, validation errors, cancellation/error fallback, and no duplicated `agent_start` output.
* Include exact notification text, tool results, and unexpected errors. Call out any watch notification that is formatted like an explicit `message` tool delivery.
* Mention duplicate notifications, missed notifications, premature mid-turn notifications, duplicate UI/status rows for the same watched agent, or unclear sender/recipient IDs. If a sub-agent was instructed to both message the parent and final-answer with the same text, record those as two expected delivery paths rather than an `agent_watch` duplicate. If the watching agent repeats a received `[tau-internal]` notification in its own commentary/final response, record that as model echo unless event logs show multiple received deliveries. If a watched child produces a later response after an unfinished background tool completes, record it as a later child turn unless the same response event was delivered more than once.
* Include whether `wait` was interrupted by a watch notification while waiting; this is expected if it reports that new input is queued.
* Include whether `self_agent_id` and `sub_agent_id` made the watcher and watched IDs clear enough.
