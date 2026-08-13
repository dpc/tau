# ARCH-tau-provider: Shared provider runtime policy

`tau-provider` owns provider-neutral storage, retry/repetition helpers, and the
immutable outbound network policy used by built-in backends. It does not own
providers, request lowering, session state, logical retry scheduling, or
provider response semantics.

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
