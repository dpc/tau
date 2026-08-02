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
