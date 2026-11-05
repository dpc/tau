---
name: tau-self-knowledge-isolation
description: Use when assessing Tau extension isolation or hardening, including tau_state_access, Linux namespaces, state mounts, provider exceptions, filesystem visibility, or the trust boundary.
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
tau_state_access: read_only
extensions:
  diagnostics:
    tau_state_access: hidden
```

`read_only` is the persistent-harness default and presents the real Tau state
tree recursively read-only. `hidden` presents an empty read-only Tau state
tree. Memory-only harnesses always force `hidden`, create no host state, and
mask an existing state root if one exists. The emergency
`TAU_EXTENSION_TAU_STATE_ACCESS=hidden|read_only` environment setting
forces every supervised extension for one newly started persistent daemon. A
memory-only harness still forces `hidden` afterward. `tau attach` rejects the
setting and Tau removes it before child execution.

A persistent harness restores the exact `<state>/ext/<instance>` directory
read-write for that instance in both modes. This direct state directory
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

On Linux, Tau starts each supervised extension in fresh user/mount namespaces,
maps the calling user, makes mount propagation private, installs the state view,
changes cwd, and executes `prefix ++ command ++ suffix`. It removes
`TAU_SECRET_*` and `TAU_EXTENSION_TAU_STATE_ACCESS`; the rest of the environment
is not sanitized.

Both modes mask the entire secret root. Only persistent Providers also receive
`<state>/providers/<instance>` as a recursive read-only bind of credential-free
settings; tool extensions receive neither this mount nor a settings snapshot.
Providers send debug captures through a bounded non-journaled protocol message
for the harness to write, not through a writable mount.

Tau masks its temporary staging bind after installing destination mounts.
The working directory must be canonical and outside masked state; unset `cwd`
uses the harness cwd. Extensions still receive their configured argv, protocol
pipes, `Configure.config`, instance name, optional tool prefix, authorized
secrets, and applicable Provider settings snapshot.

For `read_only`, Tau uses Linux 5.12 `mount_setattr` with `AT_RECURSIVE`;
it does not fall back to a nonrecursive remount that could leave nested mounts
writable. Namespace, ID-map, mount, cwd, or exec failures fail supervised
startup closed. Non-Linux Tau does not use an unmasked fallback.

## Component-specific read-only mounts

`core-shell` separately protects commands requesting a read-only cwd with a
recursive read-only bind on Linux. Other host files remain visible. Linux setup
failure reports the mount unavailable; non-Linux has no such mount.
This protects only the selected cwd, including a selected Tau state/session
subtree; there is no separate session-directory mount. `tau_state_access`
instead controls the extension process's Tau state view.

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
