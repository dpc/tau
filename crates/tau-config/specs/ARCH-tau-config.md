# ARCH-tau-config: tau-config architecture

`tau_state_access` selects `hidden`, `read_only`, or `legacy` for supervised
extensions, and an extension entry can override it with the same field. A
selected profile can replace this global default before command-line layers.
The shipped default is `read_only`;
`TAU_EXTENSION_TAU_STATE_ACCESS` accepts only those exact lowercase values and
is a final process-wide force after all configuration layers. The CLI rejects
that force on attach because it cannot change an existing daemon.
Each supervised component independently defaults
`tau_runtime_socket_access` to `hidden`; only the explicit `legacy` value
restores its ambient view of Tau harness runtime sockets.

Extension availability is layered in this order: built-in defaults, harness
configuration/drop-ins, selected profiles, and ordered `--harness-config` layers,
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
- `--harness-config KEY=VALUE` overrides are the highest-precedence generic
  harness config layers and must preserve command-line order.
- `aliases.providers` and `aliases.models` are keyed startup-only maps.
  `TAU_PROVIDER_ALIASES` and `TAU_MODEL_ALIASES` are JSON objects layered after
  file/profile/generic config, and repeatable `--provider-alias FROM=TO` /
  `--model-alias FROM=TO` operations apply last with later operations winning.
- Selected `profiles.<name>` patches load after built-in/user/drop-in files and
  before `--harness-config` layers. `--profile <name[,name...]>` wins over
  `TAU_PROFILE`; selected names apply left-to-right, including duplicates. ASCII
  spaces and tabs around names are ignored, while empty or whitespace-only
  segments and unknown names are explicit errors. When neither selects a name,
  top-level base-layer
  `default_profile` selects one exact fallback profile after trimming surrounding
  ASCII spaces/tabs and rejects comma-separated syntax. An absent or null
  `default_profile` sources no profile.
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
Authenticated profile settings carry one validated opaque credential identity;
the closed credential-slot mapping resolves it only under that extension's
Secret scope. The identity survives a profile namespace rename, so renaming
changes only the profile filename and never rewrites settings or moves
credentials.
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
inclusive positive whole-minute values, default to one and 1,440 respectively,
and reject an inverted range during configuration loading. The maximum cannot
exceed 65,535 minutes because the persisted wait registration represents its
effective timeout as `u16`. They do not affect argument-free or exact
background-result waits.
The raw config layer retains both integer keys so layering and field-specific
diagnostics see the authored values. Effective settings expose one validated
`WaitTimeoutBounds` policy rather than independent integers. Named minimum,
maximum, and duration accessors prevent runtime code from exchanging the bounds
or bypassing their positive/order/range invariants.

`agent_watch_retry_notification_threshold` suppresses model-visible
`retrying` notifications through the configured attempt, while later attempts
retain the existing once-per-sanitized-category delivery rule. It defaults to
five; zero preserves category-deduplicated delivery from the first retry.
`4294967295` suppresses every live retry notification, but current sanitized
snapshot state still updates and remains available to newly enabled watchers.
The setting does not suppress other provider-work phases or terminal failures.

`notification_delivery` defines four closed harness-owned runtime classes:
`user_prompt`, `status`, `agent_message`, and `external_message`. Each class
contains integer `idle_ms`, `wait_any_ms`, and `wait_tool_ms` delays satisfying
`idle_ms <= wait_any_ms <= wait_tool_ms`; equality is valid and invalid order or
monotonic-clock overflow fails configuration loading. Admission snapshots the
effective policy, so later configuration or state changes cannot reset a queued
deadline. The shipped defaults are respectively `0/0/5000`,
`120000/240000/240000`, `0/60000/120000`, and `0/0/30000` milliseconds.

`tau-config::provider_debug_capture` owns the dependency-neutral provider
capture basename contract shared by provider writers and harness retention:
canonical decimal microsecond timestamp, validated `AgentPromptId`, one valid
transport/direction class, and exact compressed `.json.zst` extension.

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

`tool_policy.default_shell_tool_style` selects the `codex` apply-patch, `edit`
line-coordinate, or `replace` exact-text implementation before rules and role
controls run. Both non-Codex implementations are provider-visible as `edit`;
their selector and internal tool names remain distinct. Missing, null, or
whitespace-only values reset to the selected model default: exact-text
`replace` except ChatGPT/Codex `apply_patch`. Any other nonblank value is a
config error.

## Harness role merging

Role metadata is merged through domain-specific logic rather than generic YAML
array replacement:

- Role sources retain their normal order (built-ins, files/drop-ins, selected
  profile stack, then ordered `--harness-config` layers) within each scope. After
  collecting them, `agents` defaults (`enable`, `visible`, `model`, `effort`, `verbosity`,
  `thinking_summary`, `service_tier`, `compaction`, `inference_compaction`, and
  named `compactions`) apply to every role,
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
  Alert patches retain raw integer thresholds until merge validation. Effective
  alerts carry a positive `ContextSizeAlertThreshold`, rather than a raw `u64`,
  and compare provider token usage through its named policy method.
- Named `compactions` likewise merge field-by-field. Absent fields inherit,
  `when: null` resets to `before_inference` with any status,
  `when.statuses: null` clears the restriction, and a nonempty status list
  replaces it. Empty status lists and new rules without a threshold are invalid.
  Legacy config `compaction` is a replace-all edit that normalizes into
  `inference_compaction` plus `compactions.default`. The legacy interactive CLI
  threshold command is intentionally different: it updates `default` while
  preserving named siblings.
- Patch fields distinguish absent, explicit `null`, and concrete values. `null`
  clears nullable/scalar fields; replacement lists can be cleared with `[]`.
- `tools` is a nullable replacement list: `tools: null` clears an inherited
  allow-list back to default behavior, while `tools: []` sets an explicit empty
  allow-list.

- `agents.web_tools` follows the same agent-default, role-group, and role
  inheritance. Logical search and fetch candidates are keyed maps whose
  same-named fields merge across layers; `(priority, name)` gives deterministic
  selection order. `allowed_domains: null` clears an inherited restriction while
  `[]` denies every web domain, and candidate `context_size: null` delegates to
  the provider default.
- Disabled roles are removed only after all file, drop-in, and CLI layers have
  been merged.
- After role merging, provider aliases rewrite only the provider component and
  model aliases rewrite only the exact complete model-name suffix of every
  effective configured `ModelId`. Resolution is recursive and case-sensitive;
  identity edges terminate, while any other cycle (including an unused cycle)
  fails startup. The resulting `HarnessSettings` contains only canonical model
  IDs and does not retain alias maps.
- `inter_session_receiver` and `inter_session_auto_start` are ordinary scalar
  role fields. Group defaults and role overrides can grant them across any
  number of groups. Auto-start without effective receiver capability is invalid.
- `visible` is ordinary inherited role metadata. Effective `false` suppresses
  only the built-in available-sub-task-role prompt catalog; it neither removes a
  role nor changes its authorization, diagnostics, UI lists, or other role
  surfaces.

## Selectable configuration profiles

`profiles` is a raw configuration-only map, not part of effective
`HarnessSettings`. A selected profile supports startup-only provider/model
aliases, the global `tau_state_access`
default, `agents.default_role`, agent provider defaults, agent/global role
metadata, role groups and roles, plus `extensions.<name>.enable` and arbitrary
`extensions.<name>.config` for a built-in or base-configured extension.
Extension config objects merge recursively; arrays, scalars, nested nulls, and
type mismatches replace lower precedence values, and no deletion sentinel
exists. A top-level extension `config: null` retains its existing absent/no-op
compatibility. Profile patches replay after base file layers in selected
left-to-right order, so `--profile xyz,foo` applies `xyz` then `foo` and
relative values resolve against the accumulated settings, before CLI role or
`--harness-config` patches.

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
Environment discovery matches the exact `TAU_SECRET_` prefix in the OS-native
key representation. Unrelated non-Unicode entries are ignored; a matching
suffix or value that cannot enter the UTF-8 named-secret schema causes a
redacted typed error. Harness capture still removes every matching raw key
before returning that error, while setup retains it.
`BuiltinComponentIdentity` separately preserves Tau-owned component authority
through argv wrapping without deriving authority from flattened executable
arguments.
