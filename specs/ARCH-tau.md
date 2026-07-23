# ARCH-tau: Tau architecture

Tau is a Rust workspace whose end-user `tau` binary composes first-party components. `tau-cli` starts and connects to the `tau-harness` daemon; the harness owns sessions, event sequencing, extension lifecycle, provider dispatch, and harness-owned tools. `tau-core` supplies state, routing, policy, sessions, and tool registration. `tau-proto` owns shared wire types and CBOR contracts, while `tau-client` provides the client/extension runtime and `tau-socket` supplies local Unix transport. Extensions and provider backends depend on those shared boundaries rather than owning harness state.

Peer tool providers declare registration lifecycle through transient events;
the harness validates committed declarations and publishes canonical runtime
state as specified by
[SPEC-tool-declarations-and-canonical-state](SPEC-tool-declarations-and-canonical-state.md).
They submit transient progress observations separately; the harness validates
committed routed-call ownership and publishes protected canonical progress as
specified by
[SPEC-tool-progress-reports-and-canonical-facts](SPEC-tool-progress-reports-and-canonical-facts.md).
Terminal result, error, and cancellation observations follow the same
commit-before-validation boundary. The harness alone publishes protected
terminal and provider transcript projections as specified by
[SPEC-terminal-tool-reports-and-canonical-outcomes](SPEC-terminal-tool-reports-and-canonical-outcomes.md).
Configured Provider/Tool/Core peers publish tool routing intents before the
harness checks correlation or resolves a provider. Started and rejected outcomes
are harness-authored, and durable request replay is observation-only, as specified
by [SPEC-tool-requests-and-routing](SPEC-tool-requests-and-routing.md).
Configured providers likewise publish execution reports before the harness validates
prompt or retry correlation and derives canonical facts or directed outcomes, as
specified by
[SPEC-provider-execution-reports-and-canonical-facts](SPEC-provider-execution-reports-and-canonical-facts.md).
Provider extensions similarly report bounded quota replacements, patches, and
clears before validation; only the harness publishes accepted current snapshots,
as specified by
[SPEC-provider-quota-pacing](SPEC-provider-quota-pacing.md).
Every authenticated configured extension kind may publish `persist=false`-by-default
prompt-fragment declarations. The harness commits each surviving declaration
before replacing its exact live connection's runtime prompt projection, as
specified by
[SPEC-prompt-fragment-declarations-and-projection](SPEC-prompt-fragment-declarations-and-projection.md).
The same configured-kind authority and commit-before-effects boundary governs
per-agent context registration, values, and readiness under
[SPEC-per-agent-context-declarations-and-readiness](SPEC-per-agent-context-declarations-and-readiness.md).
Configured extensions also publish transient internal-prompt requests before
the harness validates loaded-agent correlation and derives prompt facts, as
specified by
[SPEC-internal-prompt-submit-requests](SPEC-internal-prompt-submit-requests.md).
Configured extensions and attached socket UIs request per-agent metadata
mutations before validation; only the harness publishes durable canonical
metadata facts, as specified by
[SPEC-agent-metadata-requests-and-canonical-facts](SPEC-agent-metadata-requests-and-canonical-facts.md).
They publish transient start-agent requests through the same commit boundary
before role, parent, duplicate-route, and child-creation processing, as specified
by [SPEC-start-agent-requests](SPEC-start-agent-requests.md).
Configured extensions and attached local UIs publish terminal-output side-effect
events through ordinary commit, but terminal consumers act only on live delivery
and the events never enter semantic replay, as specified by
[SPEC-terminal-output-side-effect-events](SPEC-terminal-output-side-effect-events.md).
Configured extensions and attached local UIs also publish extension-owned
custom events through ordinary commit for direct live subscriber consumption;
opaque payloads never enter semantic replay. See
[SPEC-custom-extension-events](SPEC-custom-extension-events.md).
Attached local UIs publish prompt-draft and focus liveness observations through
ordinary commit for live subscribers only; neither event enters semantic replay.
See
[SPEC-ui-prompt-draft-and-focus-events](SPEC-ui-prompt-draft-and-focus-events.md).

External transport identity and trust boundaries are governed by [ARCH-external-message-boundary](ARCH-external-message-boundary.md). Cross-provider streamed output is specified by [SPEC-provider-response-streaming](SPEC-provider-response-streaming.md), agent-message delivery and projection by [SPEC-agent-message-delivery](SPEC-agent-message-delivery.md), observation by [SPEC-agent-watch](SPEC-agent-watch.md), and context recovery by [SPEC-compaction-and-context-recovery](SPEC-compaction-and-context-recovery.md). Component-local architecture and decisions live beside their owning crates under `specs/`.
Interactive UI prompt facts retain raw canonical text and typed harness-stamped
provenance; provider assembly alone derives their fieldless `<user>` presentation
under
[DECISION-interactive-user-prompt-envelope](DECISION-interactive-user-prompt-envelope.md).

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
