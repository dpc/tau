# DESIGN-per-agent-extension-workdirs: Per-agent extension workdirs

Status: confirmed, 2026-07-15, dpc

Each shell extension instance owns an independent durable workdir for each agent:
`(configured extension instance name, agent id) -> workdir`. The configured
instance name selects the existing inheritable metadata key
`ext_<instance>_cwd`; connection ids, process ids, and tool prefixes do not.
Renaming an instance creates a new namespace, while changing only its prefix
retains state.

When an instance's key is absent after successful replay, that instance commits
its frozen, validated actual process cwd. Existing generic extension `cwd`,
ext-shell `config.working_directory`, and standalone `--workdir` startup controls
remain unchanged and contribute to that process cwd. The harness and UI do not
seed `core-shell` or copy a session path across filesystem namespaces. Stored
metadata always wins, including for a disabled instance that later returns.
Direct children snapshot-inherit inheritable keys and then diverge; root, peer,
and cross-harness agents do not gain implicit workdir inheritance.

The model-facing `workdir` tool replaces persistent `cd`. Omitting `path` reads
the remembered path and status. Providing `path` validates and canonicalizes an
existing directory, then completes only after the matching metadata commit. At
most one setter may be pending for an agent and instance. The request carries an
opaque correlation id echoed by the committed fact, so an unrelated same-value
write cannot impersonate the setter's linearization point. Absolute setters can
repair stale or invalid state. Missing, renamed, inaccessible, non-directory, or
malformed stored values are retained and fail closed rather than falling back.

Every filesystem, shell, directory-lock, and user-shell invocation snapshots the
last committed workdir at admission. Queueing, lock waiting, and later execution
use that same path. Sibling calls in one provider batch have no causal ordering;
a call depending on a successful workdir change must be made in a later turn.
The GPT and generic shell call-level `cwd` arguments remain invocation-local
overrides and must never mutate workdir metadata. Current Codex uses the
call-local name `workdir`; Tau deliberately defers that advertised rename to
`tau-agent-gj13`, as recorded by
[DESIGN-model-native-tool-surfaces](DESIGN-model-native-tool-surfaces.md).

Dynamic prompt context reports only the current path/status associated with the
visible default or configured tool prefix. It does not enumerate configured
instance identities or repeat tool discovery. Instruction and skill discovery
remain process/session startup behavior and is not rebased by workdir changes.

User `!` and `!!` commands execute through exactly one shell instance and from
the target agent's admission-time workdir. With no instance they fail; with
several instances they fail until an explicit selection mechanism exists rather
than broadcasting ambiguously.

This design implements
[REQ-independent-manipulation-extension-instances](REQ-independent-manipulation-extension-instances.md)
and preserves the configured-extension trust boundary documented in
[SECURITY.md](../SECURITY.md).

## Rationale

Configured shell processes can inhabit unrelated filesystem namespaces. Their
own process cwd is the only safe missing-key default, while generic metadata
already provides durability, replay, and child inheritance. Commit-linearized
setters and admission snapshots prevent concurrent work from drifting between
directories. Retaining unusable paths prevents silent execution in the wrong
tree.
