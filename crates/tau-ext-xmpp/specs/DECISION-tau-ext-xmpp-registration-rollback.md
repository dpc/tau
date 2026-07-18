# DECISION-tau-ext-xmpp-registration-rollback: Registration timeout rollback

Authority: unconfirmed

Registration commands have an overall worker-side timeout shorter than the outer
tool-call wait. If registration times out after a successful MUC join, or if the caller
has already timed out and dropped the response receiver anyway, the worker rolls back
conversation maps and sends unavailable presence so a failed registration cannot leave
ghost XMPP routing state or a stale room occupant. Shutdown is a worker-wide
cancellation source: in-flight readiness, join, rejoin, reconnect, stanza-send, and
best-effort notice work must be interrupted or bounded so unavailable presence cleanup
gets the remaining shutdown budget. The private shutdown signal combines a synchronous
requested flag with async notification wakeups so worker futures can be cancelled
promptly without polling. Best-effort invite/fallback notices are sent only after the
success response and are cancelled by shutdown so unavailable presence cleanup is
prioritized under the bounded shutdown budget.
