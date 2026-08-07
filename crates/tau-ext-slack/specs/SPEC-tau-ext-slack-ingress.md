# SPEC-tau-ext-slack-ingress: Slack ingress identity and admission

## Record justification

Slack ingress spans Socket Mode lifecycle, identity lookup and admission, occurrence deduplication, route triggering, report submission, and mention projection, so no one implementation area can own the complete trust and delivery contract.

Sender admission, route trigger, and content trust are independent. Strict mode
admits only allowlisted verified humans; lax adds verified humans only on static
receive routes. Payload remains untrusted. Dynamic links still require exact
allowlisting. Each occurrence uses live `users.info` with no positive cache.
Wrapper team/bot identity must exact-match installed `auth.test` authority or one
unambiguous authorization; top-level event/actor team fields are not authority.
Missing, malformed, mixed, ambiguous, or conflicting evidence fails before
identity lookup, local effects, or report submission.

Verified U/W identity is authoritative. `users.info` response ID must exactly
equal the requested U/W ID, and the user must be non-deleted, non-bot, and
non-app. Display is nonempty and at most 80 Unicode
scalars/256 UTF-8 bytes. Config aliases are one-to-one, at most 64 entries, and
use exact U/W ID keys with values matching `[a-z][a-z0-9_-]{0,63}`. Displays
and aliases are presentation only.
Display text must also be free of unsafe control or format structure. A
configured alias takes display precedence over the bounded Slack display.
Published stable identity is opaque and installation-scoped; native IDs remain
private. Reconnect identity mismatch retires installation authority until
restart. Replay uses stored universal fields only, performs no Slack lookup or
alias re-resolution, and reconstructs no actionable route. Occurrence FIFO
capacity is 4,096. A repeated occurrence replays its exact retained report while
canonical confirmation is pending, then ordinary duplicate suppression drops it.
Recording still precedes identity lookup, local effects, and report construction,
so a failure before pending report installation suppresses retry until cache
eviction or process restart.
Operational logs and categorical failures omit actor IDs, displays, aliases,
reaction names, message text, and installation identifiers.

Admission uses a 64-item successful-ACK FIFO with generation checks. Shutdown or
config change invalidates late work; reconnect preserves queued authority.
Report-bearing occurrences retain their pre-ACK slot until an exact canonical
event type, target agent, configured publisher, message identity, and stable
Slack report ID return on the live post-commit downpath. Missing echoes retain
the slot, so saturation stops ACK and lets Slack retry after reconnect. Socket
Mode ACK remains a separate transport result and proves no Tau commit.
Mention-only/all-message triggers remain exact, and non-DM commands require a
leading authenticated bot mention. Only canonical confirmation installs a source
reply selector and never proactive authority.

One process-lifetime Socket Mode worker owns at most one current WebSocket.
After connection it sends Ping every 10 seconds, with the first Ping delayed by
one full interval. One independent deadline starts at connection and resets only
when any Pong is received; expiry 40 seconds after the latest Pong returns a
reconnectable closed-category failure. Text, Binary, Ping, and other non-Pong
frames do not refresh liveness. Ping, Pong, and envelope-ACK writes remain
preemptible by shutdown and the Pong deadline. Either boundary drops the socket
without another write. Every connection return marks the worker offline before
the outer loop authenticates the installation, obtains a fresh one-use Socket
Mode URL, and reconnects.

The worker thread starts at most once per extension process. Successful `hello`
marks only its current connection online. The first startup or reconnect failure
emits one bounded, identifier-free warning; a process-lifetime latch suppresses
later duplicates. Restart clears the registration, worker, online state, and
notice latch.

Bot mentions classify only exact case-sensitive `<@U…>`/`<@W…>` for the
installed bot outside complete equal-length backtick spans. Escaped, labeled,
partial, malformed, differently cased, lookalike, other, and literal orientation
tokens do not classify. Routing/commands remove exactly one eligible leading
mention; remaining eligible mentions normalize to `@slack_bridge` in create/edit
text. Commands submit no report; reactions/deletes carry no text. No native
mention field or opaque data persists; replay sees only normalized text.
Registration JSON never exposes bot/workspace IDs. Egress rejects raw native
controls; authored `@slack_bridge` stays literal, and the safe source-mention
path is the sole generator. Successful registration returns exactly
`{"status":"registered","incoming_transport_reference":"@slack_bridge"}`;
unregister returns exactly `{"status":"unregistered"}`.
