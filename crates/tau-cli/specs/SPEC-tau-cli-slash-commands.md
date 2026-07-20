# SPEC-tau-cli-slash-commands: Slash commands

The terminal input loop has multiple slash-command owners. CLI-owned commands
such as `/quit`, `/session`, `/agent`, `/name`, `/role`, `/model`, `/set`, and
`/theme` are handled locally. Dynamic extension actions are parsed against the
current published action schema and dispatched as `ActionInvoke` events.
Harness-owned prompt commands, currently `/skill <name> ...` and
`/skill:<name> ...`, are completed and echoed by the CLI but must still be
submitted as prompts so the harness can resolve skills and inject their content.

`/retry` is a static CLI-owned command whose work is harness-routed. The CLI
captures the current session and selected agent; it never resubmits prompt text.
The harness resolves the exact in-flight prompt and directly addresses its
provider owner, then directs the correlated typed result only to the invoking UI.
Provider-side transfer behavior is specified by
[SPEC-tau-ext-provider-builtin-retry-scheduler](../../tau-ext-provider-builtin/specs/SPEC-tau-ext-provider-builtin-retry-scheduler.md).

The CLI has one narrow action-specific redaction exception: `/email auth google finish ...` is redacted
in command echo and persistent prompt history because its pasted loopback URL
contains a one-time OAuth authorization code. The raw `ActionInvoke` still goes
to the owning extension so the action can complete.

`/model <provider>/<model>` has two CLI-owned paths: with a selected agent it
emits a targeted `ui.agent_model_select`; after `/new`, with no selected agent,
it stages a one-shot `ui.create_agent.model_override` for the next prompt-created
agent instead of sending an untargeted agent update.

Agent switch commands distinguish known transcript selection from effective
prompt routing. `/agent switch` completions list effectively active agents and
`none`; mentions, cycling, and suspend completion use the same effective set.
Resume completion lists loaded ineffective agents, including idle
`active-auto` agents. Explicitly typing a known hidden id still selects it.
Every `/agent` argument that refers to an agent accepts either canonical
`agent-id` text or the user-facing `@agent-id` spelling. Parsing removes exactly
one optional `@` before lookup and emits only the canonical id in protocol
events. A bare `@`, repeated prefix, or otherwise malformed id is rejected.
The strict durable/reference parsing boundary is recorded in
[ARCH-tau-proto](../../tau-proto/specs/ARCH-tau-proto.md).
Completion matches both input spellings but inserts the existing canonical bare
id and does not duplicate candidates; after an `@`, the special switch target
`none` is not offered as though it were an agent reference.
Accepted input preserves the mode. `/agent resume` or `/resume` requests
unconditional `active`, `/agent suspend` requests `suspended`, and `/agent auto`
requests `active-auto`.

`/name <display name>` is the selected-agent shortcut for `/agent name
<agent_id> <display name>`. It emits the same display-name update as `/agent
name` after resolving the currently selected agent, matching current-agent
shortcuts such as `/suspend` and `/resume`.

Only after those owners decline a line may the CLI treat an unrecognized leading
slash token as an unknown-action notice. That fallback is intentionally limited
to leading slash roots; ordinary prompt text that contains slashes later in the
line remains normal prompt text.

`/tree` argument parsing is CLI-owned, while anchor resolution is harness-owned.
The CLI maps `/tree <positive-integer>` to a one-based prompt anchor target,
`/tree 0` and `/tree root` to the explicit root/before-first target, and
`/tree node <non-negative-integer>` to the raw-node expert target. It must not
send bare numeric arguments as raw transcript node ids; the harness resolves
prompt anchors against the selected agent's durable prompt provenance.
Anchor behavior is specified by
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md).

## Shared navigation mutations

`/agent suspend`, `/agent resume`, and `/agent auto` request the absolute
harness-owned modes `suspended`, `active`, and `active-auto`. The UI never
optimistically mutates its cache; complete stats snapshots update it. Selection,
transcript view, drafts, and presentation remain local.
