# ARCH-tau-config: tau-config architecture

Extension availability is layered in this order: built-in defaults, harness
configuration/drop-ins, the selected profile, and ordered `--harness-config` layers,
`TAU_ENABLE_EXTENSIONS` named enables, then extension CLI overrides in argv order.

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
- A selected `profiles.<name>` patch loads after built-in/user/drop-in files and
  before `--harness-config` layers. `--profile <name>` wins over `TAU_PROFILE`;
  an unknown selected name is an explicit error.
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

## CLI runtime-state overlay

`CliSettings` comes from layered `cli.yaml` config and provides the default UI
state for a process. Persisted `<state_dir>/cli.json` is a partial runtime patch
written by `:set`; when it is loaded, present fields override the `CliSettings`
derived defaults and missing fields keep those defaults. This preserves new or
user-configured defaults for existing state files from older Tau versions.

Prompt completion maps configure word-prefix completers such as `@`, `./`, and
`/`; the shipped `/` rule uses `complete_path` for absolute and token-level
paths. First-non-whitespace `:` command mode is intrinsic to the terminal and
takes precedence over every configured completion rule, so user config cannot
shadow command routing or inject unrelated candidates into command arguments.

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

`session_retention_days` controls whole inactive session-directory cleanup.
`diagnostic_retention_days` independently controls best-effort startup
cleanup of non-authoritative session JSONL and provider request/response
captures and defaults to fourteen days; zero disables only that shared
diagnostic cleanup.

`tau-config::provider_debug_capture` owns the dependency-neutral provider
capture basename contract shared by provider writers and harness retention:
canonical decimal microsecond timestamp, validated `AgentPromptId`, one valid
transport/direction class, and exact legacy `.json` or current `.json.zst`
extension.

The recognized current classes include
`responses-attempt-failure.json.zst`. It uses the same eligibility, path
validation, compression, best-effort failure behavior, and
`diagnostic_retention_days` cleanup as request/response captures; it has no
separate retention knob.

`tool_policy.rules` is a keyed layered map. Rule names may contain dots (for
example `builtin.chatgpt-shell`), so dotted CLI overrides cannot naturally refer
to such rule keys; use whole-map override values for those rules unless an
escaping scheme is added. Rule aliases such as `enabled` must normalize inside
each keyed rule before source layers merge, otherwise a higher-precedence alias
can collide with a lower-precedence canonical key instead of overriding it.

## Harness role merging

Role metadata is merged through domain-specific logic rather than generic YAML
array replacement:

- Role sources retain their normal order (built-ins, files/drop-ins, selected
  profile, then ordered `--harness-config` layers) within each scope. After
  collecting them, `agents` defaults (`enable`, `model`, `effort`, `verbosity`,
  `thinking_summary`, `service_tier`, and `compaction`) apply to every role,
  then role-group defaults, then per-role overrides. `agents.enable` defaults
  to true. A narrow role patch
  therefore remains effective over a broader group/default patch from a later
  source.
- `agents.role_groups.<group>` defaults apply to every effective member of that
  group, including roles introduced by a later source.
- Per-role overrides are applied after group defaults.
- Role `order` is ordinary role metadata: lower values sort first within a
  group, with role name as the stable tie-breaker.
- Prompt fragments and required skill names are additive and de-duplicated.
- Named `context_size_alerts` merge field-by-field from agent-global defaults
  through role-group defaults to role overrides. Each inherited alert can
  therefore be customized or disabled without repeating its threshold and
  message.
- Patch fields distinguish absent, explicit `null`, and concrete values. `null`
  clears nullable/scalar fields; replacement lists can be cleared with `[]`.
  `tools` is a nullable replacement list: `tools: null` clears an inherited
  allow-list back to default behavior, while `tools: []` sets an explicit empty
  allow-list.
- Disabled roles are removed only after all file, drop-in, and CLI layers have
  been merged.
- `inter_session_receiver` and `inter_session_auto_start` are ordinary scalar
  role fields. Group defaults and role overrides can grant them across any
  number of groups. Auto-start without effective receiver capability is invalid.

## Selectable configuration profiles

`profiles` is a raw configuration-only map, not part of effective
`HarnessSettings`. A selected profile supports agent defaults,
agent/global role metadata, role groups and roles, plus `extensions.<name>.enable`
for a built-in or base-configured extension. This explicit subset avoids a second
universal recursive merge schema. Its role patches replay after base file layers,
so relative values resolve against base settings, and before CLI role or
`--harness-config` patches.

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

`ExtensionEntry::tool_prefix` is presence-aware: absence inherits, explicit
null clears, and a validated segmented ASCII string sets the immutable
per-instance structural tool prefix. `toolPrefix` is normalized as a legacy
camel-case alias in file and CLI layers. It is independent of the argv-wrapper
`prefix`. See
[SPEC-extension-tool-prefixes](../../../specs/SPEC-extension-tool-prefixes.md).

## Atomic writes

`atomic_write_following_symlink` follows a destination symlink and replaces its
target, preserving user-managed indirection. It writes to a randomized sibling
temp file, applies the resolved permissions at creation time on Unix and again
after open for exactness, removes temp files on post-create failures, renames
over the destination, and syncs the parent directory where supported.
