# Trust and data

Read this before using Tau with private repositories, credentials, or external
integrations. This page is a newcomer summary; [SECURITY.md](../SECURITY.md) and
the linked component documentation define the precise technical boundaries.

## What can access repository files

Tau's shell and filesystem capabilities run as configured local extension
processes. They can read and change files available inside their process
environment when the active role permits the corresponding tools. Model
instructions such as “do not edit” guide behavior but do not create an enforced
read-only workspace.

Configured extensions are trusted same-user executables. Tau can supervise and
mount-restrict them, but those controls are defense in depth, not containment
for malicious local code. Persistent supervised extensions receive other Tau
state recursively read-only by default; set `tau_state_access: hidden` for a
configured extension that should not see unrelated Tau state. See
[Configuring extensions](extensions.md#tau-state-access).

Installing an extension only puts its executable on `PATH`. It does not enable
the extension, create an external account, add credentials, or authorize
senders, routes, or tools.

## What leaves the machine

The selected inference provider receives the request context needed for a turn.
Depending on the conversation, that can include prompts, repository content,
images, tool definitions, and tool results. The provider's own account terms,
data controls, retention, residency, and billing apply.

Network-capable tools and enabled integrations can contact their configured
services. Provider-hosted web search happens at the inference provider; other
web-search extensions contact their configured backends. An allowlist or state
mount is not a claim that all network traffic is locally confined.

Tau keeps provider kinds distinct. In particular, ChatGPT/Codex OAuth access
uses ChatGPT account entitlements and limits, while API-key profiles use the
corresponding API service and billing. A ChatGPT subscription is not API credit.

## What Tau stores locally

On Linux with the default XDG paths:

```text
~/.config/tau/              configuration
~/.local/state/tau/         credentials, sessions, agents, and UI state
~/.local/state/tau/sessions/<session-id>/
~/.local/state/tau/agents/<agent-id>/
```

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` override those roots. Provider settings
are credential-free records under `providers/`; OAuth tokens and API keys use
typed owner-private Secret records under `secrets/`.

Durable session and agent cleanup is disabled by default. Diagnostic cleanup
defaults to 30 days. Session extension logs follow session retention rather than
diagnostic retention.

For durable provider activity, exact provider request/response capture is
default-on where supported. Captures live under the owner-private session
`debug/provider-requests/` directory and can contain full prompt, tool, model,
path, response, account, or provider-controlled content. Capture is bounded and
best-effort, so missing files do not prove that no provider request occurred.
Treat every capture as private and inspect it before sharing.

The effective retention settings are startup configuration in `harness.yaml`:

```yaml
session_retention: null
agent_retention: null
diagnostic_retention: 30d
artifact_retention: null
```

`null` disables that cleanup policy; it does not disable storage. See
[provider diagnostics](providers.md) and Tau's
[debugging self-help](../crates/tau-skills/self-knowledge/tau-self-knowledge-debugging.md)
before changing or sharing stored data.

Shared original-byte [artifacts](artifacts.md) outlive their creating sessions.
Explicit puts, including new uploads of duplicate bytes, renew their shared age;
reads, imports, sharing, and recognized same-upload retries do not. Ephemeral
sessions in persistent harnesses can explicitly store these originals; memory-only
harnesses cannot read, create, or clean them. Artifact cleanup is opportunistic
startup maintenance, not a hard TTL, and journal references do not pin originals.

## Before asking for support

Share the minimum evidence that identifies the failing boundary:

- Tau version and install route;
- OS, kernel, and architecture;
- failing command or journey step;
- the exact error after removing secrets and private content; and
- whether the failure reproduces with a minimal configuration.

Do not send the whole state directory, transcript, `events.jsonl`, extension
log, or provider capture by default. Identifiers, paths, model/profile metadata,
timing, and content-free diagnostic records can still be sensitive.
