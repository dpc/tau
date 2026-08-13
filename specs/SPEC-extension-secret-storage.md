# SPEC-extension-secret-storage: Harness-mediated extension credentials

## Record justification

Credential setup, protocol access, harness storage and launch isolation, provider refresh, and developer scratch copying span the CLI, protocol, harness, and provider extension, so no single implementation artifact can own the contract coherently.


## Scope and identity

`ExtensionDataScope::Secret` is durable across sessions, ephemeral sessions, harness restarts, and supervised respawns. The harness keys it by the stable configured extension instance name and stores it below `$XDG_STATE_HOME/tau/secrets/ext/<instance>`. Renaming or duplicating a configured entry creates an independent scope. Memory-only harnesses and in-process test extensions have no Secret authority.

The scope accepts the existing relative path grammar and whole-file read, write, compare-and-swap, create, delete, rename, and direct-child list operations. It denies append. Files are limited to 1 MiB and one list scan to 4,096 entries; there is no aggregate quota. Directories and files use `0700` and `0600`. Successful mutation means atomic, synchronously durable file and namespace publication. Operations never follow symlink components or special files.

Compare-and-swap names the BLAKE3 generation of the complete current contents. The harness serializes comparison and atomic replacement across processes sharing the state root. A mismatch is typed and never overwrites the winning generation.


## Diagnostics and launch boundary

Secret request and result payloads remain absent from events, journals, logs, generic debug formatting, errors, and OAuth diagnostics. Diagnostics may expose operation kind, byte count, sanitized relative identity, and typed failure only; they never expose credential bytes or host secret paths.

Every supervised extension starts inside a harness-owned outer Linux user and mount namespace. The launcher makes propagation private, masks the whole Tau secret root before applying configured cwd, closes setup authority, and then executes the complete configured prefix, command, and suffix. `tau_state_access` defaults to `read_only`, which presents the real state tree recursively read-only; `hidden` presents an empty read-only state tree; `legacy` retains the historical ambient view. In both restricted modes the exact persistent `<state>/ext/<instance>` tree is restored read-write. A Provider additionally receives its selected settings tree read-only. Provider debug captures cross a dedicated bounded non-journaled protocol message as opaque zstd bytes; the harness derives and writes the durable session/instance path without exposing another writable mount. Tool instances receive no Provider exception. Secrets remain masked in every mode. Any namespace, mapping, mount, cwd, or exec failure fails extension startup. Non-Linux systems have no unmasked fallback.
Independently of state access, supervised components receive an empty read-only
view of the Tau harness runtime socket directory. Per-component
`tau_runtime_socket_access: legacy` restores the historical ambient socket view
without changing state or secret access.

Recursive read-only presentation uses Linux 5.12 `mount_setattr` with
`AT_RECURSIVE`. Tau does not fall back to a non-recursive remount on older
kernels because a nested mount could remain writable; unsupported or failed
`mount_setattr` fails supervised extension startup closed.

A persistent harness creates the private mask targets before spawn. A memory-only harness creates no host state: it masks the existing Tau state root, or skips that vacuous mount when the state root does not exist. Configured cwd at or below the masked state root is invalid. In-process extensions remain test-only and cannot request Secret.

The namespace setup runs in a direct pre-exec hook. The harness prepares every owned string, path, identity map, and existence decision before `fork`; the child performs only allocation-free libc system calls and returns an OS error through the standard spawn error pipe. This avoids post-fork formatting and locks while preserving exact configured argv in embedded harnesses, where re-executing the current binary cannot assume a private launcher-marker dispatcher.

This mount mask is defense in depth under Tau's trusted configured same-UID executable boundary. It does not claim containment from a malicious same-UID process, credential misuse after authorized RPC delivery, proc/ptrace attacks, privileged namespace escape, or unrelated filesystem and network access.


## Provider registration and runtime

Credential-free provider profiles come from the disjoint union of read-only
`$XDG_CONFIG_HOME/tau/providers/<instance>/<provider>.json` and mutable
`$XDG_STATE_HOME/tau/providers/<instance>/<provider>.json`. Every profile has
exactly one source; a cross-layer duplicate is an error even when its bytes are
identical. Startup follows configuration leaf symlinks that resolve to bounded
regular files, including targets outside the canonical config instance root in a
read-only Nix store. Broken links, directories, special files, invalid names, and
oversized files fail closed. Mutable state retains no-symlink and
private-directory restrictions.

Each configured Provider instance receives one immutable, bounded merged
`Configure.settings_files` startup snapshot. A persistent Provider also receives
an ephemeral harness-owned read-only materialization of those exact bytes at its
settings mount; a memory-only preview receives the snapshot without that mount.
Tool instances receive neither surface. Profile changes require a full harness
restart; extension-only respawn does not rescan either source.
Harness startup, provider setup inspection, and the tmux helper all enforce the
same limits: 4,096 profiles per instance, 1 MiB per profile, and a merged byte
limit below the protocol frame maximum. Each reader validates the opened
descriptor as a regular file; on Unix a raced FIFO or special-file target is
opened nonblocking and rejected.

Version-zero credential records use a typed `kind`: `chatgpt_oauth` contains
complete access token, refresh token, expiry, and account id; `api_key` contains
the complete API-key value. Authenticated provider settings contain a typed
credential reference into `providers/<provider>/`. Supported local-compatible
profiles may instead select the exact `credential: {"kind":"none"}` form. That
form authorizes unauthenticated requests without creating or reading a Secret
record. Omitting `credential`, adding fields to `kind: "none"`, or selecting it
for a provider kind that requires authentication remains invalid.

Provider setup targets the exact enabled built-in provider extension instance.
Resolution preserves a typed Tau-component identity before flattening wrapped
argv; an explicit replacement command cannot claim that identity merely by
copying component suffix tokens. Omission selects only `provider-builtin`;
renamed or duplicate instances require `--extension`, and missing, disabled,
wrong-role, or wrong-component targets fail rather than being guessed.

Authenticated add writes the secret first and settings last. Keyless add writes
only settings. Remove deletes settings first and any secret last. There is no
cross-file transaction or automatic orphan collector. Runtime reads referenced
credentials through Secret RPC before use and reloads them for prompt-time
rotation. OAuth refresh replaces only its complete typed secret with
compare-and-swap; a concurrent loser reloads the winning generation instead of
reusing a rotated token.

Login hydrates or refreshes an existing authenticated profile by replacing only
its host-local typed Secret record. It neither writes settings nor creates a
profile in the other source, and it aborts if the profile source or bytes change
while authentication is in progress. Config profile leaf symlinks remain
untouched. A cross-source duplicate remains an error.

An API-key settings reference may bind the canonical record to one declared
named harness secret without serializing its value. The shared closed parser
rejects malformed sources, OAuth bindings, noncanonical paths, unknown fields,
and unsafe or whitespace names for setup, startup, and provider runtime alike.
The bound declaration is materialization authority and its value is omitted from
`Configure.secrets`.

Setup defaults to mutable state and can explicitly target config or emit canonical
JSON to standard output for dotfiles deployment. Bare ChatGPT add offers login
instead when a config-owned profile lacks current OAuth credentials; outside an
interactive terminal it reports the exact login command without starting OAuth.
List reports the same command beside absent or expired ChatGPT authentication,
including the selected extension instance when it is not the default, and show
reports profile source. Removal infers a unique source or accepts an explicit
source. Setup, login, and removal lock only the Tau-private mutable
providers instance directory, then the Secret-scope directory. Persistent startup may create an empty private instance
directory as lifecycle metadata for a config-only deployment; it never locks or
writes config and never imports a profile. Memory-only startup locks that
directory only when it already exists and otherwise performs a non-transactional
read-only config snapshot without creating host state. Stored-credential setup
resolves the selected declaration while holding the instance lock, writes the
complete typed secret, and writes settings last as activation. Keyless setup
writes settings without opening Secret scope. Removal deletes settings first as deactivation, then removes
closed credential slots. Startup takes the same locks in the same order: under
the instance lock it captures one bounded disjoint-union generation, resolves every
valid named binding from the one-shot source snapshot, and under one Secret lock
publishes the resulting typed records. Configure later uses that retained
settings generation instead of rereading the directory.

The built-in provider validates every filename and full profile in that
Configure generation before retaining parsed settings or publishing models. One
invalid entry rejects the complete generation through a bounded, redacted
`ConfigError`; the extension publishes neither models nor `Ready`, and the
harness applies its existing required/optional startup and replayable-notice
policy. Rejection does not mutate or migrate the persisted settings.

An actually unavailable or undeclared bound source replaces the old materialization with
an empty API-key record, suppresses that profile, and produces a mandatory
source-name-only warning. Direct-entry records and explicit keyless settings
have no binding and are never changed by startup refresh. Source I/O and decoding failures do not
overwrite an older record: they fail a required provider or skip an optional
provider with a redacted warning. Settings snapshot/materialization failures
follow the same required/optional policy. Memory-only harnesses take the same
immutable credential-free settings snapshot for model advertisement, but
perform no source resolution or credential materialization and omit all
declaration values owned by the typed built-in provider component.

The retired `$XDG_STATE_HOME/tau/provider-settings/` tree is completely
undiscovered: there is no detection, fallback, migration, or warning. Users
manually move only credential-free profile JSON into one new `providers/` source
and leave Secret records untouched. Legacy `auth.d`, `ProviderStore`, `AuthFile`,
inline API keys, and mixed settings/credential records likewise have no runtime
reader or migration.


## Developer scratch access

`tau dev tmux` grants provider access only to explicit `(extension instance,
provider)` pairs. It copies exactly the selected credential-free settings file
and, for a stored-credential profile, its provider Secret subtree into private
scratch roots. An explicit keyless profile has no Secret subtree to copy. The
helper never mounts or reads credentials in place from the real state tree.
