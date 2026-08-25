---
name: tau-self-knowledge-secrets
description: >
  Use this skill when the user asks how Tau handles extension secrets, including
  declarations, TAU_SECRET sources, provider credentials, Secret RPC, redaction,
  persistence, rotation, or their security limits.
advertise: false
---

# Tau secret handling

Tau treats configured extensions as trusted same-user executables. It constrains
secret delivery paths, but it does not contain an extension after Tau
authorizes it to receive a secret. See `SECURITY.md` and
`SPEC-extension-secret-storage` in the Tau source for the complete boundary.

## Declared startup secrets

An extension can declare named startup secrets in `harness.yaml`:

```yaml
extensions:
  example:
    secrets:
      api_token: {}
      optional_token:
        optional: true
```

The declaration authorizes only that extension instance. A resolved ordinary
declaration is sent point-to-point in its startup `Configure.secrets`; Tau does
not send the source path. Declaration names must be one nonempty source-name
component: ASCII letters, digits, `.`, `_`, or `-`, but not `.` or `..`.

Tau resolves each declaration from these sources:

1. `TAU_SECRET_<NAME>`, where `<NAME>` is lowercased with ASCII-only case
   conversion for lookup.
2. `<state_dir>/secrets/<lowercased-name>.yaml`.

The file is UTF-8 text, not parsed YAML despite its suffix. Tau first selects a
raw nonempty environment value over the file, then applies Rust `str::trim()`
to the selected value and treats an empty result as absent. A raw empty environment
value is not selected, so a nonempty trimmed file can supply the declaration.
A whitespace-only environment value is raw nonempty: it shadows the file and
then resolves as absent.

Tau scans environment keys with the exact `TAU_SECRET_` prefix, ASCII-lowercases
the suffix, and validates the result with the same source-name grammar. Two
raw-nonempty matching variables that normalize to one name cause startup/setup
resolution to fail, even if one becomes empty after trimming; Tau does not
select one. Invalid matching names also fail. At harness startup these are
global snapshot errors: they abort startup before Tau applies an extension's
`require` setting.

The prefix match uses the operating system's native key representation rather
than lossy Unicode conversion. Tau ignores unrelated non-Unicode entries. A
matching suffix or value that cannot enter the UTF-8 named-secret schema returns
a redacted source error.

A missing `optional: true` declaration is omitted from `Configure.secrets`.
A missing required declaration fails a required extension's startup. For an
enabled `require: false` extension, a failure while resolving one of its
declarations skips the whole extension and emits a mandatory startup warning
instead.

### Environment lifetime

At harness startup, Tau snapshots every matching `TAU_SECRET_*` variable and
removes those variables from its own environment before it spawns extensions.
It does this even if later validation reports an invalid name or collision; a
supervised child also has matching variables removed from its inherited
environment. The snapshot supplies that startup only, so changing the shell
environment after launch has no effect.

Provider setup uses the same resolver with a retained environment instead:
`tau provider add` and related setup operations do not consume the caller's
`TAU_SECRET_*` variables. Start a new harness after changing a named source
that must be materialized at startup.

## Secret RPC and durable storage

Extensions use the harness-mediated Secret RPC (`ExtensionDataScope::Secret`)
for durable credentials, rather than receiving a host secret directory. Tau
keys this scope by configured extension instance at
`$XDG_STATE_HOME/tau/secrets/ext/<instance>`. Renaming or duplicating an
instance creates a different scope.

The RPC permits whole-file read, write, compare-and-swap, create, delete,
rename, and direct-child list operations with the existing safe relative-path
grammar; append is denied. Secret files are at most 1 MiB, a list has at most
4,096 entries, files use mode `0600`, directories use `0700`, and successful
mutations atomically and synchronously publish the file and namespace.
Compare-and-swap uses a BLAKE3 generation of complete contents, so a losing
writer receives a typed mismatch rather than overwriting the winner.

Secret RPC is unavailable to memory-only harnesses and in-process test
extensions. It is the durable storage path across sessions, harness restarts,
and supervised respawns.

## Provider credentials and rotation

Provider settings are credential-free files under
`$XDG_STATE_HOME/tau/providers/<instance>/<provider>.json`; the
Provider gets one bounded immutable `Configure.settings_files` snapshot at
startup. Typed credentials live in that Provider instance's Secret scope.

An API-key settings record can name one current or future secret source without
serializing its value. When the exact targeted-instance declaration exists, it
is materialization authority and is excluded from `Configure.secrets`; the name
alone grants no authority. Setup eagerly resolves a selected configured name and must
succeed before it writes settings last as activation. An explicit new-name
forward reference instead writes only credential-free settings and stays
disabled until its declaration and value are deployed. Persistent harness
startup resolves valid bindings and materializes the resulting typed API-key
record in Secret storage.
At that persistent startup only, an unavailable or undeclared bound source
replaces the old record with an empty API-key record and suppresses the profile;
source I/O or decoding failures do not overwrite an older record. Memory-only
startup snapshots settings but does no source resolution or materialization.
Changing a named source takes effect when setup writes a new record or a new
persistent harness startup rematerializes it; a running Provider does not
reread the named source.

At prompt time, the Provider reads credentials through Secret RPC, so it reloads
the stored record for rotation. OAuth refresh uses compare-and-swap to replace
the complete typed record; a concurrent loser reloads the winning generation
instead of reusing its stale token. Immediately materialized provider setup
writes secret first and settings last; explicit deferred setup writes only
settings. Removal deletes settings first and secret last. There is no cross-file
transaction or orphan collector.

## Redaction and limits

Tau keeps Secret RPC request/result payloads out of events, journals, logs,
generic debug output, errors, and OAuth diagnostics. Diagnostics may expose an
operation kind, byte count, sanitized relative name, and typed failure, but not
credential bytes or host secret paths. Provider settings exclude credential
values.

This is not a guarantee that secrets cannot leave a trusted extension. An
extension can use a secret that Tau intentionally delivered, and it can access
unrelated host/network resources within the configured-extension trust boundary.
Provider-specific request capture, if an operator enables it, can contain
upstream credentials or requests. Do not put secrets in prompts, extension
configuration, tool arguments, or ordinary logs.

For operational provider setup see `tau-self-knowledge-config` and
`tau-self-knowledge-ext-provider-builtin`. For the authoritative cross-component
contract, inspect `specs/SPEC-extension-secret-storage.md`.
