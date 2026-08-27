# Decoded delivery memory measurement

Tau can recursively measure decoded delivery ownership when the
`tau_cli::delivery_memory=trace` or `tau_harness::delivery_memory=trace` target is
explicitly enabled. The mode is diagnostic only. It changes no admission,
delivery, queue, cursor, replay, overflow, backpressure, disconnect, or failure
outcome.

For a short diagnostic run:

```console
TAU_LOG='tau_cli::delivery_memory=trace,tau_harness::delivery_memory=trace' tau
```

Interactive CLI records use
`$XDG_STATE_HOME/tau/uis/<ui-id>/ui.log`; harness records use the session
`logs/tau-harness.log`. Setting `TAU_LOG` on `tau attach` enables only that new
CLI process; it cannot reconfigure an already-running harness. Restart or resume
the harness with the filter to enable harness measurement. A harness restart
resets harness high-water state. Another CLI attachment has an independent CLI
tracker and adds one consumer to the running harness tracker.

## Reported quantities

- `encoded_bytes` is the complete observed CLI frame size. Harness live-suffix
  measurements reproduce the canonical outbound CBOR encoding.
- `decoded_logical_bytes_estimate` sums recursively visited text and byte-string
  leaves.
- `decoded_requested_capacity_estimate` sums capacities requested by an
  enabled-only recursive CBOR value projection. It is a shape comparator, not
  the exact typed Rust layout.
- `shared_allocations` charges one canonical harness live-suffix `Arc` once.
  `shared_fanout` reports its in-process `Arc` strong-reference count, while
  `pending_target_fanout` separately reports frozen attachment ownership.
- item, owner, container, expansion, overlap, and high-water fields are bounded
  content-free aggregates.

The CLI reports decode/current, cold staging, renderer FIFO, scheduler
lookahead/fold, and handler cuts independently. The harness reports the
canonical live suffix and simultaneous attachment fanout. The renderer's final
visible/hidden presentation contains transformed and selectively copied data,
so the current probe labels retained projection bytes unobservable rather than
charging the input estimate to it.

## Workload and uncertainty

Deterministic probes use deeply nested text and byte sequences, owner moves
through every CLI cut, shared live payloads, and simultaneous consumers. These
shapes expose encoded-to-decoded expansion and overlapping ownership without
recording content.

Allocator usable size, fragmentation, stack memory, RSS, socket queue occupancy,
and kernel buffers are not deterministic Rust ownership. The probes make no
claim about them and report `kernel_bytes_observable=false`. OS-specific RSS or
socket samples may be useful experimental context, but they cannot establish a
whole-process or end-to-end bound. Independent CLI and harness aggregates carry
no authoritative cross-process delivery identity or state. Exact sizes, shape
ratios, fanout, and timestamps can still permit heuristic correlation and reveal
workload metadata. Treat the operational trace files as private.

Disabled mode creates no tracker `Arc`, recursive projection, or accounting map;
it performs one startup trace-interest check and option branches at instrumented
cuts. Enabled mode serializes each immutable live-suffix frame once, recursively
walks each CLI input once, rescans content-free aggregates while holding local
ownership locks, and emits synchronously through tracing. A pinned unbounded
live suffix can therefore make even cached aggregate scans and trace output
expensive. Use this mode only for short diagnostics.
