# DECISION-tau-ext-slack-proactive-sends: Configured proactive transport sends

Authority: confirmed, 2026-07-14, dpc

A static conversation alias grants proactive initiation only when
`proactive_send:true`, independently of receive, registration, and dynamic-DM
authority. The model selects the configured alias, never native route or thread
coordinates. Current config and lifecycle authority are revalidated at send
time; discovery output is not a bearer capability.

This separates explicit operator-granted initiation from source-bound reply
authority. Exact selector and preflight behavior is
[SPEC-tau-ext-slack-conversation-routing](SPEC-tau-ext-slack-conversation-routing.md);
delivery, retry, cancellation, replay, and ambiguity are
[SPEC-tau-ext-slack-send-delivery](SPEC-tau-ext-slack-send-delivery.md).
