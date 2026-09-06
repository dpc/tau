# SPEC-tau-cli-command-mode: Command mode

## Record justification

Command-mode behavior spans input parsing and completion, local CLI state,
protocol request construction, harness-owned execution, and renderer feedback,
so no single owning module can state command ownership and cross-boundary
behavior coherently.

The terminal input loop has multiple command owners. CLI-owned commands
such as `:quit`, `:quit-session`, `:session`, `:agent`, `:name`, `:role`,
`:model`, `:set`, and `:theme` are handled locally. `:quit` (alias `:q`) releases
the invoking UI through a harness-authoritative quit handshake. A daemon launched
with an immediate UI automatically shuts down when its last participating UI
leaves, including unexpected disconnection. `:detach` first disables that policy
for the entire daemon incarnation and only then disconnects the UI. Reattachment
never rearms it; headless launches start with it disabled. This is a last-UI rule,
not special lifetime authority attached to the creating UI.
`:quit-session` sends the unconditional dedicated shutdown request, regardless of
policy or other UIs, causing canonical session shutdown.

Quit decisions serialize at the harness: an acknowledged quitter no longer
participates in the last-UI count even while its transport drains. Explicit
detach cannot undo already-selected shutdown. Following terminal cleanup, normal
exit prints exactly one stderr line, `Session detached` after an authoritative
survival decision or `Session terminated` after confirming the original daemon
has exited. A request write, shutdown fact, or socket EOF alone is not termination
confirmation. Failed or unconfirmed termination produces an explicit diagnostic
instead, never a successful termination line. Process exit does not certify
successful persistence cleanup.
If explicit detach receives no acknowledgment while its connection remains live,
the input loop stays connected and reports that detach was not confirmed. It
must not turn an unconfirmed policy clear into automatic shutdown by closing
the transport. Process-killed UIs cannot print a final status; their EOF still
follows harness policy.
Replies echo a request correlation, so a late acknowledgment of a timed-out
detach cannot decide a subsequent quit.

Dynamic extension actions are parsed against the
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

The CLI has one narrow action-specific redaction exception: after submission,
`:email auth google finish ...` is represented as exactly `:email auth google
finish <redacted>` in command echo, in-process navigation and search history,
persistent prompt history, external-editor context, and content-enabled prompt
draft events. The pasted loopback URL contains a one-time OAuth authorization
code. The active editor and immediate parse/routing stack retain the raw line,
and the single successful raw `ActionInvoke` still goes to the exact owning
extension so the action can complete.

`:model <provider>/<model>` has two CLI-owned paths: with a selected agent it
emits a targeted `ui.agent_model_select`; with no selected agent, whether in
overview or the explicit composer, it stages a one-shot
`ui.create_agent.model_override` for the next prompt-created agent instead of
sending an untargeted agent update. Bare `:new` enters the composer, clears only
a stale staged role, and preserves staged model and ephemeral options.

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
The Gmail OAuth finish redaction exception applies after this canonicalization:
an escaped sensitive action becomes the fixed redacted literal rather than
exposing its code and state as a model prompt.
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
waits for and prints that same notice before exiting, without switching formats
based on whether stdout is a terminal. The harness applies the exact preview
presentation contract in
[SPEC-tau-harness-session-state](../../tau-harness/specs/SPEC-tau-harness-session-state.md)
before either client receives the notice. Tree navigation forms remain
`ui.navigate_tree` events because they mutate the selected agent head.
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
