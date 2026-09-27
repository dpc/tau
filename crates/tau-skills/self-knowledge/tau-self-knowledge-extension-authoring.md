---
name: tau-self-knowledge-extension-authoring
description: Use when building or registering a Tau extension process, declaring tools or models, subscribing to events, or writing an interceptor.
advertise: false
---

# Build a Tau extension

Start with `docs/extensions.md` for configuration, instance enable/require
policy, secret delivery, state access, and supervised logs. Extensions are
cooperative same-user processes, not adversarial sandbox tenants. Use the
versioned `tau-proto` types and `tau-client` transport; consult
`docs/sdk-releases.md` and `specs/SPEC-extension-protocol-versioning.md`
when matching a separately built extension to the harness. Do not use an old
published protocol number from a historical feature page as the current
checkout's compatibility guarantee.

Register tools, prompt fragments, or model routes through the corresponding
protocol declarations. Event subscriptions and interceptors are separate
capabilities: read `docs/messages.md` for external message publication and
`docs/interceptors.md` for matching, failure, backpressure, and persistence
semantics. `tau-self-knowledge-ext-rhai` provides a user-facing scripting
alternative, not a replacement for the extension protocol.

For a credential-free declaration preview run
`tau dev preview-declarations`; it reports config-derived declarations and
gaps, **not** effective prompt/tools, selected role/model policy, credentials,
or runtime readiness. It may launch configured trusted extension executables
in a bounded inspection exchange. A partial or failed report is not an empty
runtime inventory. For effective composition, use
`tau --role ROLE dev print-tools` and `tau --role ROLE dev print-prompt`;
these start a temporary ordinary harness, configure extensions, and can have
their normal side effects. See `docs/declaration-inspection.md` for the
precise preview scope and outcomes.
