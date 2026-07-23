# DECISION-colon-command-mode: Colon command mode

Authority: confirmed, 2026-07-23, dpc

Tau uses a first-non-whitespace `:` to enter command mode. This mode offers
only built-in CLI commands, harness-owned prompt commands, and live extension
action roots while completing a command name. The harness-owned forms include
`:skill <name> [args]` and compact `:skill:<name> [args]`. After Tau recognizes
a command token, only that command's registered argument or subcommand
completer may contribute candidates. This intrinsic routing takes precedence
over user completion configuration.

A first-non-whitespace `/` has no command meaning and follows ordinary prompt
and configured path-completion behavior. Tau replaces every built-in command,
harness-owned skill command, and extension action root with colon spelling; it
does not retain slash aliases or migrate history. Shell shortcuts `!` and `!!`
remain unchanged.

A doubled command prefix escapes literal prompt text: `::text` is previewed,
accepted, recorded, routed, and submitted canonically as `:text`. Tau never
stores or submits the escape prefix. A single leading `:` always enters command
mode, including after leading whitespace and for input produced by the external
editor. Unknown or malformed commands fail locally rather than becoming model
prompts; headless send follows the same rule.

The `ActionSchema` root grammar changes from slash to colon as a
harness-extension interface break. Serialized `action.*` DTO shapes remain
unchanged, and both `ActionSchema.version` and the global protocol version
remain `0`. Tau provides no aliases, translation layer, or migration, consistent with
[DECISION-no-backward-compatibility](DECISION-no-backward-compatibility.md).
Generic `complete_actions` extension mechanics remain available, but configured
completion roots cannot shadow intrinsic command-mode routing.

## Rationale

The user directed: “BTW. We should have the actions start with `:` instead of
`/`. Just like in Vim, Helix, tmux and many other Unix software. `/` also
collides with path autocompletion. Leading `:` should mean command mode, where
we call commands (actions), and the auto-completion for commands only.” The user
subsequently approved the `::text` escape through authenticated XMPP.

Colon follows established command-mode conventions and frees slash for absolute
and token-level path completion. A single grammar keeps routing, completion,
extension publication, history, and provider input coherent; compatibility
aliases would preserve the collision and make command ownership ambiguous.

This interface decision follows the prior-approval requirement in
[DECISION-persistence-and-extension-interface-change-approval](DECISION-persistence-and-extension-interface-change-approval.md).
The distributed behavior is specified by
[SPEC-tau-cli-command-mode](../crates/tau-cli/specs/SPEC-tau-cli-command-mode.md),
and the extension interface is described by
[ARCH-tau-actions](../crates/tau-actions/specs/ARCH-tau-actions.md).
