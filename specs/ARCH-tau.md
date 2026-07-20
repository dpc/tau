# ARCH-tau: Tau architecture

Tau is a Rust workspace whose end-user `tau` binary composes first-party components. `tau-cli` starts and connects to the `tau-harness` daemon; the harness owns sessions, event sequencing, extension lifecycle, provider dispatch, and harness-owned tools. `tau-core` supplies state, routing, policy, sessions, and tool registration. `tau-proto` owns shared wire types and CBOR contracts, while `tau-client` provides the client/extension runtime and `tau-socket` supplies local Unix transport. Extensions and provider backends depend on those shared boundaries rather than owning harness state.

Peer tool providers declare registration lifecycle through transient events;
the harness validates committed declarations and publishes canonical runtime
state as specified by
[SPEC-tool-declarations-and-canonical-state](SPEC-tool-declarations-and-canonical-state.md).

External transport identity and trust boundaries are governed by [ARCH-external-message-boundary](ARCH-external-message-boundary.md). Cross-provider streamed output is specified by [SPEC-provider-response-streaming](SPEC-provider-response-streaming.md), observation by [SPEC-agent-watch](SPEC-agent-watch.md), and context recovery by [SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md). Component-local architecture and decisions live beside their owning crates under `specs/`.

Local configured extensions are trusted host executables with limited protocol
authority, cooperative inter-harness IPC is a separate same-UID coordination
boundary, and adapter-facing external content is untrusted ingress. Reviewers must
select the applicable boundary through [`SECURITY.md`](../SECURITY.md) before
turning robustness observations into feature-blocking security requirements.

Dependency direction is inward toward shared protocol/core/client libraries: the harness composes them, the CLI and extensions communicate through protocol/client APIs, and transport bridges translate external systems without granting external payloads internal authority. Provider adapters classify and stream backend results, but the harness retains session, routing, recovery, and durable-state authority.

Architectural or externally meaningful functional changes to event logs or
journals and harness-extension interfaces are gated by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
Extension-owned durable state that can be reconstructed from committed facts follows
[DECISION-event-log-first-extension-state](DECISION-event-log-first-extension-state.md).
