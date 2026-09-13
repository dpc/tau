# Session lifecycle and upgrades

Start with one session, one agent, and one bounded task. Delegation adds another
independent transcript, model/tool policy, lifecycle, and coordination cost; use
it after the [first-task guide](getting-started.md) is familiar.

## Everyday lifecycle

```text
tau                         create a new durable session and initial agent
tau attach [SESSION]        connect to a live daemon
tau resume [SESSION]        start a daemon from persisted session state
tau session list            list responsive live daemons
```

Inside the terminal UI:

```text
:quit or :q                 leave; normally stop a foreground-owned session
:detach                     leave and deliberately keep its daemon running
:quit-session               request shutdown of the session daemon
```

Plain `tau` and interactive `tau resume` normally stop when the last UI
disconnects. Once a UI uses `:detach`, that daemon stays alive across later
ordinary UI quits until it is explicitly stopped. An attached UI does not take
ownership of the daemon. See [Session startup](session-startup.md) for locking,
pickers, multiple UIs, and supervised `tau serve` operation.

Tau starts in the verbose diagnostic transcript. Press <kbd>Ctrl-V</kbd> or use
`:verbose-mode-toggle` to switch between verbose and compact views. The toggle
changes only that UI's presentation.

## Upgrade and recovery checklist

Running daemons do not adopt a newly installed executable or reread startup
configuration. Use this conservative sequence:

1. Let important turns and tools finish.
2. Record the session IDs and `tau --version`.
3. Use `:quit-session` for every daemon being upgraded.
4. Preserve the current executable in a private backup location, or record the
   exact source revision/install reference needed to reproduce it.
5. Back up Tau's config and state roots while Tau is stopped.
6. Replace the executable using the same install route.
7. Confirm the new `tau --version`.
8. Resume one session first and inspect it before resuming the rest.

Default Linux roots are `${XDG_CONFIG_HOME:-$HOME/.config}/tau/` and
`${XDG_STATE_HOME:-$HOME/.local/state}/tau/`. Preserve both: configuration,
credentials, session membership, agent transcripts, and diagnostics have
different owners and recovery roles.

Tau is under heavy development and does not promise backward compatibility.
Keep the previous executable and backup until the new version has opened the
state you need. On failure, preserve the exact error. Do not delete state,
truncate journals, rerun `tau init --force`, or repeatedly retry provider turns
as generic repair steps.

## Minimal troubleshooting map

- **Install/startup:** verify `tau --version`, then read the first exact startup
  error.
- **Provider/auth/model:** run `tau provider list`; use its displayed
  `tau provider login <profile>` repair when applicable; restart after provider
  settings change.
- **Tool execution:** retain the tool name, safe error, project root, and focused
  reproduction. Confirm the configured extension actually started.
- **Attach/resume:** run `tau session list` on the host. Attach requires a live
  daemon; resume requires valid persisted state.
- **Restricted startup:** the recursive read-only Tau-state mount requires Linux
  5.12 or later and fails closed when unavailable.

The [first-task guide](getting-started.md#if-a-step-fails) gives the newcomer
decision tree. Tau's
[debugging self-help](../crates/tau-skills/self-knowledge/tau-self-knowledge-debugging.md)
documents owner-private paths and deeper diagnostics.
