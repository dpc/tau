# Security policy

Tau is early-stage software, but security issues are important. Please report suspected vulnerabilities through GitHub private vulnerability reporting for `dpc/tau` (<https://github.com/dpc/tau/security/advisories/new>) when available. If that path is unavailable, contact the maintainer privately first and avoid filing a public issue with exploit details.

For technical trust boundaries, start with [ARCH-external-message-boundary](specs/ARCH-external-message-boundary.md) and the applicable project and component records under `specs/` and `crates/*/specs/`.
Authenticated ChatGPT quota acquisition and its credential-free lifecycle are
documented in
[`tau-provider-chatgpt/SECURITY.md`](crates/tau-provider-chatgpt/SECURITY.md)
and
[`tau-ext-provider-builtin/SECURITY.md`](crates/tau-ext-provider-builtin/SECURITY.md).

Peer harness messaging is cooperative same-UID local IPC, not a hostile-process
sandbox or per-sender ACL. Callback correlation prevents accidental sender/route
confusion before bounded admission or model-spending auto-start, while peer text
remains model input rather than a harness instruction. Delivery is best-effort
at-least-once: an ambiguous crash or retry can duplicate prompts, agents, model
work, and spend.

## Local IPC and external ingress

Configured extension processes are trusted local executables. “Less-trusted
extension” means protocol authority is limited—the harness still validates phase,
source ownership, routing identity, configuration, and collisions—not that the
stdio stream is a hostile availability boundary or process sandbox. Operation
quotas do not promise to bound protocol deserialization; see
[`SPEC-tau-harness-session-state`](crates/tau-harness/specs/SPEC-tau-harness-session-state.md#extension-data)
and [`ARCH-tau-supervisor`](crates/tau-supervisor/specs/ARCH-tau-supervisor.md#child-environment).
Generic configured-extension spawn diagnostics treat the configured instance
name, resolved executable, and explicitly configured cwd as non-secret metadata;
do not place credentials or tokens in those fields. Diagnostics bound and escape
those fields, include cwd only when configured, and preserve the underlying
operating-system error/source chain. They never retain or render command
arguments, full extension configuration, environment values, or resolved secret
values. Re-check this contract whenever extension spawn configuration or
startup/respawn logging changes.
Robust framing and cleanup improvements are welcome when scoped, but unrelated
features must not be expanded into slowloris, connection-flood, or sandbox
hardening without an approved threat-model design.

Inter-harness/session communication is likewise cooperative same-UID IPC, with
correlation and bounded model-spend admission rather than hostile-sender ACLs.
Genuinely untrusted ingress is external network/service content received through
Slack, XMPP, Telegram, providers, web fetches, and similar adapters. Authenticate
and bound that adapter boundary where applicable and keep payloads untrusted model
content; proxying them through an extension does not make the local extension
transport itself adversarial. The boundary summary is recorded in
[`ARCH-external-message-boundary`](specs/ARCH-external-message-boundary.md).

The Slack bridge requires exact configured conversation/kind/thread policy and
verified live-human admission. Receive permission creates only opaque
source-bound reply authority; proactive permission is a separate alias-only
grant. Dynamic DMs remain bounded, allowlist/exact-user-bound, and reply-only.
The separately authorized, default-off Slack reaction tool accepts only commit-accepted opaque exact-message refs, requires current route and role authority, and permits removal only of same-agent runtime-owned reactions. It adds `reactions:write` without reaction listing; reactions are externally visible and can trigger notifications or workflows.

The separately authorized Slack discovery tool reveals all static model-facing
aliases and configured policy, including receive-only routes, but excludes native
routes, dynamic links, identities, runtime state, and Slack-fetched metadata.
`security_mode: lax` materially widens prompt-injection exposure on static
routes and must not be treated as control authority. Slack, workspace
administrators, Slack Connect participants, and conversation members may read
transported text; this is not an end-to-end encrypted channel.
Slack-specific review triggers and failure/replay invariants are recorded in
[`crates/tau-ext-slack/SECURITY.md`](crates/tau-ext-slack/SECURITY.md).

## Standalone compaction recovery reliability

Standalone compaction and its continuation are harness-owned durable work. Every
new provider cut must be a closed transcript prefix; a tool-calling assistant
response and its complete terminal results node are indivisible. A failed
transaction with a resume watermark remains fail-closed until an explicit
successor preserves same-branch coverage of that watermark. A successor may
retreat its cut to retain more exact suffix, but it must not replace the owed
watermark with an ancestor or sibling selected by later head navigation.
Ordinary input and `/cancel` do not abandon this ownership; if the selected head
no longer descends from the owed watermark, explicit recovery must remain
blocked. Core validation and warm/cold replay regressions enforce these rules.
Revisit them when adding any explicit abandon/rewind operation or changing
compaction replay ownership.

## Release build resource reliability

The universal release binary's accepted build-time, memory, size, and runtime
tradeoffs are documented in
[`DESIGN-release-build-profile`](specs/DESIGN-release-build-profile.md) with
measurement details in
[`docs/release-builds.md`](docs/release-builds.md). The design record owns the
reliability limits and revisit triggers; the evidence document owns the
measurements.

## Reporting guidance

When reporting a vulnerability, include:

- affected Tau version or commit;
- operating system and relevant configuration;
- minimal reproduction steps;
- whether an extension, provider, UI client, or daemon boundary is involved;
- any logs that do not contain secrets.

Avoid sharing API keys, OAuth tokens, email contents, or other private data in
reports.
