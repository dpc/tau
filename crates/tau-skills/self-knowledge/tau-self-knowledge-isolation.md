---
name: tau-self-knowledge-isolation
description: >
  Use this skill when the user asks how Tau hardens or isolates supervised
  extensions, including tau_state_access, Linux namespaces, state mounts,
  provider exceptions, filesystem visibility, or the trust boundary.
advertise: false
---

# Tau extension hardening and isolation

Tau supervises configured extensions as trusted same-user executables. Its
namespace and mount setup reduces accidental access to unrelated Tau state; it
is defense in depth, not hostile-code containment. `SECURITY.md`,
`GATE-configured-extension-trust-boundary`, and
`SPEC-extension-secret-storage` define that boundary.

## State visibility policy

Supervised extensions use `tau_state_access`, globally or per instance:

```yaml
tau_state_access: hidden
extensions:
  diagnostics:
    tau_state_access: read_only
  legacy-integration:
    tau_state_access: legacy
```

`hidden` is the default and presents an empty read-only Tau state tree.
`read_only` presents the real Tau state tree recursively read-only. `legacy`
retains the historical ambient state view. The emergency
`TAU_EXTENSION_TAU_STATE_ACCESS=hidden|read_only|legacy` environment setting
forces every supervised extension for one newly started daemon; `tau attach`
rejects it and Tau removes it before child execution.

A persistent harness restores the exact `<state>/ext/<instance>` directory
read-write for that instance in restricted modes. This direct state directory
is also the `Configure.state_dir` path where the harness can provide persistent
state. Independently, extension-data RPC gives each instance its own writable
Session, User, Cache, and Secret scopes:

- Session: `<state>/sessions/<session>/ext/data/<instance>`; unavailable in an
  ephemeral session.
- User: `<state>/ext/<instance>`.
- Cache: the user cache directory at `tau/ext/<instance>`.
- Secret: `<state>/secrets/ext/<instance>`; unavailable to in-process
  extensions and never exposed as a direct path.

All extension-data RPC scopes are unavailable in a memory-only harness. These
scopes are per configured instance, not per executable or connection. The
restricted state view does not remove their authorized RPC access in a
persistent harness.

## Linux launch view

On Linux, every supervised extension starts in a fresh user namespace and mount
namespace. Tau maps the calling user, makes mount propagation private, and
installs the selected state view. When a state root exists, it stages a private
bind of real state to install that view. It then changes to the configured
working directory and executes the complete
configured `prefix ++ command ++ suffix` argv. It removes `TAU_SECRET_*` and
`TAU_EXTENSION_TAU_STATE_ACCESS` from the child environment but otherwise does
not present a sanitized environment.

When a state root exists, Tau masks the entire Tau secret root in every policy.
A persistent harness restores the per-instance direct-state bind after the
restricted state view. A persistent Provider, and only a persistent Provider,
additionally receives its selected
`<state>/providers/<instance>` tree as a recursive read-only bind.
That tree contains credential-free settings; tool extensions receive neither
this mount nor a settings snapshot. Provider debug captures do not create a
writable mount: they cross a dedicated bounded non-journaled protocol message,
and the harness derives and writes their session/instance path.

When it stages real state, the launcher then covers that temporary tree with an
empty read-only mount. This prevents access through the staging source after
the selected destination binds exist. The working directory must be canonical and
cannot be in masked Tau state; an unset `cwd` selects the harness process cwd.
The extension still receives its ordinary configured argv, stdin/stdout
protocol pipes, configured free-form `Configure.config`, instance name, optional
tool prefix, authorized startup secrets, and Provider settings snapshot where
applicable.

For `read_only`, Tau uses Linux 5.12 `mount_setattr` with `AT_RECURSIVE`;
it does not fall back to a nonrecursive remount that could leave nested mounts
writable. Namespace, ID-map, mount, cwd, or exec failures fail supervised
startup closed. Non-Linux Tau does not run a supervised extension with an
unmasked fallback. Memory-only harnesses force `hidden`, create no host state,
and mask an existing state root if one exists.

## Component-specific read-only mounts

The harness state policy applies to supervised extensions. `core-shell` adds a
separate command-level Linux hardening measure: for commands requesting a
read-only working directory, it clones and recursively read-only bind-mounts
that cwd before exec. The command otherwise sees the host filesystem as before.
If this Linux mount setup cannot run, `core-shell` reports the unavailable
read-only mount rather than silently claiming one; non-Linux has no such bind
mount. If the selected cwd is a Tau session or state subtree, that selected
subtree receives the same read-only bind behavior; `core-shell` has no separate
session-directory mount.

This is distinct from `tau_state_access`: it protects a shell command's chosen
cwd subtree, whereas `tau_state_access` controls the extension process's Tau
state view.

## What this does not defend against

Configured extensions, including Providers, remain trusted same-user programs.
The isolation does not defend against malicious extension code, authorized
secret misuse, `procfs` or `ptrace`, pre-opened descriptors, privileged
namespace escape, unrelated host filesystem data, or network access. It also
does not sandbox skills or model instructions. Treat externally sourced content
as untrusted separately from this local configured-extension boundary.

For configuration examples see `docs/extensions.md`. For exact secret storage
and launch rules, see `specs/SPEC-extension-secret-storage.md`; for the
component and protocol lifecycle, see
`crates/tau-harness/specs/SPEC-tau-harness-extension-lifecycle.md`.
