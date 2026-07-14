# Configuring extensions

Tau extensions are separate processes configured under `extensions` in
`harness.yaml`. Run `tau init` to generate the normal configuration files.

An extension entry can:

- use `enable: false` to stay completely disabled;
- use `require: false` to let startup continue, with a visible notice, when an
  enabled extension cannot initialize;
- set its command argv with `command`, wrap it with an argv `prefix`, or set its
  process working directory with `cwd`;
- pass extension-specific data through `config`;
- declare the Tau-managed secrets that the harness may inject; and
- set `tool_prefix` to distinguish tools from multiple configured instances.

For example:

```yaml
extensions:
  core-shell:
    prefix: ["ssh", "workstation"]
  std-slack:
    enable: true
    require: false
    tool_prefix: work
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

Configured extension processes are trusted local executables with the user's OS
authority. Tau limits their protocol authority and injects only declared
Tau-managed secrets, but it does not make them an operating-system sandbox. See
[SECURITY.md](../SECURITY.md) before enabling or replacing an extension.

## Configuration layers

Built-in defaults load first, followed by `harness.yaml`, lexically sorted
`harness.d/*.yaml` or `*.yml` drop-ins, and ordered command-line overrides.

Repeat `--harness-config=KEY=VALUE` to make a one-launch override:

```console
$ tau --harness-config=extensions.core-shell.cwd=/srv/project
```

Values are parsed as YAML. Quote a value when a string could otherwise be
interpreted as a boolean, number, `null`, list, or map. These startup settings
cannot be applied when merely attaching to an existing harness.

Service and container launchers can additively enable configured extensions
with a comma-separated, case-sensitive list:

```console
$ TAU_ENABLE_EXTENSIONS=std-pim,std-xmpp tau
```

The variable accepts names only, not arguments or credentials. Ordered CLI
extension overrides are applied after it, so an explicit CLI disable still
wins. Use one joined environment value rather than repeated keys.

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
