# DECISION-tau-harness-effective-tool-surface-authority: Harness-owned effective tool surface

Authority: unconfirmed

For each provider prompt, the harness owns one immutable effective tool snapshot
after role policy and provider capability filtering. That snapshot is the single
authority for provider definitions, prompt capability claims, tool fragments,
authorization, and prompt-owned rejection diagnostics. Later role/model changes,
registrations, or runtime state cannot rewrite an already-dispatched prompt's
authority.

Extensions and providers publish neutral metadata rather than choosing which
model receives which tool surface. Effective visible-name collisions are
rejected instead of resolved by registry order. Tool examples are omitted from
provider definitions and may appear only as bounded, failure-triggered,
one-shot diagnostic help.

One harness-owned snapshot prevents advertised capabilities, authorization, and
diagnostics from disagreeing across mutable runtime boundaries. The tradeoff is
that dispatch must fail explicitly when rendering or effective-surface
construction cannot produce one coherent snapshot.

Exact filtering, policy precedence, sparse template data, parallel-call claims,
diagnostics, and lifecycle behavior are specified by
[SPEC-tau-harness-prompt-dispatch](SPEC-tau-harness-prompt-dispatch.md).
