# ARCH-tau-provider: Shared provider runtime policy

`tau-provider` owns provider-neutral storage, retry/repetition helpers, and the
immutable outbound network policy used by built-in backends. It does not own
provider profiles, request lowering, session state, logical retry scheduling, or
provider response semantics.

The shared provider debug-capture writer accepts already-serialized request and
response metadata through one bounded process-wide nonblocking FIFO. Its
detached worker validates durable session paths, zstd-compresses records, and
performs all filesystem I/O. Overload, worker startup/write failure, and process
exit may omit captures; the process-lifetime sender exposes no shutdown, drain,
or join API, so provider and UI work never waits for capture completion. A
streaming compression/write failure or process interruption can leave a
truncated final `.json.zst`; readers must treat decompression failure as an
omitted diagnostic.

Provider startup captures one `Arc<OutboundNetworkPolicy>` and injects it into
each backend runtime. The snapshot selects lowercase proxy variables before
uppercase equivalents, applies DNS-free `NO_PROXY` matching, and optionally
loads the bounded `TAU_PROVIDER_CA_BUNDLE`. Invalid selected configuration
remains an immutable typed error until restart.

The policy constructs explicit async reqwest clients with redirect and
environment discovery disabled. HTTP/WS targets select the HTTP proxy class;
HTTPS/WSS targets select the HTTPS class; both fall back to ALL_PROXY. A selected
proxy is the only route. TLS always uses the platform verifier plus strictly
parsed additive roots.

Transport failures expose only closed route, phase, and category facts. Provider
backends retain ownership of HTTP/provider status classification and product
behavior. See [SECURITY.md](../SECURITY.md) for the threat boundary and
[the repository gate](../../../specs/GATE-provider-backend-split-and-codex-ws-only.md)
for confirmed authority.
