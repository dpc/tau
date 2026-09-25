# SPEC-per-agent-extension-workdirs: Per-agent extension workdirs

## Record justification

Per-agent workdirs span ext-shell metadata folding and tool admission, harness
context projection and inheritance, extension-instance configuration, and
user-shell routing. No one local component can describe their shared
namespace, ordering, visibility, and failure rules completely.

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
existing directory, then completes only after the matching metadata commit and
the source-local discovery replacement is durably installed. At
most one setter may be pending for an agent and instance. The request carries an
opaque correlation id echoed by the committed fact, so an unrelated same-value
write cannot impersonate the setter's linearization point. Absolute setters can
repair stale or invalid state. Missing, renamed, inaccessible, non-directory, or
malformed stored values are retained and fail closed rather than falling back.

Every filesystem, shell, directory-lock, and user-shell invocation snapshots the
last committed workdir at admission. Queueing, lock waiting, and later execution
use that same path. Sibling calls in one provider batch have no causal ordering;
a call depending on a successful workdir change must be made in a later turn.
The generic shell's call-level `cwd` and the ChatGPT-facing
`shell_command.workdir` remain invocation-local overrides and must never mutate
workdir metadata. The latter is distinct from the top-level persistent
`workdir(path)` tool, as required by
[REQ-model-trained-tool-compatibility](REQ-model-trained-tool-compatibility.md).

The verified Codex CLI interface at revision `2f7d89b141` uses an
invocation-local argument named `workdir` for both legacy `shell_command` and
unified `exec_command`; `write_stdin` has no directory argument. Tau's
ChatGPT-facing `shell_command` therefore advertises and accepts only `workdir`,
never a runtime-only legacy `cwd` alias. Omitting `workdir` uses the shell
instance's remembered persistent path. Sibling calls in one provider batch have
no causal ordering, so a persistent setter must complete before a dependent
call in a later turn.

Dynamic prompt context reports only the current path/status associated with the
visible default or configured tool prefix. It does not enumerate configured
instance identities or repeat tool discovery. When the effective role/model hides
the workdir capability, its shell-workdir guidance is also absent. The guidance
explains that each visible shell family has independent state, that a setter
affects later calls from only that instance, and that dependent calls require a
later turn; it may call out cwd-sensitive configured wrappers without claiming
that any wrapper is enabled.

Agent-load project discovery uses the explicit remembered, inherited, or restored
cwd on the shell execution host; only absent metadata uses the process-startup
fallback. Every canonical persistent cwd change, including a same-path setter,
replaces only that shell instance's project skills and AGENTS contribution.
User inputs retain their existing lifecycle; builtin, other-instance, and
other-agent inputs are unchanged. Getters and invocation-local overrides do not
refresh discovery. The existing root order, symlink traversal, collision, and
role-filter rules still apply.

The canonical commit starts a source-local readiness barrier. Dependent inference
and selected-agent `:skill` expansion wait for the latest installed replacement,
independently of setter cancellation or backgrounding. A superseded scan cannot
install over a newer mutation. No agent reinitialization, provider restart, or
cache-history rewrite occurs. The durable bootstrap slot is replaced rather than
appending another AGENTS message; already-loaded skill messages and historical
transcript/compaction content remain unchanged. See
[SPEC-per-agent-context-declarations-and-readiness](SPEC-per-agent-context-declarations-and-readiness.md).

A true scan failure, timeout, or disconnected scanner removes stale project input,
retains user and other-source input, and installs an explicit degraded diagnostic.
An affected setter reports that the cwd committed but discovery failed, never
claims rollback. Established/restored agents missing a required skill remain
loaded and usable for an absolute or same-path repair; fresh initialization stays
strict. Successful repair replaces the degraded view.

User `!` and `!!` commands execute through exactly one shell instance and from
the target agent's admission-time workdir. With no instance they fail; with
several instances they fail until an explicit selection mechanism exists rather
than broadcasting ambiguously.

This behavior implements
[REQ-independent-manipulation-extension-instances](REQ-independent-manipulation-extension-instances.md)
and preserves the configured-extension trust boundary documented in
[SECURITY.md](../SECURITY.md).
