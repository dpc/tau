# Configuring extensions

## Tau-state access

Persistent supervised extensions receive the real Tau-state tree recursively
read-only by default. Use `hidden` to hide unrelated state, or `legacy` to
recover the historical ambient writable view:

```yaml
extensions:
  narrow-integration:
    tau_state_access: hidden
  legacy-integration:
    tau_state_access: legacy
```

`TAU_EXTENSION_TAU_STATE_ACCESS=hidden|read_only|legacy tau` overrides every
supervised extension in one new persistent daemon. A memory-only harness still
forces `hidden` afterward. The setting is intentionally rejected by `tau attach`
and never reaches extension child environments. Secrets stay masked in every
mode. Restricted modes restore only the extension's own durable state directory
read-write. Providers additionally receive their selected credential-free
settings read-only. Provider captures cross the
extension protocol as bounded opaque zstd blobs; the harness alone derives and
writes their session/instance paths.

A selected profile can set the global default for that daemon without using the
process-wide environment force:

```yaml
profiles:
  rostra-bot:
    tau_state_access: hidden
```

The profile value loads after base files and before `--harness-config`.
An explicit `extensions.<name>.tau_state_access` remains the per-instance
override, and `TAU_EXTENSION_TAU_STATE_ACCESS` remains the final process-wide
force.

The read-only default improves debugging and cooperative extension
introspection. It also means every configured supervised extension in a
persistent harness can read other Tau session and agent state unless `hidden`
is selected explicitly. Memory-only harnesses always force `hidden`, create no
host state, and mask an existing state root if one exists.

The recursive read-only mount operation requires Linux 5.12 or later.
Tau never weakens a restricted mount to accommodate an older kernel:
`mount_setattr` failure fails supervised extension startup closed.

## Runtime socket discovery

Tau presents an empty read-only view of the harness runtime socket directory to
every supervised component by default. Thus, `tau session list --json` inside a
supervised shell can correctly return `[]` even when host-side harness listeners
are live. Run discovery outside the supervised extension namespace. A trusted
component that genuinely must discover or connect to Tau harnesses can explicitly
restore the historical ambient view:

```yaml
extensions:
  trusted-coordinator:
    tau_runtime_socket_access: legacy
```

This opt-out affects runtime socket discovery only. It does not weaken state or
secret masking.

For a content-free mount-namespace diagnostic, see the
[debugging self-knowledge](../crates/tau-skills/self-knowledge/tau-self-knowledge-debugging.md#runtime-discovery-from-supervised-components).

## Rostra

The bundled `std-rostra` instance is disabled by default. It runs one full
Rostra client with relay-only Iroh peer transport, Pkarr HTTPS/DNS discovery,
and no direct peer-IP transport. It derives its identity from a Tau-managed
24-word mnemonic:

```yaml
extensions:
  std-rostra:
    enable: true
    require: false
    secrets:
      rostra_identity_mnemonic: {}
    config:
      identity_mnemonic_secret: rostra_identity_mnemonic
```

Set `TAU_SECRET_ROSTRA_IDENTITY_MNEMONIC` when starting Tau. The harness
consumes it before it spawns extensions; the generic secret resolver otherwise
uses `<tau-state>/secrets/rostra_identity_mnemonic.yaml`. The extension derives
the public identity instead of accepting a duplicate ID. It remains read-only
until the first signed call; activation can create a signed node announcement
and begins best-effort background Pkarr/head publication.

The read tools are `rostra_status`, `rostra_list_posts`, `rostra_read_post`,
and `rostra_get_profile`. The signed tools are `rostra_post`, `rostra_react`,
`rostra_follow`, `rostra_unfollow`, `rostra_update_profile`, and
`rostra_vote`. A signed-tool result confirms only durable local storage;
publication is asynchronous best effort. A timeout, cancellation, or crash can
leave a possibly stored/published event, so retrying can create a duplicate.
Timeline reads cover direct following, the locally known two-hop network, and
one explicit author. They never perform on-demand synchronization, and empty
or missing results mean only that data is absent from the synchronized local
database.

`std-rostra` stores graph state, projections, synchronization metadata, and its
Iroh node secret in the stable per-instance `rostra.redb`. The store survives
Tau sessions and is not part of session journals. The extension fails in
memory-only mode. Changing the derived identity for an existing state directory
fails closed; use another extension instance or move the old directory with a
new instance name so publisher-scoped notification IDs cannot repeat. Tau
never accepts a public Rostra ID alongside the mnemonic, creates an identity,
enables direct-IP public mode, or turns synchronized posts into arbitrary
inbound messages. The mnemonic grants permanent signing authority to every role allowed
the signed tools; Tau has no human per-call confirmation mechanism.

`rostra_notifications` is agent-scoped and accepts only `{"enabled": boolean}`.
It starts at the materialization-feed tip, selects matching direct-followee
posts only, and uses the bounded durable materialization feed after lossy
broadcast hints. It suppresses self posts and historical syncs using the
database initialization and current follow-epoch timestamps. It becomes
eligible after 30 seconds of quiet or five minutes of batch age, then limits
each agent's canonical Rostra reports and normal wakes to one every five
minutes. That does not rate-limit model runs; normal busy-agent batching can
coalesce or delay them. Reports are count-only, lossy wakes: their body is
`Rostra received 1 new followed post.` or `Rostra received {count} new followed
posts.` The generic authenticated external-message envelope is the sole
provenance/trust wrapper. A wake contains no post body, ID, author, tags, or
timestamp; inspect the current following timeline with
`rostra_list_posts({"timeline":"following"})` and `rostra_read_post`. That
cannot recover the exact announced batch.


## Tau Swarm

The bundled `std-swarm` instance is disabled by default and optional
(`require: false`). See the [authoritative configuration, bounds, retry, and
process-memory semantics](../crates/tau-ext-swarm/README.md). The extension
registers the agent-scoped `task_info`, `task_blocker`, and `task_update` tools, but none
is model-visible by default even after the extension starts. Opt in deliberately
for selected roles:

```yaml
agents:
  role_groups:
    engineer:
      enable_tool_groups: [swarm]
```

Use `enable_tools: [task_info]`, `[task_blocker]`, or `[task_update]` for one
exact tool instead. The old names do not remain as aliases. If the extension
uses `tool_prefix: work`, use `work_swarm`, `work_task_info`,
`work_task_blocker`, or `work_task_update` in role policy.




Tau extensions are separate processes configured under `extensions` in
`harness.yaml`. Run `tau init` to generate the normal configuration files.

An extension entry can:

- use `enable: false` to stay completely disabled;
- use `require: false` to let startup continue, with a visible notice, when an
  enabled extension cannot initialize;
- set `startup_timeout_seconds` from 1 through 3,600 to bound its own
  successful-spawn-to-`Ready` interval; ordinary extensions default to two
  seconds, while bundled `std-rostra` defaults to ten seconds for local
  database upgrade and compaction work;
- set its command argv with `command`, wrap it with an argv `prefix`, or set its
  process working directory with `cwd`;
- pass extension-specific data through `config`;
- declare the Tau-managed secrets that the harness may inject; and
- set `tool_prefix` to distinguish tools from multiple configured instances.

The built-in provider extension stores authenticated-profile credentials in its
private Secret scope. Supported local-compatible settings may instead contain
an explicit keyless marker and have no Secret record. See
[providers](providers.md#scoped-provider-credentials) for rotation and restart
semantics.

For example:

```yaml
extensions:
  core-shell:
    prefix: ["ssh", "workstation"]
  std-slack:
    enable: true
    require: false
    tool_prefix: work
    config:
      prefix_agent_id: false
  custom-tool:
    command: ["/usr/local/bin/custom-tau-extension"]
    cwd: /srv/project
    config:
      mode: concise
```

`prefix` wraps the process argv; it is useful for `ssh`, `docker exec`,
`bwrap`, and similar launchers. Setting an explicit `command` on a built-in
extension clears its inherited `suffix`; supply `suffix` explicitly too when
the replacement command still needs those trailing component arguments.

`tool_prefix` is unrelated: a prefix of `work` exposes a logical tool such as
`slack_send` as `work_slack_send`. It applies to tool names, aliases, and groups,
but not to actions, tags, schemas, or prose. Changing it requires restarting the
extension. Exact role policy uses the final prefixed names.

Extension-specific settings remain under `config`. For example, std-slack's
`prefix_agent_id` defaults to `false`; set it to `true` only when Slack posts
should retain the legacy `[agent-id] ` presentation.

`std-utils` keeps its best-effort `papercut` reporter disabled unless its
instance config sets `papercut.enable: true`. See the
[std-utils README](../crates/tau-ext-utils/README.md) for its exact JSONL
record, per-instance User-storage location, limits, privacy, retention, and
inspection contract. `tau dev papercut list [--markdown]` inspects the normal
instance's records; `tau dev papercut clear` clears its locked snapshot.
The model-visible tool is conditional: agents use it only for an incidental
Tau harness, tooling, environment, confusing, or suspicious problem, never
merely to state that no problem occurred.

Core-shell protects non-interactive model and user shell commands from pager
prompts by applying `PAGER=cat`, `GIT_PAGER=cat`, `GH_PAGER=cat`,
`JJ_PAGER=cat`, and `SYSTEMD_PAGER=cat` after inherited variables and
`config.shell.extra_env`. It preserves `TERM`. To intentionally run a configured
pager, set `config.shell.non_interactive_pager: false`; this explicit opt-out
forfeits Tau's protection for these pagers:

```yaml
extensions:
  core-shell:
    config:
      shell:
        non_interactive_pager: false
        extra_env:
          PAGER: less
```

The protected `cat` command must resolve through the child's effective `PATH`;
an environment without `cat` fails normally with a shell “not found” error.
`MANPAGER`, `BAT_PAGER`, and other application-specific pager variables remain
ordinary configuration.

Core-shell can also apply an optional best-effort command guardrail:

```yaml
extensions:
  core-shell:
    config:
      shell:
        allowlist:
          - workdir: /srv/project/**
            command: cargo *
          - workdir: /srv/project
            command: jj status
          - workdir: /srv/project/**
            command_regex: 'jj (?:log|show [a-z]{6,32})'
```

Omitting `allowlist` preserves unrestricted execution; `allowlist: []` denies
every covered command. Each rule requires `workdir` plus exactly one command
matcher: existing `command` is a glob, while `command_regex` is a Rust regular
expression. One rule must match both the canonical absolute effective workdir and
the raw submitted shell-language command. Both matcher kinds are whole-string and
case-sensitive. Regexes use absolute implicit anchors, so authors should not add
`^...$`; matching includes newlines in the submitted command. Use a YAML
single-quoted scalar for regexes containing backslashes. Inline case-insensitive
regex flags are rejected.

Workdir `*` stays within one path component while `**` crosses components.
Command globs retain globset grammar and treat separators and newlines as ordinary
characters. Regular expressions use Rust's non-backtracking `regex` engine, which
rejects look-around and backreferences. Each allowlist has at most 32 rules, each
workdir or command pattern has at most 2,048 authored UTF-8 bytes, and each
compiled glob or regex matcher has a 262,144-byte bound. Configuration rejects
invalid patterns, both/neither matcher fields, and each limit with a stable error.

The same rules cover model `shell`, ChatGPT-facing `shell_command`, and user
`!`/`!!` before VCR replay or process spawn. A denial shows the configured
typed command matcher and workdir pair so the agent can choose a permitted
invocation. Do not place secrets in a glob or regex: denials deliberately disclose
the configured patterns. Matching
does not inspect shell syntax, the configured shell or wrapper prefix,
environment, `PATH`, or resolved executables; fixed internal subprocesses such
as `grep`'s `rg` are excluded. This is a best-effort guardrail, not a sandbox or
security boundary.

When `allowlist` is present, core-shell also adds its effective typed
`command_glob`/`command_regex` and `workdir` selector pairs to the model's
system prompt. The prompt sorts and de-duplicates equivalent presentation
entries, but enforcement still evaluates the authored rules as configured.
Both selectors in one displayed pair must match. An explicit empty list renders
`none (all shell commands are denied)`; omission leaves the existing shell
workdir guidance unchanged. Selector strings use JSON escaping, including
`\u007b` and `\u007d` for braces, so patterns remain literal prompt content.

Configured extension processes are trusted local executables with the user's OS
authority. Tau limits their protocol authority and injects only declared
Tau-managed secrets, but it does not make them an operating-system sandbox. See
[SECURITY.md](../SECURITY.md) before enabling or replacing an extension.

## Configuration layers

Built-in defaults load first, followed by `harness.yaml`, lexically sorted
`harness.d/*.yaml` or `*.yml` drop-ins, selected profiles, and ordered
command-line overrides. `--profile NAME[,NAME...]` wins over
`TAU_PROFILE=NAME[,NAME...]`; when neither selects a name, top-level
`default_profile: NAME` selects one fallback profile. Omit `default_profile`,
or set it to `null`, to load only base layers. Named profiles do not inherit
the fallback profile. Its surrounding ASCII spaces/tabs are ignored, but it
cannot use comma-separated syntax. A comma-separated selection patches the accumulated configuration
left-to-right, so `--profile xyz,foo` applies `xyz` then `foo`; repeated names
also apply repeatedly. ASCII spaces and tabs around names are ignored, while
empty or whitespace-only segments and unknown names fail startup. A profile can
change the global `tau_state_access` default plus `enable` and arbitrary `config` for a
base-configured or built-in extension, and CLI overrides still win:

```yaml
default_profile: focused

profiles:
  focused:
    extensions:
      std-pim:
        enable: false
      core-shell:
        config:
          shell:
            allowlist:
              - workdir: /srv/project/**
                command: cargo *
```

Extension config maps merge recursively across layers. Arrays, scalars, nested
nulls, and type mismatches replace lower-precedence values; there is no deletion
sentinel. A top-level extension `config: null` keeps its historical absent/no-op
meaning.

Repeat `--harness-config=KEY=VALUE` to make a one-launch override:

```console
$ tau --harness-config=extensions.core-shell.cwd=/srv/project
```

Values are parsed as YAML. Quote a value when a string could otherwise be
interpreted as a boolean, number, `null`, list, or map. These startup settings
cannot be applied when merely attaching to an existing harness.

Tau validates and normalizes every startup layer before launching configured
processes. A missing optional user file keeps the built-in layer, but an existing
unreadable or malformed file, conflicting alias, invalid target, or malformed
override stops startup with its source context. One accepted snapshot supplies
extension commands, provider and secret inputs, roles, tool policy, retention,
and prompt settings for that process; Tau does not reread individual settings or
silently replace them with built-ins while the harness runs.

Service and container launchers can additively enable configured extensions
with a comma-separated, case-sensitive list:

```console
$ TAU_ENABLE_EXTENSIONS=std-pim,std-xmpp tau
```

The variable accepts names only, not arguments or credentials. Ordered CLI
extension overrides are applied after it, so an explicit CLI disable still
wins. Use one joined environment value rather than repeated keys.

## Extension logs

Each supervised extension writes stderr to
`~/.local/state/tau/sessions/<session-id>/logs/<instance>.log`. When `TAU_LOG`
is absent or invalid, Tau's built-in component processes enable `info` for
their own first-party target and use `warn` as the global fallback. Custom
extension processes own their subscriber and fallback policy. The built-in
policy keeps low-frequency configuration, connection, and durable-completion
records visible without enabling dependency `info` logs.

An explicit valid `TAU_LOG` is a complete replacement and is inherited unchanged:

```console
TAU_LOG=warn tau
TAU_LOG='xmpp=debug,warn' tau
```

Debug and trace records can contain private identifiers, queries, paths, or
script-authored text. Extension stderr is stored unredacted and follows
whole-session retention; it is not covered by the shorter debug-diagnostic
cleanup. Dependency warnings can also contain public identifiers or filesystem
paths, so treat the entire file as private. `tau attach` starts only a new UI,
so its environment cannot change
the filter of an already-running harness or extension. Restart or resume the
harness with the desired filter instead.

`tau serve --mirror-extension-stderr` additionally copies child stderr to the
serve process's inherited stderr as escaped, bounded, generation- and
PID-attributed records. It is default-off, best-effort, and never replaces or
changes the private file. A full internal queue drops mirror records and later
reports bounded record/raw-byte counts; a stderr sink failure disables the
process-wide mirror while raw files continue. Attach/close file markers,
extension stdout and protocol, events, journals, debug JSONL, provider captures,
and Configure payloads are never mirrored.

The worker uses an independent duplicate of inherited stderr when available.
Any mirror setup failure, including descriptor duplication or worker-thread
creation, disables only the mirror. Mirror traffic still shares the underlying
fd-2 sink capacity, and ordinary harness tracing keeps its existing synchronous
behavior.

Each mirror record has exactly this single-line shape and one trailing LF:

```text
tau: extension stderr: extension=<validated-name> generation=<u32> pid=<u32> boundary=<line|chunk|eof|dropped> message="<escaped-message>"
```

Framing splits only on byte LF and omits that LF from `message`. An exact
4096-byte payload followed by LF is one `line`. Longer logical lines emit one or
more `chunk` records and then a final `line`; an unterminated suffix ends with
`eof`, including an empty `eof` after a final full chunk. Ordinary fragments
contain at most 4096 raw payload bytes plus at most three lookahead bytes needed
to avoid splitting valid UTF-8. The canonical escaping is `\\`, `\"`, `\t`,
and `\r`; invalid bytes, C0/C1 controls, DEL, and ESC use uppercase `\xHH`;
U+2028, U+2029, U+061C, U+200E/U+200F, U+202A–U+202E, and U+2066–U+2069 use
uppercase-codepoint `\u{...}`. Other printable valid Unicode is preserved.

Queue saturation drops mirror records only. Each affected child logger retains
saturating dropped-record and original raw-byte counts. When admission resumes,
one `boundary=dropped` record with
`message="records=<count> raw_bytes=<count>"` precedes its later content.
Ordering is preserved only within one `(extension,generation)` logger;
cross-extension and cross-generation order is scheduler-dependent. Generation
and PID distinguish an old same-name child whose stderr remains open while its
replacement runs. File attach/close markers never enter the mirror.

This option does not apply `TAU_LOG` again: the child remains the sole producer
filter. Arbitrary custom extension stderr is mirrored once and is **unredacted**.
Journal readers are commonly a wider audience than readers of Tau's private
state, so enable it only when that audience may see the complete custom stderr
stream. Journald may independently suppress records.

## Failure and naming behavior

`enable` answers whether an extension should run. For an enabled extension,
`require` answers whether its initialization is startup-critical. Optional
extensions that fail to initialize are skipped or disabled and reported with a
mandatory notice; this is availability policy, not sandboxing.

Extension names must be nonempty and contain only ASCII letters, digits, `_`,
and `-`. Final internal tool names are unique across live connections. Duplicate
model-visible aliases are allowed only when policy keeps them out of the same
effective prompt; a `tool_prefix` is the normal way to avoid both kinds of
collision for multiple tool-producing instances.

Extension-specific `config` and secret requirements belong to each component's
README. Current configuration layering is recorded in
[ARCH-tau-config](../crates/tau-config/specs/ARCH-tau-config.md); lifecycle,
optional-startup, and protocol boundaries are specified in
[SPEC-tau-harness-extension-lifecycle](../crates/tau-harness/specs/SPEC-tau-harness-extension-lifecycle.md).

Model-facing generic user-payload framing follows [SPEC-exact-sentinel-prompt-envelopes](../specs/SPEC-exact-sentinel-prompt-envelopes.md); payload-local XML-like tags do not establish Tau provenance.
