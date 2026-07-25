# DECISION-persisted-session-activity-accounting-authority: Persisted session activity accounting authority

Authority: confirmed, 2026-07-25, dpc

## Decision

Forward-only agent journals must preserve the immutable facts needed to account
for session activity without consulting current configuration, transient runtime
state, diagnostic logs, or inferred ancestry:

- The existing `agent.started` fact gains a `creator` field carrying tagged
  creator provenance: user, session-qualified agent identity, or extension with
  stable extension identity. The creator records the authenticated initiating path;
  `parent_agent` remains creation ancestry and does not imply creator identity.
  User creation with an explicit parent is therefore still user-created, while
  agent-owned local and cross-session starts identify the initiating agent and its
  session, and a start with no user or agent owner is attributed to its extension.
- Dedicated harness-authored `agent.outer_turn_started` and
  `agent.outer_turn_finished` facts carry a stable per-agent outer-turn ID,
  session ID, exact initiating activation-occurrence correlation, and terminal
  disposition. Exactly one non-overlapping turn starts
  when an accepted activation moves the agent from idle to running; inputs accepted
  while it remains running join that turn and cannot create another. The start
  identifies the exact durable input occurrence or carries an equally stable
  correlation copied from a non-journaled accepted input, not merely its source
  category. Exactly one matching terminal fact records the transition back to
  idle. A crash-cut journal may end with one unmatched start, which represents an
  explicitly unterminated turn; readers never synthesize or infer its terminal
  boundary. Every ordinary inference `agent.prompt_started.outer_turn_id`
  identifies its owning outer turn; work outside an outer turn, including
  standalone compaction, carries no outer-turn ID.
- The existing durable `agent.prompt_started` fact gains a `model_params` field
  carrying the complete captured `ModelParams` dispatch snapshot. Historical
  model-and-effort accounting uses that snapshot rather than mutable role
  configuration or model defaults.
- The existing canonical durable `provider.response_finished` fact gains
  `estimated_api_cost_rates` and `estimated_api_cost_increment` fields carrying
  the harness-selected effective rates and exact increment computed from that
  response's accepted local usage counters. A response with no billable usage
  records a zero increment. Providers do not author these
  accounting fields. The response's local token counters and cost snapshot are its
  accounting authority; the nested cumulative `usage.stats` presentation snapshot
  is not historical accounting input and must never be summed or consulted by
  `tau session stats`.

These per-occurrence journal facts, joined by their stable creation, outer-turn,
and prompt identities, are the sole durable session-activity accounting
authority. Aggregation includes all accepted facts attributed to the session,
independent of the currently selected transcript branch. Tau does not persist a
parallel accounting sidecar or a mutable cumulative cost total.

This is an incompatible forward-only journal contract. Tau provides no migration,
backfill, dual-read path, or inference for older journals; operators must discard
or reset them under
[DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).

## Rationale

Creation provenance and outer-turn lifetime are distinct semantic facts: neither
can be reconstructed reliably from ancestry, prompt origin, provider completions,
or transient turn state. Their dedicated immutable lifecycle representation
prevents attribution and boundary guesses. Model parameters belong on the
existing prompt materialization authority because they are one dispatch snapshot,
while cost rates and increments belong on the existing canonical terminal
response because they are computed once from that response's accepted usage.
This keeps raw facts auditable and avoids a second mutable accounting authority.

The decision is required by
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md)
and preserves the prompt materialization authority established by
[DECISION-compact-prompt-materialization-authority](DECISION-compact-prompt-materialization-authority.md).
