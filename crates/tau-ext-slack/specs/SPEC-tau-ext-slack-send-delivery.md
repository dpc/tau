# SPEC-tau-ext-slack-send-delivery: Slack send delivery

## Record justification

Slack send delivery spans reader-side reservation, HTTP worker scheduling, lifecycle and replay ledgers, serialized report output, and downstream canonical facts, so no one implementation area can own the complete delivery contract.

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
workers, and requests shutdown. After both writes, the ledger remains pending and
installs no posted-message or reaction authority. Only the originating configured
publisher's matching event type, target agent, and stable message ID on the
canonical `message.sent` live downpath completes the ledger and installs that
authority after current lifecycle validation.

Same-call replay coalesces while canonical confirmation is pending and returns
the stable result after confirmation; it causes no I/O or second report.
Conflicting call-ID reuse fails. A new ID is new intent. The ledger is non-evicting with capacity
1,024 and at most 64 active sends; both clear with session/process lifetime.
Ambiguous outcomes may yield zero, one, or two remote copies. Definitive auth,
permission, target, and request rejection are not retried. Errors and
`Retry-After` are bounded and expose no body, header, token, native ID, mention,
or text.

Every Slack-owned sole tool terminal uses checked ordered output. Asynchronous
definitive failure, retry exhaustion, and lifecycle cancellation submit typed
`tool.error_reported` observations, and the send ledger retains ownership until
that write succeeds. Mandatory output failure terminates the extension session
so harness disconnect cleanup settles any routed calls; optional progress and
notices remain best effort.

Submitted sent reports and downstream canonical facts follow
[SPEC-external-message-reports-and-facts](../../../specs/SPEC-external-message-reports-and-facts.md).
