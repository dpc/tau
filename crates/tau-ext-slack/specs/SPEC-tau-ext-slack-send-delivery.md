# SPEC-tau-ext-slack-send-delivery: Slack send delivery

A send freezes agent, arguments, route, lifecycle/config generations,
installation, logical text, and serialized body. It reserves `ToolCallId` before
I/O; HTTP and waits run off the protocol reader. There is one initial attempt and
at most one byte-identical retry. Retry starts within 60 seconds of intent
preparation, respects strictly bounded `Retry-After`, and revalidates authority
before each attempt.
Per-channel attempts are FIFO while unrelated channels proceed independently.
A started synchronous HTTP request may outlive retirement only to its request
bound and may not retry or restore authority.

After remote success, the serialized writer flushes `message.sent_reported` and
then transient `tool.result_reported`; the harness derives canonical facts
downstream. This is not a remote/durable transaction or harness commit ACK.
Confirmed writer failure latches output failure, retires Slack authority, wakes
workers, and requests shutdown. Local success installs state only after both
writes and current lifecycle validation.

Same-call replay is stable and causes no I/O or second report; conflicting call-ID
reuse fails. A new ID is new intent. The ledger is non-evicting with capacity
1,024 and at most 64 active sends; both clear with session/process lifetime.
Ambiguous outcomes may yield zero, one, or two remote copies. Definitive auth,
permission, target, and request rejection are not retried. Errors and
`Retry-After` are bounded and expose no body, header, token, native ID, mention,
or text.

The governing choice is
[DECISION-tau-ext-slack-send-delivery](DECISION-tau-ext-slack-send-delivery.md).
Submitted sent reports and downstream canonical facts follow
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).
