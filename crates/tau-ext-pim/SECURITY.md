# tau-ext-pim security boundaries

Calendar and email provider responses are untrusted external ingress. Runtime
code must bound response bodies, parsing work, pagination requests, accumulated
rows, and model-visible fields even though configured local extensions and the
harness remain cooperative boundaries described by the repository
[`SECURITY.md`](../../SECURITY.md).


## Calendar semantic pagination

One calendar search or free/busy page may issue at most 100 sequential provider
page requests and consume at most 10,000 provider rows while filling filtered
results. Exceeding either bound returns a visible tool error; it must not be
reported as provider exhaustion or as a successful short page.

The runtime tracks every provider cursor observed during one semantic page.
Any repeated cursor, including a multi-token cycle, fails visibly. A successful
continuation cursor always identifies the position after every provider row
consumed to build the returned semantic page. This exact position prevents
excluded rows from being replayed or later matching rows from being skipped.

Regression coverage must include advancing empty pages, multi-token cursor
cycles, provider row/request budget exhaustion, filtered rows before later
matches, and exact post-consumption continuation state. Recheck these cases
whenever provider pagination, lifecycle filters, cursor encoding, or page-size
selection changes.


## Google Calendar read diagnostics

Google Calendar list/read failures and RSVP's preparatory GET retain at most the
first 4 KiB of a non-success response body. Before control and whitespace
cleanup, the backend uses credential-length bounded lookahead to exact-redact
the active access token and configured custom API base, including matches that
cross the retained-prefix cut. The resulting bounded diagnostic may reach tool
errors and the Calendar audit log; success responses and post-dispatch write
classification remain unchanged.

Regression coverage must exercise the production list/read and RSVP paths,
model-visible and audit sinks, repeated and overlapping credentials, and each
credential crossing the 4 KiB cut. Revisit this boundary whenever Calendar
request credentials, API-base selection, non-success body reads, diagnostic
bounds/sanitization, or error sinks change.


## Google Calendar mutation outcomes

Entering ureq's `send` or `call` method for the side-effecting
POST/PATCH/DELETE request is the Google Calendar mutation dispatch cut. Local
validation, account/calendar lookup, OAuth acquisition, request-body
construction and serialization, and RSVP's preparatory GET occur before that
cut. A failure there is `NotDispatched`; an approved claim can safely return
from `sending` to `pending`.

Once dispatch begins, only a complete trusted success result completes the
claim as `approved`. Transport failure, every non-success HTTP response, body
read failure, a successful response above the unchanged 1 MiB cap, malformed
success JSON, or a success result missing required fields is `OutcomeUnknown`.
The durable claim remains `sending` across restart, and approving that same ID
again performs no provider request. The fixed user diagnostic contains no
provider or transport text: the change may have applied, must not be retried,
and needs manual provider reconciliation.
OutcomeUnknown tool envelopes and audit statuses use the existing
`network_error` code.

Direct writes with approval disabled return the same diagnostic but have no
durable operation identity or deduplication. A new direct invocation can repeat
an unknown mutation. This accepted residual must not be described as
exactly-once behavior.

The deterministic loopback safeguards in
`src/calendar/runtime/tests/google_write_outcomes.rs` exercise the production
runtime and Google backend across the dispatch cut. They cover create, update,
delete, and RSVP; disconnect, non-success status, read failure,
malformed/oversized success, complete success, restart, same-ID refusal,
direct-write residual state, and bounded sanitized diagnostics. Revisit this
boundary whenever the HTTP client, request construction, success parsing/body
cap, approval states, error-code mapping, direct-write policy, or Google
mutation APIs change.


## SMTP submission outcomes

SMTP connection, TLS, authentication, message construction, and other proven
pre-submission failures are `NotDispatched`. Complete negative SMTP replies also
prove rejection. Once Tau enters message submission, a timeout, disconnect,
malformed response, or other failure without a complete negative reply is
`OutcomeUnknown`. Direct sends return the fixed `smtp_outcome_unknown`
diagnostic: the server may have accepted the email, automatic retry is unsafe,
and the account or provider needs reconciliation. Provider detail, message body,
and Bcc recipients are absent from that terminal.

Tau never retries SMTP message submission internally. OAuth may retry only
authentication before submission. Approved drafts retain `sending` after
either failure class and refuse later redispatch; direct allowlisted sends have
no durable identity, so a separate invocation can still repeat an unknown
outcome.

Deterministic scripted SMTP tests in `src/email/real_backend/tests.rs` exercise
the production connection, authentication, and submission path with explicit
replies and EOF. They cover pre-DATA rejection, password authentication
rejection, complete acceptance, complete post-DATA rejection, accepted DATA
followed by lost final reply, and exactly one observed DATA block. Revisit this
boundary whenever SMTP connection/authentication flow, lettre error
classification, timeouts, approval lifecycle, direct-send policy, or error-code
mapping changes.
