# Provider and client output-cost measurement

Tau has two disabled-by-default private TRACE targets for measuring the local
provider-output path:

- `provider-builtin.output-cost` covers public-Responses sampler
  materialization, typed worker-report measurement/admission, worker queue
  depth/age, and main-loop drain batches.
- `tau_client::output_cost` covers exact frame measurement, local admission,
  writer wait, transport encoding, and flush.

Both schemas are fixed-cardinality and content-free. They use only closed class
labels, scalar counts/bytes/durations, and process-local numeric correlation.
Provider correlation joins sampler materialization to worker queue ownership;
client correlation is deliberately independent because no cross-crate or
extension-interface seam is added.
They do not create protocol fields, events, journals, captures, debug JSON, or
protocol messages. Provider TRACE follows ordinary extension stderr and can be
retained in the owner-private per-session component log or operational mirror;
client TRACE follows the host process's configured sink. Keep enabled traces
owner-private because exact timing, size, ordinal correlation, and nearby log
context reveal workload metadata.

The ignored client release fixture uses fixed repetitions and content-free `x`
payloads for zero/small/growing frames, executes the real 64-frame detached FIFO
with blocked transport, and admits below/equal/rejects above the exact 8 MiB
client boundary. The separate provider fixture starts 1/2/8 real producer
threads through the worker report sink and runs the production queued targeted
cancellation/reuse path with enabled queue/dequeue observations. Each CSV row
names only the production path it actually executes; no row fabricates an
inapplicable downstream phase.

The campaign is measurement-only. Evidence must be packaged separately with
the exact candidate revision and fixture command. This bounded fixture selects
no production optimization: its aggregate release timings establish coverage
and applicability but do not rank the private per-phase TRACE values. Any later
optimization proposal requires a separate private live capture showing one
phase dominates; this task implements none.
