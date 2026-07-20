# SPEC-tau-proto-provider-updates: Provider updates

This protocol record refines [SPEC-provider-response-streaming](../../../specs/SPEC-provider-response-streaming.md), [SPEC-agent-watch](../../../specs/SPEC-agent-watch.md), and [SPEC-compaction-and-context-recovery](../../../specs/SPEC-compaction-and-context-recovery.md).

## Provider response streaming updates

`provider.response_updated_reported` is the Provider-authored transient append-delta
surface. After generic commit and owner validation, the harness publishes canonical
`provider.response_updated` for visible assistant/reasoning progress. Providers must send newly appended text in
`deltas`, not full accumulated message snapshots; retry/status diagnostics belong
in the separate `status` field because they are provider-authored, not
assistant-authored. Live byte/duration stats are provider-owned content-free metadata carried in
`response_stats`; UIs render them directly from `provider.response_updated`.
Fresh transport setup may emit a fixed content-free status with no retry facts;
it must not expose endpoints, credentials, accounts, or raw transport errors.
`provider.response_finished.output_items` remains the complete durable response
and replay source.

The same report/canonical split covers submitted, finished, cache-diagnostic, and
directed retry-result payloads. The report variants reuse their canonical DTOs but have
distinct `_reported` wire names; there is no canonical provider retry-result event. See
[SPEC-provider-execution-reports-and-canonical-facts](../../../specs/SPEC-provider-execution-reports-and-canonical-facts.md).

## Structured watched provider status

Transient retry updates may carry `ProviderRetryStatus`: a closed work category, saturating attempt number, and approximate whole-second delay. Human status text remains UI presentation and is not an authority for harness decisions. `AgentWatchProviderStatusNotification` is the harness-authored cross-agent projection and contains only bounded facts, prompt/turn correlation, watch subscription identity, and a nested serde-tagged `state`. The `phase` discriminator selects `retrying`, `recovering_context`, `blocked`, `dispatch_uncertain`, or `terminal_error`; each variant owns exactly the category, attempt, delay, or failure fields valid for that phase, so contradictory option combinations are neither constructible nor decodable. `recovering_context` represents active harness-authorized reactive compaction; retrying, recovery, blocked-compaction, restored dispatch-uncertain, and terminal-error projections are emitted according to current work state.
Retry presentation keeps exact seconds for waits below one hour, then uses a
rounded two-unit minute/hour/day form so a legitimate distant reset remains
readable without changing the underlying whole-second fact.

### Reactive context recovery correlation

Context-overflow recovery is correlated entirely by durable facts. Inference checkpoints capture the provider-qualified model, operation, and immutable pre-activation cut. The harness, never the provider, stamps an eligible terminal response as `reactive_compaction_planned`; a reactive standalone-compaction start then uniquely claims that failed prompt id. Legacy checkpoints omit the new cut facts and are recovery-ineligible.

### Durable manual-compaction facts

`agent.manual_compaction_requested` records harness-owned pre-start
correlation. Exactly one matching `agent.standalone_compaction_started` with a
`manual_agent_tool` trigger or
`agent.manual_compaction_request_failed` may terminate that pre-start state.
These control facts are persisted for durable agents and use the same
memory-only semantics as other facts for ephemeral agents.
Terminal context-window responses may carry optional harness-authored
`context_limit_telemetry`. The harness overwrites extension input and correlates
the immutable dispatch snapshot by prompt, exact model, and operation. Closed
observation, policy, eligibility, and action tags contain no prompt/provider
body. `action=reactive_compaction_planned` is valid only with the matching
`recovery_disposition`; absent fields preserve legacy decoding.

Raw provider input usage remains separate from the optional exact JSON byte
length of transcript growth. The conservative token projection uses byte-free
JSON structure plus explicit canonical-image byte and patch accounting; neither
projection nor exact serialized bytes are provider-token evidence. Categorical
below/at-or-above observations require a positive advertised limit and nonzero
provider usage, while projection-only or contradictory cases are
`insufficient_evidence`. If exact transcript serialization or checked
projection aggregation is unavailable, the corresponding field is absent.
These diagnostics never calibrate limits or
thresholds automatically.
