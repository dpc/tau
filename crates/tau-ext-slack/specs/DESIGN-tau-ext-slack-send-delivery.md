# DESIGN-tau-ext-slack-send-delivery: Bounded cancellable Slack send delivery

Status: confirmed, 2026-07-15, dpc; harness-completion portions superseded
2026-07-17 by
[DESIGN-extension-published-message-facts](../../../specs/DESIGN-extension-published-message-facts.md)

`slack_send` uses process/session-scoped at-least-once delivery. After validating
one exact tool intent, the extension freezes its agent, arguments, source reply
or configured destination, lifecycle/configuration generations, authenticated
installation, final logical text, and exact serialized `chat.postMessage` body.
It reserves the `ToolCallId` before I/O and runs HTTP plus retry waiting outside
tau-client's serialized reader.

The delivery budget is one initial attempt plus at most one byte-identical retry.
Retry-After is bounded, per-channel logical calls are FIFO, and unrelated
channels remain independent. The retry must begin within 60 seconds of intent
preparation. Notification-driven waits revalidate current route, session,
configuration, installation, and tool/agent lifecycle before provider I/O.

Already-started synchronous HTTP cannot be recalled and may leave a remote effect
after authority retirement. A retired worker cannot retry or restore local reply,
post, or reaction state.

## Fact and result writes

After Slack success, the extension constructs the sent `MessageFactId` as
`slack-message:<opaque-digest>` from the private channel/message coordinates.
Under one serialized local gate it writes and
flushes `message.sent`, then writes and flushes the ordinary `tool.result`.
This preserves extension-frame order but is not a harness commit acknowledgement
and is not a transaction with the remote effect or durable storage. Any confirmed writer failure latches output failure, retires the entire Slack
session and all receive/send/reaction authority, wakes workers, and requests
shutdown.

Only after both local writes succeed may current lifecycle authority install
posted-message/reaction state and mark the call completed. Same-call replay
returns the retained stable result without Slack I/O or another fact. Conflicting
agent/argument reuse fails; a new call ID is new intent.

The process/session ledger has 1,024 non-evicting entries and at most 64 active
workers. It retains prepared, active, retry-scheduled, completed, definitive,
ambiguous/exhausted, and cancelled dispositions. Session/process retirement
clears the ledger. There is no pending harness-completion queue, acceptance RPC,
durable outbox, restart idempotency, `client_msg_id`, or remote reconciliation.

## Ambiguity and privacy

Timeout, transport interruption, service failure, or malformed post-I/O response
may mean Slack accepted the request. If an ambiguous initial attempt is retried,
one or two copies may exist; two ambiguous attempts may leave zero, one, or two.
Definitive authentication, permission, target, or request rejection is not
retried.

Agent-authored mrkdwn rejects raw `<@`, `<!`, and `<#` controls. HTTP and post
failures cross the boundary only as closed typed categories and bounded
Retry-After. Raw bodies, Slack errors, headers, tokens, native IDs, mentions, and
message text never enter tool errors, notices, or ordinary logs.
