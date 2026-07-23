# DECISION-interactive-user-prompt-envelope: Direct user prompt provider envelope

Authority: confirmed, 2026-07-23, dpc

Every accepted prompt whose harness-stamped submission source is `HumanUi`
projects into provider context as exactly one user-role text item:
`<user>{body}</user>`. The element has no attributes or added whitespace. Tau
replaces only exact `</user>` occurrences in the accepted effective prompt with
`&lt;/user&gt;` and otherwise preserves its text and whitespace.

The raw accepted text and submission source remain canonical in
`agent.prompt_submitted` or `agent.prompt_steered`. UI requests, transcript
presentation, prompt history, navigation, and watch fanout remain raw. Tau
carries typed provenance through the derived transcript and renders the element
only during provider assembly. The projection creates no second payload fact,
role, activation, wake, route, identity, or trust authority.

This covers existing-agent, new-agent initial, and queued/steered interactive UI
prompts. A successful `:skill` invocation wraps the harness-expanded canonical
prompt, so only an exact outer-close collision is replaced in the `<user>` body. Injected,
internal, extension, external-message, and agent-message inputs retain their
separate projections.

Live and replay use the same typed renderer. Decodable historical `HumanUi`
submissions reproject; source is never inferred. `AgentPromptSteered` gains a
required harness-stamped source under the no-backward-compatibility policy.
Committed compaction replacement windows are not rewritten, and provider cache
keys and buckets do not change. Tau adds no direct-prompt-specific content cap
and does not reuse the external-message text limit; existing bounded protocol,
journal, and provider-context failure rules continue to apply.

## Rationale

A small explicit boundary helps the model distinguish direct interactive user
instructions from other user-role context without adding provider objects or
inventing identity and trust metadata. Keeping raw text authoritative avoids
leaking presentation markup into persistence, UI, navigation, and IPC, while
late typed projection gives live and replay one deterministic path. Reusing the
external-message envelope would incorrectly assign publisher, authentication,
untrusted-content, activation, and routing semantics to the authenticated
interactive instruction channel.

This decision is approved under
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md)
and its body framing follows
[DECISION-exact-sentinel-prompt-envelopes](DECISION-exact-sentinel-prompt-envelopes.md).
It follows
[DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).
