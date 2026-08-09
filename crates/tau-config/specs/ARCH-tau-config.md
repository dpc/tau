# ARCH-tau-config: tau-config architecture

`tau_state_access` selects `hidden`, `read_only`, or `legacy` for supervised
extensions, and an extension entry can override it with the same field. The
shipped default is `hidden`;
`TAU_EXTENSION_TAU_STATE_ACCESS` accepts only those exact lowercase values and
is a final process-wide force after all configuration layers. The CLI rejects
that force on attach because it cannot change an existing daemon.

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
  when neither selects a name, top-level base-layer `default_profile` selects a
  fallback profile. An absent or null `default_profile` sources no profile. An
  unknown selected name is an explicit error.
- Config discovery is fallible: unreadable base paths, unreadable drop-in
  directories, bad directory entries, and non-directory `*.d` paths are explicit
  config errors.

`testing.yaml` is intentionally not part of the normal layered CLI/harness load
order. It is an optional standalone testing-only file used by development
helpers such as `tau dev tmux start` to decide whether providers may be
copied into scratch state. Absence is distinct from an empty
`testing_providers: []` list so callers can warn differently, but both states
mean no provider access. Unknown fields fail closed, and each entry validates an
exact `ExtensionName`/`ProviderName` pair so path-like values never become
filenames.
Unreadable, unstatable, or non-regular `testing.yaml` paths are explicit config
errors rather than missing files.

Provider profile path helpers keep portable read-only config and mutable state
under the same instance-qualified `providers/<instance>/<provider>.json` shape.
Callers disjoint-union the two sources and reject duplicate profile names. Config
leaf symlinks may resolve to bounded regular files outside the canonical config
instance root, including read-only Nix-store targets; unusable targets fail
closed. State lifecycle locks and writes never follow symlinks. Cooperative
startup/setup serialization locks only Tau-private mutable state; persistent
config-only startup may create an empty private lifecycle directory, while
memory-only diagnostics never create host state.
The shared descriptor reader enforces 1 MiB per profile and validates regular
file type after opening. Unix opens are nonblocking and mutable-state reads add
`O_NOFOLLOW`. Callers enforce the shared 4,096-profile and merged snapshot byte
limits across their complete selected source set.

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

`wait_timeout_minimum_minutes` and `wait_timeout_maximum_minutes` bound the
effective activating-input `wait({"timeout_minutes": N})` deadline. They are
inclusive positive whole-minute values, default to five and 1,440 respectively,
and reject an inverted range during configuration loading. The maximum cannot
exceed 65,535 minutes because the persisted wait registration represents its
effective timeout as `u16`. They do not affect argument-free or exact
background-result waits.

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

`tool_policy.default_shell_tool_style` selects `codex`, `edit`, or `replace`
before rules and role controls run. Missing, null, or whitespace-only values
reset to the selected model default; any other nonblank value is a config error.

## Harness role merging

Role metadata is merged through domain-specific logic rather than generic YAML
array replacement:

- Role sources retain their normal order (built-ins, files/drop-ins, selected
  profile, then ordered `--harness-config` layers) within each scope. After
  collecting them, `agents` defaults (`enable`, `visible`, `model`, `effort`, `verbosity`,
  `thinking_summary`, `service_tier`, and `compaction`) apply to every role,
  then role-group defaults, then per-role overrides. `agents.enable` defaults
  to true, as does `agents.visible`. A narrow role patch
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
- `visible` is ordinary inherited role metadata. Effective `false` suppresses
  only the built-in available-sub-task-role prompt catalog; it neither removes a
  role nor changes its authorization, diagnostics, UI lists, or other role
  surfaces.

## Selectable configuration profiles

`profiles` is a raw configuration-only map, not part of effective
`HarnessSettings`. A selected profile supports `agents.default_role`, agent
provider defaults, agent/global role metadata, role groups and roles, plus
`extensions.<name>.enable` and arbitrary `extensions.<name>.config` for a
built-in or base-configured extension. Extension config objects merge
recursively; arrays, scalars, nested nulls, and type mismatches replace lower
precedence values, and no deletion sentinel exists. A top-level extension
`config: null` retains its existing absent/no-op compatibility. Role and
extension patches replay after base file layers, so relative values resolve
against base settings, and before CLI role or `--harness-config` patches.

`default_profile` is base selection configuration, evaluated from built-in,
user, and ordered `harness.d` layers before profile loading. A later null value
clears an earlier fallback. It is not read from a profile. Selecting a named
profile does not also apply the fallback profile; profiles remain independent
patches over the base layers.

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

`ExtensionEntry::startup_timeout_seconds` is likewise an ordinary layered scalar:
absence inherits a built-in value or the general two-second default, while a
configured integer from one through 3,600 replaces it. Resolution rejects values
outside that inclusive range. The harness uses the resolved value for the
extension's initial readiness deadline as specified by
[SPEC-tau-harness-extension-lifecycle](../../tau-harness/specs/SPEC-tau-harness-extension-lifecycle.md).

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

## Provider credential configuration

`tau-config` owns the dependency-neutral closed provider credential-selection
schema: either a canonical stored reference or the exact explicit keyless
marker. It also owns canonical slot paths, named-secret source resolution, and
the nofollow per-instance providers lifecycle lock. The built-in provider owns
which profile kinds may select keyless operation. Provider setup, harness
startup, and provider runtime share the selection parser and resolver rather
than interpreting credential authority independently. Harness source capture removes one-shot
environment variables before child spawn; setup retains them. Both modes share
normalization, collision, environment-before-file precedence, trimming,
optionality, UTF-8 failure, and safe-name rules.
`BuiltinComponentIdentity` separately preserves Tau-owned component authority
through argv wrapping without deriving authority from flattened executable
arguments.
