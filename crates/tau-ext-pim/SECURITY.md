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
