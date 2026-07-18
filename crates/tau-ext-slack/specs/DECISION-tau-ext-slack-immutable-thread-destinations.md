# DECISION-tau-ext-slack-immutable-thread-destinations: Thread destinations are immutable authenticated routes

Authority: confirmed, 2026-07-14, dpc

A thread is an optional immutable root timestamp under one Slack conversation, not an
independent conversation. An incoming reply uses `thread_ts`; a configured
receive-enabled fixed-thread route also matches its root create when `ts` equals that
root and normalizes it to the root. Parent receive routes retain each actual optional
incoming root. Reply routes and prepared proactive sends retain this exact value; the
sent fact and result are derived from the same frozen route.

`slack_send` exposes no thread argument, never substitutes a child timestamp, and never
broadcasts a reply. Fixed-thread aliases may bind the root directly.
