# SPEC-extension-secret-storage: Harness-mediated extension credentials

## Record justification

Credential setup, protocol access, harness storage and launch isolation, provider refresh, and developer scratch copying span the CLI, protocol, harness, and provider extension, so no single implementation artifact can own the contract coherently.


## Scope and identity

`ExtensionDataScope::Secret` is durable across sessions, ephemeral sessions, harness restarts, and supervised respawns. The harness keys it by the stable configured extension instance name and stores it below `$XDG_STATE_HOME/tau/secrets/ext/<instance>`. Renaming or duplicating a configured entry creates an independent scope. Memory-only harnesses and in-process test extensions have no Secret authority.

The scope accepts the existing relative path grammar and whole-file read, write, compare-and-swap, create, delete, rename, and direct-child list operations. It denies append. Files are limited to 1 MiB and one list scan to 4,096 entries; there is no aggregate quota. Directories and files use `0700` and `0600`. Successful mutation means atomic, synchronously durable file and namespace publication. Operations never follow symlink components or special files.

Compare-and-swap names the BLAKE3 generation of the complete current contents. The harness serializes comparison and atomic replacement across processes sharing the state root. A mismatch is typed and never overwrites the winning generation.


## Diagnostics and launch boundary

Secret request and result payloads remain absent from events, journals, logs, generic debug formatting, errors, and OAuth diagnostics. Diagnostics may expose operation kind, byte count, sanitized relative identity, and typed failure only; they never expose credential bytes or host secret paths.

Every supervised extension starts inside a harness-owned outer Linux user and mount namespace. The launcher makes propagation private, masks the whole Tau secret root before applying configured cwd, closes setup authority, and then executes the complete configured prefix, command, and suffix. Configured Provider instances additionally receive only their selected provider-settings tree mounted read-only. Tool instances receive no provider-settings mount. Any namespace, mapping, mount, cwd, or exec failure fails extension startup. Non-Linux systems have no unmasked fallback.

A persistent harness creates the private mask targets before spawn. A memory-only harness creates no host state: it masks the existing Tau state root, or skips that vacuous mount when the state root does not exist. Configured cwd at or below the masked state root is invalid. In-process extensions remain test-only and cannot request Secret.

The namespace setup runs in a direct pre-exec hook. The harness prepares every owned string, path, identity map, and existence decision before `fork`; the child performs only allocation-free libc system calls and returns an OS error through the standard spawn error pipe. This avoids post-fork formatting and locks while preserving exact configured argv in embedded harnesses, where re-executing the current binary cannot assume a private launcher-marker dispatcher.

This mount mask is defense in depth under Tau's trusted configured same-UID executable boundary. It does not claim containment from a malicious same-UID process, credential misuse after authorized RPC delivery, proc/ptrace attacks, privileged namespace escape, or unrelated filesystem and network access.


## Provider registration and runtime

The CLI owns credential-free provider settings below `$XDG_STATE_HOME/tau/provider-settings/<instance>/<provider>.json`. Each persistent configured Provider instance receives one immutable, bounded `Configure.settings_files` startup snapshot, and the launcher exposes only its selected settings tree read-only. Tool and memory-only instances receive neither the snapshot nor the mount. Settings, endpoint, model, and compatibility changes therefore require Provider restart.

Version-zero credential records use a typed `kind`: `chatgpt_oauth` contains complete access token, refresh token, expiry, and account id; `api_key` contains the complete API-key value. Provider settings contain only a typed credential reference into `providers/<provider>/`.

Provider setup targets the exact enabled built-in provider extension instance. Omission selects only `provider-builtin`; renamed or duplicate instances require `--extension`, and missing, disabled, wrong-role, or wrong-component targets fail rather than being guessed.

Add writes the secret first and settings last. Remove deletes settings first and the secret last. There is no cross-file transaction or automatic orphan collector. Runtime reads credentials through Secret RPC before use and reloads them for prompt-time rotation. OAuth refresh replaces only its complete typed secret with compare-and-swap; a concurrent loser reloads the winning generation instead of reusing a rotated token.

Legacy `auth.d`, `ProviderStore`, `AuthFile`, inline API keys, and mixed settings/credential records have no runtime reader or migration. Users re-register providers and manually remove legacy files.


## Developer scratch access

`tau dev tmux` grants provider access only to explicit `(extension instance, provider)` pairs. It copies exactly the selected credential-free settings file and provider secret subtree into private scratch roots. It never mounts or reads credentials in place from the real state tree.
