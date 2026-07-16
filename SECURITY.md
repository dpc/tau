# Security policy

Tau is early-stage software, but security issues are important. Please report suspected vulnerabilities through GitHub private vulnerability reporting for `dpc/tau` (<https://github.com/dpc/tau/security/advisories/new>) when available. If that path is unavailable, contact the maintainer privately first and avoid filing a public issue with exploit details.

For technical trust boundaries, start with [ARCH-external-message-boundary](specs/ARCH-external-message-boundary.md) and the applicable project and component records under `specs/` and `crates/*/specs/`.
The distinct deterministic and live/VCR test-fixture boundaries are documented
in [`tau-e2e-tests/SECURITY.md`](crates/tau-e2e-tests/SECURITY.md).
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

## Agent display names

Agent display/task names are presentation-only metadata, never routing or trust
identity and never provider context or semantic message content. Local names are
authoritative only within the session whose agent metadata supplied them; remote
names are eligible only when carried by a typed peer endpoint. Message labels
keep the stable id visible, escape controls, bidi/structural Unicode, delimiter
characters, quotes, and backslashes, then apply byte and terminal-column bounds
after escaping. Revisit these invariants whenever adding a new name source or
using names outside UI presentation.
See
[`SPEC-tau-cli-agent-message-labels`](crates/tau-cli/specs/SPEC-tau-cli-agent-message-labels.md)
for the rendering and session-provenance behavior.

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
Per-agent shell-instance workdirs are committed coordination state, not an
access-control boundary. Paths are interpreted only by the configured extension
instance that owns them. Malformed or unavailable remembered paths fail closed
until explicitly repaired, and user-shell routing failures remain local command
failures rather than changing extension authority. Directory locks remain
advisory coordination.
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
Transport ingress has no adapter reply authority before durable commit.
Deduplication is global across retained agent journals and preserves the first
target and route. Harness processes sharing an agents root serialize a
checksummed append-only locator transaction. Its dirty marker is parent-fsynced
before publication and retained after ambiguous journal failure; count and byte
capacity are prospective. Retention/import/pruning must take the same lock and
atomically rebuild. Unreadable, ambiguous, pruned, over-capacity, or
unpersistable canonical state rejects intake rather than falling back to a new
occurrence. A committed result activates at most one exact current
connection/session-generation/capability-epoch waiter. Re-check these invariants,
the focused locator state-machine tests, and the cold-reopen/waiter regressions
whenever agent retention, journal format, session rollover, capability
revocation, or locator storage changes. Subprocess power-loss testing remains a
future hardening layer rather than a current release claim.
The separately authorized, default-off Slack reaction tool accepts only commit-accepted opaque exact-message refs, requires current route and role authority, and permits removal only of same-agent runtime-owned reactions. It adds `reactions:write` without reaction listing; reactions are externally visible and can trigger notifications or workflows.

The separately authorized Slack discovery tool reveals all static model-facing
aliases and configured policy, including receive-only routes, but excludes native
routes, dynamic links, identities, runtime state, and Slack-fetched metadata.
`security_mode: lax` materially widens prompt-injection exposure on static
routes and must not be treated as control authority. Slack, workspace
administrators, Slack Connect participants, and conversation members may read
transported text; this is not an end-to-end encrypted channel.
Slack additionally binds `auth.test` bot/workspace identity to each supported
Events API wrapper and fails closed on missing, ambiguous, or mismatched
installation evidence. Native U/W ids remain authoritative; fetched display
names and operator aliases are presentation only. Agent sends reject raw Slack
mention/control markup; the optional source-mention field can name only the
verified human already bound to a live reply selector and is frozen with the
bounded retry body.
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

Ordinary-inference cancellation may release only the exact matching warm-process
`DispatchUncertain` owner so later work on that agent can proceed. Its transient
terminal and late-response rejection do not establish crash-exact cancellation
persistence; standalone-compaction ownership remains covered by the durable
rules above.

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
