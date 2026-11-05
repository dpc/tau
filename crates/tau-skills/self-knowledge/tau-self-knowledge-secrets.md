---
name: tau-self-knowledge-secrets
description: Use when configuring or auditing Tau extension secrets, including declarations, TAU_SECRET sources, provider credentials, Secret RPC, redaction, persistence, rotation, or security limits.
advertise: false
---

# Tau secret handling

Tau treats configured extensions as trusted same-user executables. It constrains
secret delivery paths, but it does not contain an extension after Tau
authorizes it to receive a secret. See `SECURITY.md` and
`SPEC-extension-secret-storage` in the Tau source for the complete boundary.

## Declared startup secrets

`<state_dir>/secrets/foo_api_key.yaml` holds the value:

```text
the-api-key-value
```

`harness.yaml` declares who receives it and how that extension uses its name:

```yaml
extensions:
  example:
    secrets:
      foo_api_key: {}
    config:
      foo_api_key_secret: foo_api_key
```

- The backing file (or `TAU_SECRET_FOO_API_KEY`) owns the value.
- `extensions.example.secrets.foo_api_key` authorizes Tau to deliver that named
  value to `example` in `Configure.secrets` during its startup handshake.
- `config.foo_api_key_secret` is defined by `example`; it tells that extension
  which delivered name it should use.

Config references do not authorize delivery; undeclared names are not delivered.

An extension can declare named startup secrets in `harness.yaml`:

```yaml
extensions:
  example:
    secrets:
      api_token: {}
      optional_token:
        optional: true
```

Names must be one nonempty component of ASCII letters, digits, `.`, `_`, or `-`,
except `.` and `..`. Tau sends ordinary resolved declarations to that instance's
`Configure.secrets`, without source paths.

Tau resolves each declaration from these sources:

1. `TAU_SECRET_<NAME>`, where `<NAME>` is lowercased with ASCII-only case
   conversion for lookup.
2. `<state_dir>/secrets/<lowercased-name>.yaml`.

Files contain UTF-8 text, not YAML. A raw nonempty environment value wins over
the file; Tau then applies Rust `str::trim()`. An empty variable allows file
fallback, but whitespace-only content shadows the file and resolves as absent.

The prefix `TAU_SECRET_` is exact; suffix matching is ASCII case-insensitive.
Invalid matching names, non-UTF-8 names/values, or two raw-nonempty variables
normalizing to one name fail resolution. These global snapshot errors abort
startup regardless of extension `require`. Unrelated non-Unicode entries are ignored.

A missing `optional: true` secret is omitted. Missing required secrets fail a
required extension's startup; declaration-resolution failure for `require: false`
skips the whole extension with a startup warning.

### Environment lifetime

At startup Tau snapshots and removes all `TAU_SECRET_*` variables before spawning
extensions, even if validation later fails. It also removes them from supervised
children. Changing the shell environment after launch has no effect.

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

Secret RPC supports whole-file read/write, compare-and-swap, create, delete,
rename, and direct-child list with safe relative paths; no append. Limits:
1 MiB per file, 4,096 list entries, `0600` files, `0700` directories.
Mutations publish atomically and synchronously. Compare-and-swap uses a BLAKE3
content generation; losing writers receive a typed mismatch.

Secret RPC is unavailable to memory-only harnesses and in-process test
extensions. It is the durable storage path across sessions, harness restarts,
and supervised respawns.

## Provider credentials and rotation

Provider settings are credential-free files under
`$XDG_STATE_HOME/tau/providers/<instance>/<provider>.json`; the
Provider gets one bounded immutable `Configure.settings_files` snapshot at
startup. Typed credentials live in that Provider instance's Secret scope.

API-key settings can name a secret source without storing its value. The exact
target-instance declaration authorizes materialization and is then excluded from
`Configure.secrets`. Setup resolves configured names before activating settings;
a new-name forward reference stays disabled until its declaration/value exist.
Persistent startup materializes bindings into typed Secret records. An unavailable
or undeclared source clears the old API-key record and suppresses the profile;
I/O/decoding errors leave the old record intact. Memory-only startup does not
resolve or materialize sources. To change a named source, rerun setup or start a
new persistent harness; running Providers do not reread it.

Providers reload stored credentials through Secret RPC at prompt time.
OAuth refresh uses compare-and-swap; a loser reloads the winning record.
Setup writes secrets before settings; deferred setup writes only settings.
Removal deletes settings before secrets. There is no cross-file transaction or
orphan collector.

## Redaction and limits

Secret RPC payloads stay out of events, journals, logs, errors, debug output,
and OAuth diagnostics. Diagnostics may show operation kind, byte count,
sanitized relative name, and failure type, not credentials or host secret paths.
Trusted extensions can still use delivered secrets and access host/network
resources. Provider request captures can contain upstream credentials or requests.
Keep secrets out of prompts, extension config, tool arguments, and ordinary logs.

For operational provider setup see `tau-self-knowledge-config` and
`tau-self-knowledge-ext-provider-builtin`. For the authoritative cross-component
contract, inspect `specs/SPEC-extension-secret-storage.md`.
