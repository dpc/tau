# SPEC-tau-cli-command-mode: Command mode

## Record justification

Command-mode behavior spans input parsing and completion, local CLI state,
protocol request construction, harness-owned execution, and renderer feedback,
so no single owning module can state command ownership and cross-boundary
behavior coherently.

The terminal input loop has multiple command owners. CLI-owned commands
such as `:quit`, `:session`, `:agent`, `:name`, `:role`, `:model`, `:set`, and
`:theme` are handled locally. Dynamic extension actions are parsed against the
current published action schema and dispatched as `ActionInvoke` events.
Harness-owned prompt commands, currently `:skill <name> ...` and
`:skill:<name> ...`, are completed and echoed by the CLI but must still be
submitted as prompts so the harness can resolve skills and inject their content.

The CLI-owned `:pick-agent` and `:pick-agent-all` commands invoke the same
active-agent and all-live-agent picker paths as the configurable `agent-pick`
and `agent-pick-all` binding actions. `agent-pick` remains bound to C-b by
default; the all-agent action has no built-in key binding.

`:retry` is a static CLI-owned command whose work is harness-routed. The CLI
captures the current session and selected agent; it never resubmits prompt text.
The harness resolves the exact in-flight prompt and directly addresses its
provider owner, then directs the correlated typed result only to the invoking UI.
Provider-side transfer behavior is specified by
[SPEC-tau-ext-provider-builtin-retry-scheduler](../../tau-ext-provider-builtin/specs/SPEC-tau-ext-provider-builtin-retry-scheduler.md).

The CLI has one narrow action-specific redaction exception: `:email auth google finish ...` is redacted
in command echo and persistent prompt history because its pasted loopback URL
contains a one-time OAuth authorization code. The raw `ActionInvoke` still goes
to the owning extension so the action can complete.

`:model <provider>/<model>` has two CLI-owned paths: with a selected agent it
emits a targeted `ui.agent_model_select`; after `:new`, with no selected agent,
it stages a one-shot `ui.create_agent.model_override` for the next prompt-created
agent instead of sending an untargeted agent update.

Agent switch commands distinguish known transcript selection from effective
prompt routing. `:agent switch` completions list effectively active agents and
`none`; mentions, cycling, and suspend completion use the same effective set.
Resume completion lists loaded ineffective agents, including idle
`active-auto` agents. Explicitly typing a known hidden id still selects it.
Every `:agent` argument that refers to an agent accepts either canonical
`agent-id` text or the user-facing `@agent-id` spelling. Parsing removes exactly
one optional `@` before lookup and emits only the canonical id in protocol
events. A bare `@`, repeated prefix, or otherwise malformed id is rejected.
The strict durable/reference parsing boundary is recorded in
[ARCH-tau-proto](../../tau-proto/specs/ARCH-tau-proto.md).
Completion matches both input spellings but inserts the existing canonical bare
id and does not duplicate candidates; after an `@`, the special switch target
`none` is not offered as though it were an agent reference.
Accepted switch/picker/selection input preserves the mode. An accepted visible
human prompt to the selected existing target makes that target `active`.
`:agent resume` or `:resume` requests unconditional `active`, `:agent suspend`
requests `suspended`, and `:agent auto` requests `active-auto`.

`:name <display name>` is the selected-agent shortcut for `:agent name
<agent_id> <display name>`. It emits the same display-name update as `:agent
name` after resolving the currently selected agent, matching current-agent
shortcuts such as `:suspend` and `:resume`.

For interactive input, the first non-whitespace `:` intrinsically selects command
mode and cannot be shadowed by configured prompt completion rules. Only after all
command owners decline a line may the CLI report its leading colon token as an
unknown command. A first-non-whitespace `/` is ordinary prompt text and
participates in configured filesystem completion, including absolute paths and path
tokens later in a line.

`::text` escapes command mode and is canonically submitted as literal prompt
text `:text`. Tau removes exactly one colon before in-process and persistent
history, external-editor context, routing, durable prompt events, skill
processing, or provider projection; the escape prefix is never stored or sent.
The CLI carries typed literal provenance beside that canonical text in its UI
request so downstream command processors bypass it rather than interpreting the
canonical leading colon again. The harness preserves the canonical text and
does not copy the provenance marker into the provider prompt body.

`--prompt-stdin` does not use the interactive colon grammar. It preserves its
complete stdin body as the initial prompt and carries the same literal provenance,
so neither CLI command/action dispatch nor harness skill expansion interprets
colon-prefixed stdin. For example, stdin `:skill` reaches the initial agent prompt
as `:skill`.

Headless `tau dev send` shares the colon grammar, literal escape, shell
shortcuts, and control-event mappings. Interactive-only commands are explicit
no-ops there, `:skill` remains harness-forwarded, and unknown or malformed colon
commands fail locally instead of becoming model prompts. Slash-prefixed text is
submitted as an ordinary prompt in both clients.

`:tree` argument parsing is CLI-owned, while anchor resolution is harness-owned.
Bare `:tree` sends the dedicated `ui_tree_request` message and renders the
harness's exactly one requester-directed multiline notice. `tau dev send`
waits for and prints that same notice before exiting. Tree navigation forms
remain `ui.navigate_tree` events because they mutate the selected agent head.
The CLI maps `:tree <positive-integer>` to a one-based prompt anchor target,
`:tree 0` and `:tree root` to the explicit root/before-first target, and
`:tree node <non-negative-integer>` to the raw-node expert target. It must not
send bare numeric arguments as raw transcript node ids; the harness resolves
prompt anchors against the selected agent's durable prompt provenance.
Anchor behavior is specified by
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md).

## Shared navigation mutations

`:agent suspend`, `:agent resume`, and `:agent auto` request the absolute
harness-owned modes `suspended`, `active`, and `active-auto`. The UI never
optimistically mutates its cache; complete stats snapshots update it. Selection,
transcript view, drafts, and presentation remain local.
