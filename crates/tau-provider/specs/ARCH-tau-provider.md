# ARCH-tau-provider: Shared provider runtime policy

`tau-provider` owns provider-neutral storage, retry/repetition helpers, the
immutable outbound network policy used by built-in backends, and bounded
provider-neutral materialization for Tau's shared summary-compaction fallback.
Backend crates still own wire request lowering. This crate does not own
providers, session state, logical retry scheduling, or provider response
semantics.

The crate also owns the doc-hidden, unstable scalar carrier for
disabled-by-default backend-stage TRACE diagnostics. Backend adapters select it
once from the dedicated target and mark only boundaries they actually own; the
carrier emits one fixed-cardinality process-local observation and has no
protocol, event, journal, capture, or supported API representation. Disabled
selection constructs no carrier and takes no observation clock.

The shared provider debug-capture writer accepts already-serialized request and
response metadata through one bounded process-wide nonblocking FIFO. Its
detached worker zstd-compresses records and synchronously flushes a dedicated
non-journaled protocol message containing typed session/prompt attribution and
opaque bytes. The complete encoded message uses the shared 16 MiB protocol
ceiling. Overload, compression/protocol failure, and process exit may omit
captures. Capture compression and harness filesystem work remain detached from
provider request execution and terminal generation. Capture and terminal frames
share the ordinary non-preemptive extension IPC writer: a terminal queued after
an already-started capture frame waits for that frame, with no capture-specific
terminal gate, priority scheduler, or second stream.

Scalar `cache-diagnostic` captures additionally reserve a full 256-KiB record
through transport completion, capped at 64 reservations / 16 MiB including
in-flight serialized data. Admission allocates a process-local sequence before
its nonblocking capacity check; exhaustion disables new records. Rejected,
abandoned and failed provider-side records increment a saturating known-loss
counter. Harness-side loss remains unobserved. This budget does not change
existing raw-capture limits. The executable supplies its existing build identity
locally; no normal extension/configuration wire field is introduced.

The harness authenticates the configured Provider instance, accepts only known
durable-session attribution, derives
`debug/provider-requests/<instance>/`, and writes through a second bounded
best-effort worker without inspecting or decompressing the blob. Capture
directories and files are owner-only. The Provider receives no writable capture
mount or host capture path.

Provider startup captures one `Arc<OutboundNetworkPolicy>` and injects it into
each backend runtime. The snapshot selects lowercase proxy variables before
uppercase equivalents, applies DNS-free `NO_PROXY` matching, and optionally
loads the bounded `TAU_PROVIDER_CA_BUNDLE`. Invalid selected configuration
remains an immutable typed error until restart.

The policy constructs explicit async reqwest clients with redirect and
environment discovery disabled. HTTP/WS targets select the HTTP proxy class;
HTTPS/WSS targets select the HTTPS class; both fall back to ALL_PROXY. A selected
proxy is the only route. TLS always uses the platform verifier plus strictly
parsed additive roots. HTTP routes negotiate and decode gzip and zstd responses;
their existing body limits and transport-byte accounting apply to the decoded
payload. WebSocket upgrade requests carry the same HTTP content-coding
advertisement, but WebSocket frames do not use HTTP response decoding.

Transport failures expose only closed route, phase, and category facts. Provider
backends retain ownership of HTTP/provider status classification and product
behavior. See [SECURITY.md](../SECURITY.md) for the threat boundary and
[the repository gate](../../../specs/GATE-provider-backend-split-and-codex-ws-only.md)
for confirmed authority.
