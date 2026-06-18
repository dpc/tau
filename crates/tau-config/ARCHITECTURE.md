# tau-config architecture

`tau-config` is the boundary between user-authored files/CLI overrides and the
rest of Tau. Config mistakes must fail explicitly with path/key context; do not
silently ignore unreadable files, invalid names, duplicate aliases, or malformed
overrides.

## Load order and layering

- Built-in `cli.yaml`, `cli-bindings.yaml`, and `harness.yaml` are the lowest
  precedence layers.
- User `cli.yaml` / `harness.yaml` are layered above built-ins.
- Sorted `*.yaml` / `*.yml` files from `cli.d/` and `harness.d/` are layered in
  lexical order above the base user file.
- `--harness-config KEY=VALUE` overrides are the highest-precedence harness
  config layers and must preserve command-line order.
- Config discovery is fallible: unreadable base paths, unreadable drop-in
  directories, bad directory entries, and non-directory `*.d` paths are explicit
  config errors.

`testing.yaml` is intentionally not part of the normal layered CLI/harness load
order. It is an optional standalone testing-only file used by development
helpers such as `tau dev tmux start` to decide whether provider profiles may be
copied into scratch state. Absence is distinct from an empty
`testing_providers: []` list so callers can warn differently, but both states
mean no provider access. Unknown fields fail closed, and provider entries are
validated as `ProviderName`s so path-like values never become filenames.
Unreadable, unstatable, or non-regular `testing.yaml` paths are explicit config
errors rather than missing files.

## Alias normalization

Legacy camelCase keys are accepted for compatibility, but aliases are normalized
per source layer before merging. A source that sets both a legacy alias and the
canonical key is invalid and must report both names. The same canonicalization
applies to YAML files and `--harness-config`, including YAML map values on the
right-hand side.

When adding or renaming a harness config field, update all alias handling paths
(file-layer normalization, CLI override canonicalization, serde aliases where
needed for direct patch parsing) and add regression coverage for both file and
CLI override forms.

`tool_policy.rules` is a keyed layered map. Rule names may contain dots (for
example `builtin.chatgpt-shell`), so dotted CLI overrides cannot naturally refer
to such rule keys; use whole-map override values for those rules unless an
escaping scheme is added. Rule aliases such as `enabled` must normalize inside
each keyed rule before source layers merge, otherwise a higher-precedence alias
can collide with a lower-precedence canonical key instead of overriding it.

## Harness role merging

Role metadata is merged through domain-specific logic rather than generic YAML
array replacement:

- `role_groups.<group>` defaults apply to all existing members of that group and
  to roles listed in the same layer.
- Per-role overrides are applied after group defaults.
- Prompt fragments are additive and de-duplicated.
- Patch fields distinguish absent, explicit `null`, and concrete values. `null`
  clears nullable/scalar fields; replacement lists can be cleared with `[]`.
  `tools` is a nullable replacement list: `tools: null` clears an inherited
  allow-list back to default behavior, while `tools: []` sets an explicit empty
  allow-list.
- Disabled roles are removed only after all file, drop-in, and CLI layers have
  been merged.

## Extension names and paths

Extension names come from harness config keys and may feed harness-owned state
paths and dotted CLI override paths. Valid names contain only ASCII letters,
digits, `_`, and `-`. Reject invalid names while loading harness settings, not
later at a consumer.

`ExtensionEntry::cwd` is presence-aware: an absent layer inherits lower
precedence config, a path sets cwd, and explicit `cwd: null` clears a lower-layer
cwd so the child inherits the harness process cwd.

Extension availability uses two separate fields. `enable` decides whether an
extension is desired at all; disabled entries should be inert for command,
secret, and spawn validation. `require` decides whether an enabled extension is
startup-critical; absence inherits lower layers/built-in defaults, and user-added
entries ultimately default to required. Both fields are ordinary layered config,
so file/drop-in/CLI override tests should cover parsing and precedence.

## Atomic writes

`atomic_write_following_symlink` follows a destination symlink and replaces its
target, preserving user-managed indirection. It writes to a randomized sibling
temp file, applies the resolved permissions at creation time on Unix and again
after open for exactness, removes temp files on post-create failures, renames
over the destination, and syncs the parent directory where supported.
